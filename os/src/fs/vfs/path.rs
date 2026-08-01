use super::super::mount::{
    MountNamespaceId, mount_supports_dentry_cache, mounted_root_for, mounted_root_for_static_child,
    mounted_root_parent, namespace_has_dynamic_mounts, primary_mount_id, root_ino_for, with_mount,
};
use super::super::path::{PathContext, WorkingDir, normalize_path_at_root};
use super::super::{dentry_cache, dentry_cache::DentryLookupResult, inode_state};
use super::{BackendOp, FsError, FsNodeKind, FsResult, VfsNodeId};
use crate::perf;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

const EXT4_NAME_MAX: usize = 255;
const SYMLINK_TARGET_MAX: usize = 4096;
const MAX_SYMLINK_FOLLOWS: usize = 40; // Linux returns ELOOP after 40 symlink resolutions.

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct VfsPath {
    pub(crate) node: VfsNodeId,
    pub(crate) kind: FsNodeKind,
    pub(crate) visible_path: Option<String>,
}

pub(crate) struct VfsCreateTarget<'a> {
    pub(crate) parent: VfsNodeId,
    pub(crate) leaf_name: &'a str,
    pub(crate) leaf_path: String,
}

pub(crate) enum VfsOpenTarget<'a> {
    Existing(VfsPath),
    Create(VfsCreateTarget<'a>),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum LookupMode {
    // Normal open/stat-style lookup: follow final symlinks and mounted roots.
    FollowFinal,
    // lstat/readlink/O_NOFOLLOW-style lookup: keep a final symlink as the node.
    NoFollowFinal,
    // mount/umount target lookup: follow the final symlink but stop before a
    // final mounted root so callers operate on the covered directory itself.
    MountTarget,
}

impl LookupMode {
    fn follow_final_symlink(self) -> bool {
        matches!(self, Self::FollowFinal | Self::MountTarget)
    }

    fn follow_final_mount(self) -> bool {
        !matches!(self, Self::MountTarget)
    }
}

#[derive(Clone, Debug)]
struct VfsCursor {
    node: VfsNodeId,
    kind: FsNodeKind,
    path: String,
    tracks_path: bool,
}

#[derive(Debug)]
struct VfsChildLookup {
    cursor: VfsCursor,
    parent_node: VfsNodeId,
    parent_kind: FsNodeKind,
    parent_path_len: usize,
}

#[derive(Clone, Debug)]
enum PathComponent<'a> {
    Borrowed(&'a str),
    Owned(String),
}

impl PathComponent<'_> {
    fn as_str(&self) -> &str {
        match self {
            Self::Borrowed(component) => component,
            Self::Owned(component) => component.as_str(),
        }
    }
}

impl VfsPath {
    pub(crate) fn new(node: VfsNodeId, kind: FsNodeKind) -> Self {
        Self {
            node,
            kind,
            visible_path: None,
        }
    }

    pub(crate) fn with_visible_path(
        node: VfsNodeId,
        kind: FsNodeKind,
        visible_path: String,
    ) -> Self {
        Self {
            node,
            kind,
            visible_path: Some(visible_path),
        }
    }
}

impl VfsCreateTarget<'_> {
    pub(crate) fn synthetic_child(&self, _context: &PathContext) -> Option<VfsPath> {
        mounted_root_for_static_child(self.parent, self.leaf_name).map(|node| {
            VfsPath::with_visible_path(node, FsNodeKind::Directory, self.leaf_path.clone())
        })
    }
}

impl VfsCursor {
    fn root(context: &PathContext) -> Self {
        let root = context.root();
        Self {
            node: VfsNodeId::new(root.mount_id(), root.ino()),
            kind: FsNodeKind::Directory,
            path: String::from(context.root_path()),
            tracks_path: true,
        }
    }

    fn root_with_capacity(context: &PathContext, additional: usize) -> Self {
        let root = context.root();
        let root_path = context.root_path();
        let mut path = String::with_capacity(root_path.len().saturating_add(additional));
        path.push_str(root_path);
        Self {
            node: VfsNodeId::new(root.mount_id(), root.ino()),
            kind: FsNodeKind::Directory,
            path,
            tracks_path: true,
        }
    }

