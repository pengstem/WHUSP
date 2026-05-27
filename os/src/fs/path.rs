use super::mount::{
    MountId, MountNamespaceId, ROOT_MOUNT_NAMESPACE, primary_mount_id, root_ino_for,
};
use alloc::string::String;

/// Numeric cwd/root anchor used by VFS lookup inside one mounted backend.
///
/// Keep this paired with `PathContext` string snapshots: `WorkingDir` is the
/// lookup identity, while cwd/root strings feed Linux-visible path ABIs such as
/// getcwd and procfs links.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct WorkingDir {
    mount_id: MountId,
    ino: u32,
}

/// Per-process pathname view used by dirfd, cwd, chroot, and mount namespaces.
///
/// `root`/`cwd` are authoritative for VFS lookup; `root_path`/`cwd_path` are
/// the user-visible normalized paths that must be updated by chdir/chroot-style
/// operations in the same step.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PathContext {
    root: WorkingDir,
    cwd: WorkingDir,
    namespace_id: MountNamespaceId,
    root_path: String,
    cwd_path: String,
}

impl WorkingDir {
    pub(crate) fn root() -> Self {
        let mount_id = primary_mount_id();
        Self {
            mount_id,
            ino: root_ino_for(mount_id).unwrap_or(2),
        }
    }

    pub(crate) fn new(mount_id: MountId, ino: u32) -> Self {
        Self { mount_id, ino }
    }

    pub(crate) fn mount_id(self) -> MountId {
        self.mount_id
    }

    pub(crate) fn ino(self) -> u32 {
        self.ino
    }
}

impl PathContext {
    pub(crate) fn new_in_namespace(
        root: WorkingDir,
        cwd: WorkingDir,
        namespace_id: MountNamespaceId,
        root_path: String,
        cwd_path: String,
    ) -> Self {
        Self {
            root,
            cwd,
            namespace_id,
            root_path,
            cwd_path,
        }
    }

    pub(crate) fn global_root() -> Self {
        let root = WorkingDir::root();
        Self {
            root,
            cwd: root,
            namespace_id: ROOT_MOUNT_NAMESPACE,
            root_path: "/".into(),
            cwd_path: "/".into(),
        }
    }

    pub(crate) fn root(&self) -> WorkingDir {
        self.root
    }

    pub(crate) fn cwd(&self) -> WorkingDir {
        self.cwd
    }

    pub(crate) fn namespace_id(&self) -> MountNamespaceId {
        self.namespace_id
    }

    pub(crate) fn root_path(&self) -> &str {
        self.root_path.as_str()
    }

    pub(crate) fn cwd_path(&self) -> &str {
        self.cwd_path.as_str()
    }

    pub(crate) fn is_global_root(&self) -> bool {
        self.namespace_id == ROOT_MOUNT_NAMESPACE && self.root == WorkingDir::root()
    }
}

pub(crate) fn normalize_path(cwd_path: &str, path: &str) -> Option<alloc::string::String> {
    let mut segments = alloc::vec::Vec::new();
    if path.starts_with('/') {
        for segment in path.split('/') {
            if segment.is_empty() || segment == "." {
                continue;
            }
            if segment == ".." {
                segments.pop();
            } else {
                segments.push(segment);
            }
        }
    } else {
        for segment in cwd_path.split('/') {
            if segment.is_empty() {
                continue;
            }
            segments.push(segment);
        }
        for segment in path.split('/') {
            if segment.is_empty() || segment == "." {
                continue;
            }
            if segment == ".." {
                segments.pop();
            } else {
                segments.push(segment);
            }
        }
    }

    if segments.is_empty() {
        Some("/".into())
    } else {
        Some(alloc::format!("/{}", segments.join("/")))
    }
}

fn collect_segments(path: &str) -> alloc::vec::Vec<&str> {
    path.split('/')
        .filter(|segment| !segment.is_empty() && *segment != ".")
        .collect()
}

fn build_path(segments: &[&str]) -> alloc::string::String {
    if segments.is_empty() {
        alloc::string::String::from("/")
    } else {
        alloc::format!("/{}", segments.join("/"))
    }
}

/// Normalizes a path without allowing `..` to escape `floor_path`.
///
/// This maintains the string snapshot used by cwd/chroot-visible ABIs. It does
/// not perform symlink expansion or mount traversal; those belong to VFS lookup.
fn normalize_path_above_floor(
    base_path: &str,
    path: &str,
    floor_path: &str,
) -> Option<alloc::string::String> {
    let floor_segments = collect_segments(floor_path);
    let mut segments = collect_segments(base_path);
    if segments.len() < floor_segments.len()
        || segments[..floor_segments.len()] != floor_segments[..]
    {
        return normalize_path(base_path, path);
    }

    for segment in path.split('/') {
        if segment.is_empty() || segment == "." {
            continue;
        }
        if segment == ".." {
            if segments.len() > floor_segments.len() {
                segments.pop();
            }
        } else {
            segments.push(segment);
        }
    }

    Some(build_path(&segments))
}

pub(crate) fn normalize_path_at_root(
    root_path: &str,
    cwd_path: &str,
    path: &str,
) -> Option<alloc::string::String> {
    // This is a string-level Linux chroot/cwd view. Symlink traversal and mount
    // overlay resolution stay in the VFS lookup layer.
    if path.starts_with('/') {
        normalize_path_above_floor(root_path, path, root_path)
    } else if path_inside_root(root_path, cwd_path).is_some() {
        normalize_path_above_floor(cwd_path, path, root_path)
    } else {
        normalize_path(cwd_path, path)
    }
}

pub(crate) fn path_inside_root(root_path: &str, path: &str) -> Option<alloc::string::String> {
    // The returned string is the process-visible path. A path outside the
    // current root intentionally returns None so callers can report Linux's
    // "(unreachable)" style cwd prefix where appropriate.
    if root_path == "/" {
        return Some(alloc::string::String::from(path));
    }
    if path == root_path {
        return Some(alloc::string::String::from("/"));
    }
    let suffix = path.strip_prefix(root_path)?;
    if suffix.starts_with('/') {
        Some(alloc::string::String::from(suffix))
    } else {
        None
    }
}