    fn from_working_dir_with_capacity(context: &PathContext, additional: usize) -> Self {
        let cwd = context.cwd();
        let cwd_path = context.cwd_path();
        let mut path = String::with_capacity(cwd_path.len().saturating_add(additional));
        path.push_str(cwd_path);
        Self {
            node: VfsNodeId::new(cwd.mount_id(), cwd.ino()),
            kind: FsNodeKind::Directory,
            path,
            tracks_path: true,
        }
    }

    fn numeric_start(context: &PathContext, path: &str) -> Self {
        let working_dir = if path.starts_with('/') {
            context.root()
        } else {
            context.cwd()
        };
        Self {
            node: VfsNodeId::new(working_dir.mount_id(), working_dir.ino()),
            kind: FsNodeKind::Directory,
            path: String::new(),
            tracks_path: false,
        }
    }

    fn root_for_tracking(context: &PathContext, tracks_path: bool) -> Self {
        if tracks_path {
            Self::root(context)
        } else {
            Self::numeric_start(context, "/")
        }
    }

    fn materialize_path(&mut self, context: &PathContext, path: &str) -> FsResult {
        if self.tracks_path {
            return Ok(());
        }
        let base_path = if path.starts_with('/') {
            context.root_path()
        } else {
            context.cwd_path()
        };
        let _profile_scope = perf::time_scope(perf::ProfilePoint::VfsLookupVisiblePath);
        let visible_path = if path == "/" {
            String::from(context.root_path())
        } else if path_is_simple(path) {
            if path.starts_with('/') {
                if context.root_path() == "/" {
                    String::from(path)
                } else {
                    alloc::format!("{}{path}", context.root_path())
                }
            } else if base_path == "/" {
                alloc::format!("/{path}")
            } else {
                alloc::format!("{base_path}/{path}")
            }
        } else {
            normalize_path_at_root(context.root_path(), base_path, path).ok_or(FsError::NotFound)?
        };
        perf::record_vfs_visible_path_update(1);
        self.path = visible_path;
        self.tracks_path = true;
        Ok(())
    }

    fn into_path(self) -> VfsPath {
        VfsPath::with_visible_path(self.node, self.kind, self.path)
    }

    fn is_mount_root(&self) -> bool {
        root_ino_for(self.node.mount_id).is_some_and(|root_ino| self.node.ino == root_ino)
    }
}

fn path_is_simple(path: &str) -> bool {
    let components = path.strip_prefix('/').unwrap_or(path);
    !components.is_empty()
        && !components.ends_with('/')
        && components
            .split('/')
            .all(|component| !component.is_empty() && component != "." && component != "..")
}

impl VfsChildLookup {
    fn into_parent(mut self) -> VfsCursor {
        self.cursor.path.truncate(self.parent_path_len);
        VfsCursor {
            node: self.parent_node,
            kind: self.parent_kind,
            path: self.cursor.path,
            tracks_path: self.cursor.tracks_path,
        }
    }
}

fn follow_mounted_root(context: &PathContext, cursor: VfsCursor) -> VfsCursor {
    if cursor.kind != FsNodeKind::Directory {
        return cursor;
    }
    if !cursor.tracks_path {
        debug_assert!(!namespace_has_dynamic_mounts(context.namespace_id()));
        return cursor;
    }
    if let Some(node) = mounted_root_for(context.namespace_id(), cursor.node, cursor.path.as_str())
    {
        return VfsCursor {
            node,
            kind: FsNodeKind::Directory,
            path: cursor.path,
            tracks_path: true,
        };
    }
    cursor
}

fn join_visible_path(base: &str, component: &str) -> String {
    let _profile_scope = perf::time_scope(perf::ProfilePoint::VfsLookupVisiblePath);
    perf::record_vfs_visible_path_update(1);
    if base == "/" {
        alloc::format!("/{component}")
    } else {
        alloc::format!("{base}/{component}")
    }
}

fn reserve_visible_path(path: &mut String, additional: usize) {
    if path.capacity().saturating_sub(path.len()) < additional {
        perf::record_vfs_visible_path_allocation();
        path.reserve(additional);
    }
}

fn reserve_visible_path_for_lookup(cursor: &mut VfsCursor, path: &str) {
    reserve_visible_path(&mut cursor.path, path.len().saturating_add(1));
}

fn push_visible_path_component(path: &mut String, component: &str) {
    let _profile_scope = perf::time_scope(perf::ProfilePoint::VfsLookupVisiblePath);
    perf::record_vfs_visible_path_update(0);
    let slash_len = usize::from(path.as_str() != "/");
    reserve_visible_path(path, slash_len + component.len());
    if path.as_str() != "/" {
        path.push('/');
    }
    path.push_str(component);
}

fn truncate_visible_path_parent(path: &mut String) {
    let _profile_scope = perf::time_scope(perf::ProfilePoint::VfsLookupVisiblePath);
    perf::record_vfs_visible_path_update(0);
    if path.as_str() == "/" {
        return;
    }
    let parent_len = match path.rfind('/') {
        Some(0) | None => 1,
        Some(index) => index,
    };
    path.truncate(parent_len);
}

fn parent_visible_path(path: &str) -> String {
    let _profile_scope = perf::time_scope(perf::ProfilePoint::VfsLookupVisiblePath);
    perf::record_vfs_visible_path_update(1);
    if path == "/" {
        return String::from("/");
    }
    match path.rsplit_once('/') {
        Some(("", _)) | None => String::from("/"),
        Some((parent, _)) => String::from(parent),
    }
}

fn lookup_cached_child(
    namespace_id: MountNamespaceId,
    parent_node: VfsNodeId,
    component: &str,
) -> FsResult<DentryLookupResult> {
    let cacheable = component != ".." && mount_supports_dentry_cache(parent_node.mount_id);
    let lookup_backend = || -> FsResult<DentryLookupResult> {
        inode_state::with_directory_read(parent_node, |_| {
            let token = cacheable
                .then(|| dentry_cache::version_token(parent_node))
                .flatten();
            let result = {
                let _profile_scope = perf::time_scope(perf::ProfilePoint::VfsLookupBackend);
                with_mount(parent_node.mount_id, BackendOp::Lookup, |mount| {
                    mount.lookup_component_from(parent_node.ino, component)
                })
                .ok_or(FsError::Io)?
            };
            match result {
                Ok((ino, kind)) => {
                    let node = VfsNodeId::new(parent_node.mount_id, ino);
                    if let Some(token) = token {
                        let _profile_scope =
                            perf::time_scope(perf::ProfilePoint::VfsLookupDentryInsert);
                        dentry_cache::insert_positive(
                            namespace_id,
                            parent_node,
                            token,
                            component,
                            node,
                            kind,
                        );
                    }
                    Ok(DentryLookupResult::Positive { node, kind })
                }
                Err(FsError::NotFound) => {
                    if let Some(token) = token {
                        let _profile_scope =
                            perf::time_scope(perf::ProfilePoint::VfsLookupDentryInsert);
                        dentry_cache::insert_negative(namespace_id, parent_node, token, component);
                    }
                    Ok(DentryLookupResult::Negative)
                }
                Err(err) => Err(err),
            }
        })
    };

    let cached = cacheable.then(|| {
        let _profile_scope = perf::time_scope(perf::ProfilePoint::VfsLookupDentry);
        dentry_cache::lookup(namespace_id, parent_node, component)
    });
    match cached.flatten() {
        Some(cached) => Ok(cached),
        None if cacheable => {
            dentry_cache::with_lookup_single_flight(namespace_id, parent_node, component, || {
                let cached = {
                    let _profile_scope = perf::time_scope(perf::ProfilePoint::VfsLookupDentry);
                    dentry_cache::lookup(namespace_id, parent_node, component)
                };
                cached.map_or_else(lookup_backend, Ok)
            })
        }
        None => lookup_backend(),
    }
}

/// Resolves the allocation-free numeric subset used by relative metadata
/// probes. Direct regular children cannot cross a mount or follow a symlink;
/// every other shape returns `None` so the full visible-path resolver retains
/// exact Linux-compatible fallback behavior.
pub(crate) fn resolve_direct_regular_child_in(
    namespace_id: MountNamespaceId,
    parent: WorkingDir,
    component: &str,
) -> FsResult<Option<VfsNodeId>> {
    if component.is_empty()
        || component == "."
        || component == ".."
        || component.contains('/')
        || component.len() > EXT4_NAME_MAX
    {
        return Ok(None);
    }
    let parent = VfsNodeId::new(parent.mount_id(), parent.ino());
    match lookup_cached_child(namespace_id, parent, component)? {
        DentryLookupResult::Positive {
            node,
            kind: FsNodeKind::RegularFile,
        } => Ok(Some(node)),
        DentryLookupResult::Positive { .. } | DentryLookupResult::Negative => Ok(None),
    }
}

fn lookup_child_raw(
    context: &PathContext,
    mut cursor: VfsCursor,
    component: &str,
) -> FsResult<VfsChildLookup> {
    if cursor.kind != FsNodeKind::Directory {
        return Err(FsError::NotDir);
    }
    if component.len() > EXT4_NAME_MAX {
        return Err(FsError::NameTooLong);
    }

    let parent_node = cursor.node;
    let parent_kind = cursor.kind;
    let parent_path_len = cursor.path.len();
    if cursor.tracks_path {
        if component == ".." {
            truncate_visible_path_parent(&mut cursor.path);
        } else {
            push_visible_path_component(&mut cursor.path, component);
        };
    }
    if component != ".."
        && let Some(node) = mounted_root_for_static_child(parent_node, component)
    {
        // Static boot mounts are VFS edges, not necessarily backend dirents.
        // Resolve them before backend lookup so a root image without the
        // covered directory can still expose the mount point.
        cursor.node = node;
        cursor.kind = FsNodeKind::Directory;
        return Ok(VfsChildLookup {
            cursor,
            parent_node,
            parent_kind,
            parent_path_len,
        });
    }

    let result = lookup_cached_child(context.namespace_id(), parent_node, component)?;
    let DentryLookupResult::Positive { node, kind } = result else {
        return Err(FsError::NotFound);
    };
    cursor.node = node;
    cursor.kind = kind;
    Ok(VfsChildLookup {
        cursor,
        parent_node,
        parent_kind,
        parent_path_len,
    })
}

fn lookup_parent(context: &PathContext, cursor: VfsCursor) -> FsResult<VfsCursor> {
    if cursor.is_mount_root() {
        if cursor.node.mount_id == primary_mount_id() {
            return Ok(VfsCursor::root_for_tracking(context, cursor.tracks_path));
        }
        if let Some(parent) =
            mounted_root_parent(context.namespace_id(), cursor.node, cursor.path.as_str())
        {
            return Ok(VfsCursor {
                node: parent,
                kind: FsNodeKind::Directory,
                path: cursor
                    .tracks_path
                    .then(|| parent_visible_path(cursor.path.as_str()))
                    .unwrap_or_default(),
                tracks_path: cursor.tracks_path,
            });
        }
        // UNFINISHED: This kernel still allows unmounting without mount-user
        // reference checks, so a cwd can point at a detached mounted root. Linux
        // keeps such paths alive through mount references; we currently fall
        // back to `/` for that orphaned case.
        return Ok(VfsCursor::root_for_tracking(context, cursor.tracks_path));
    }
    lookup_child_raw(context, cursor, "..").map(|child| child.cursor)
}

fn lookup_parent_in_context(cursor: VfsCursor, context: &PathContext) -> FsResult<VfsCursor> {
    let root = context.root();
    if cursor.node == VfsNodeId::new(root.mount_id(), root.ino()) {
        return Ok(cursor);
    }
    lookup_parent(context, cursor)
}

fn start_cursor(context: &PathContext, path: &str, track_path: bool) -> VfsCursor {
    if !track_path {
        return VfsCursor::numeric_start(context, path);
    }
    let capacity_hint = path.len().saturating_add(1);
    if path.starts_with('/') {
        VfsCursor::root_with_capacity(context, capacity_hint)
    } else {
        VfsCursor::from_working_dir_with_capacity(context, capacity_hint)
    }
}

fn borrowed_path_components(path: &str) -> Vec<PathComponent<'_>> {
    let _profile_scope = perf::time_scope(perf::ProfilePoint::VfsLookupPathComponents);
    let components: Vec<PathComponent<'_>> = path
        .split('/')
        .filter(|component| !component.is_empty() && *component != ".")
        .map(PathComponent::Borrowed)
        .collect();
    perf::record_vfs_path_components(components.len(), 0);
    components
}

fn owned_path_components<'a>(path: &str) -> Vec<PathComponent<'a>> {
    let _profile_scope = perf::time_scope(perf::ProfilePoint::VfsLookupPathComponents);
    let components: Vec<PathComponent<'a>> = path
        .split('/')
        .filter(|component| !component.is_empty() && *component != ".")
        .map(|component| PathComponent::Owned(String::from(component)))
        .collect();
    perf::record_vfs_path_components(components.len(), components.len());
    components
}

fn read_symlink_target(cursor: &VfsCursor) -> FsResult<String> {
    let mut buffer = vec![0u8; SYMLINK_TARGET_MAX + 1];
    let len = inode_state::with_mapping_read(cursor.node, || {
        let plan = with_mount(cursor.node.mount_id, BackendOp::ReadPlan, |mount| {
            mount.prepare_readlink_plan(cursor.node.ino, buffer.len())
        })
        .flatten();
        if let Some(plan) = plan {
            Ok(plan.execute(&mut buffer))
        } else {
            with_mount(cursor.node.mount_id, BackendOp::Readlink, |mount| {
                mount.readlink(cursor.node.ino, &mut buffer)
            })
            .ok_or(FsError::Io)?
        }
    })?;
    if len > SYMLINK_TARGET_MAX {
        return Err(FsError::NameTooLong);
    }
    let target = core::str::from_utf8(&buffer[..len]).map_err(|_| FsError::InvalidInput)?;
    Ok(String::from(target))
}

fn resolve_path_streaming_no_symlink(
    context: &PathContext,
    path: &str,
    mode: LookupMode,
) -> FsResult<Option<VfsCursor>> {
    if path.is_empty() {
        return Err(FsError::NotFound);
    }
    let track_path = namespace_has_dynamic_mounts(context.namespace_id());
    let mut cursor = start_cursor(context, path, track_path);
    let mut components_seen = 0usize;
    let mut components = path
        .split('/')
        .filter(|component| !component.is_empty() && *component != ".")
        .peekable();

    while let Some(component) = components.next() {
        components_seen += 1;
        let is_final = components.peek().is_none();
        if component == ".." {
            cursor = lookup_parent_in_context(cursor, context)?;
        } else {
            let child = lookup_child_raw(context, cursor, component)?;
            if child.cursor.kind == FsNodeKind::Symlink
                && (!is_final || mode.follow_final_symlink())
            {
                perf::record_vfs_path_components(components_seen, 0);
                return Ok(None);
            }
            cursor = child.cursor;
        }
        if mode.follow_final_mount() || !is_final {
            cursor = follow_mounted_root(context, cursor);
        }
    }
    perf::record_vfs_path_components(components_seen, 0);
    if mode.follow_final_mount() && components_seen == 0 {
        cursor = follow_mounted_root(context, cursor);
    }
    cursor.materialize_path(context, path)?;
    Ok(Some(cursor))
}

fn resolve_path_with_component_vec(
    context: PathContext,
    path: &str,
    mode: LookupMode,
) -> FsResult<VfsCursor> {
    let mut cursor = start_cursor(&context, path, true);
    let mut components = borrowed_path_components(path);
    let mut index = 0usize;
    let mut symlink_follows = 0usize;

    if mode.follow_final_mount() && components.is_empty() {
        cursor = follow_mounted_root(&context, cursor);
    }
    while index < components.len() {
        let is_final = index + 1 == components.len();
        let component = components[index].as_str();
        if component == ".." {
            cursor = lookup_parent_in_context(cursor, &context)?;
        } else {
            let child = lookup_child_raw(&context, cursor, component)?;
            if child.cursor.kind == FsNodeKind::Symlink
                && (!is_final || mode.follow_final_symlink())
            {
                if symlink_follows == MAX_SYMLINK_FOLLOWS {
                    return Err(FsError::Loop);
                }
                symlink_follows += 1;

                let target = read_symlink_target(&child.cursor)?;
                let mut next_components = owned_path_components(target.as_str());
                next_components.extend(components[index + 1..].iter().cloned());
                components = next_components;
                index = 0;
                cursor = if target.starts_with('/') {
                    VfsCursor::root(&context)
                } else {
                    child.into_parent()
                };
                reserve_visible_path_for_lookup(&mut cursor, target.as_str());
                if mode.follow_final_mount() && components.is_empty() {
                    cursor = follow_mounted_root(&context, cursor);
                }
                continue;
            } else {
                cursor = child.cursor;
            }
        }
        if mode.follow_final_mount() || !is_final {
            cursor = follow_mounted_root(&context, cursor);
        }
        index += 1;
    }
    Ok(cursor)
}

fn resolve_path_inner(context: PathContext, path: &str, mode: LookupMode) -> FsResult<VfsCursor> {
    if path.is_empty() {
        return Err(FsError::NotFound);
    }
    if let Some(cursor) = resolve_path_streaming_no_symlink(&context, path, mode)? {
        return Ok(cursor);
    }
    resolve_path_with_component_vec(context, path, mode)
}

fn split_parent_path(path: &str) -> FsResult<(&str, &str)> {
    if path.is_empty() {
        return Err(FsError::NotFound);
    }
    let (parent_path, leaf_name) = match path.rsplit_once('/') {
        Some((parent_path, leaf_name)) => (parent_path, leaf_name),
        None => ("", path),
    };
    if leaf_name.is_empty() || leaf_name == "." || leaf_name == ".." {
        return Err(FsError::InvalidInput);
    }
    if leaf_name.len() > EXT4_NAME_MAX {
        return Err(FsError::NameTooLong);
    }
    Ok((parent_path, leaf_name))
}

fn parent_path_for_lookup<'a>(path: &str, parent_path: &'a str) -> &'a str {
    if path.starts_with('/') && parent_path.is_empty() {
        "/"
    } else {
        parent_path
    }
}

pub(crate) fn resolve_existing_in(
    context: PathContext,
    path: &str,
    mode: LookupMode,
) -> FsResult<VfsPath> {
    let _profile_scope = perf::time_scope(perf::ProfilePoint::VfsLookup);
    let resolved = resolve_path_inner(context, path, mode)?.into_path();
    if path.ends_with('/') && resolved.kind != FsNodeKind::Directory {
        return Err(FsError::NotDir);
    }
    Ok(resolved)
}

pub(crate) fn resolve_mount_target_in(context: PathContext, path: &str) -> FsResult<VfsPath> {
    resolve_existing_in(context, path, LookupMode::MountTarget)
}

pub(crate) fn resolve_create_parent_in(
    context: PathContext,
    path: &str,
) -> FsResult<VfsCreateTarget<'_>> {
    let (parent_path, leaf_name) = split_parent_path(path)?;
    let parent_path = parent_path_for_lookup(path, parent_path);
    let parent = if parent_path.is_empty() {
        let cursor = start_cursor(&context, path, true);
        follow_mounted_root(&context, cursor)
    } else {
        resolve_path_inner(context.clone(), parent_path, LookupMode::FollowFinal)?
    };
    if parent.kind != FsNodeKind::Directory {
        return Err(FsError::NotDir);
    }
    Ok(VfsCreateTarget {
        parent: parent.node,
        leaf_name,
        leaf_path: join_visible_path(parent.path.as_str(), leaf_name),
    })
}

pub(crate) fn resolve_open_in(
    context: PathContext,
    path: &str,
    follow_final_symlink: bool,
    for_create: bool,
) -> FsResult<VfsOpenTarget<'_>> {
    let mode = if follow_final_symlink {
        LookupMode::FollowFinal
    } else {
        LookupMode::NoFollowFinal
    };
    match resolve_existing_in(context.clone(), path, mode) {
        Ok(existing) => return Ok(VfsOpenTarget::Existing(existing)),
        Err(FsError::NotFound) if for_create => {}
        Err(err) => return Err(err),
    }

    if !for_create {
        return Err(FsError::NotFound);
    }
    Ok(VfsOpenTarget::Create(resolve_create_parent_in(
        context, path,
    )?))
}
