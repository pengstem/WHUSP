use super::ext4::FsNodeKind;
use super::mount::{MountId, mount_exists, primary_mount_id, with_mount};
use lwext4_rust::ffi::EXT4_ROOT_INO;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct WorkingDir {
    mount_id: MountId,
    ino: u32,
}

impl WorkingDir {
    pub(crate) fn root() -> Self {
        Self {
            mount_id: primary_mount_id(),
            ino: EXT4_ROOT_INO,
        }
    }

    pub(crate) fn new(mount_id: MountId, ino: u32) -> Self {
        Self { mount_id, ino }
    }

    fn mount_id(self) -> MountId {
        self.mount_id
    }

    fn ino(self) -> u32 {
        self.ino
    }
}

pub(super) struct ResolvedFile {
    pub mount_id: MountId,
    pub ino: u32,
    pub kind: FsNodeKind,
}

pub(super) struct CreateTarget<'a> {
    pub mount_id: MountId,
    pub parent_ino: u32,
    pub leaf_name: &'a str,
}

pub(super) struct ParentTarget<'a> {
    pub mount_id: MountId,
    pub parent_ino: u32,
    pub leaf_name: &'a str,
}

pub(super) enum ResolvedOpen<'a> {
    Existing(ResolvedFile),
    Create(CreateTarget<'a>),
}

fn is_bare_name(path: &str) -> bool {
    !path.is_empty() && !path.starts_with('/') && !path.contains('/')
}

fn parse_prefixed_mount(component: &str) -> Option<MountId> {
    let suffix = component.strip_prefix('x')?;
    let index = suffix.parse::<usize>().ok()?;
    (index != 0).then_some(MountId(index))
}

fn resolve_absolute_mount(path: &str) -> Option<(MountId, &str)> {
    let relpath = path.strip_prefix('/')?;
    if relpath.is_empty() {
        return Some((primary_mount_id(), ""));
    }

    let (first_component, rest) = match relpath.split_once('/') {
        Some((first_component, rest)) => (first_component, Some(rest)),
        None => return Some((primary_mount_id(), relpath)),
    };

    let Some(mount_id) = parse_prefixed_mount(first_component) else {
        return Some((primary_mount_id(), relpath));
    };

    let mount_relpath = rest?.trim_start_matches('/');
    if mount_relpath.is_empty() || !mount_exists(mount_id) {
        return None;
    }
    Some((mount_id, mount_relpath))
}

fn resolve_on_mount<'a>(
    mount_id: MountId,
    base_ino: u32,
    relpath: &'a str,
    for_create: bool,
) -> Option<ResolvedOpen<'a>> {
    with_mount(mount_id, |mount| {
        // TODO: we need a ResolvedFile and CreateTarget new function.
        if for_create {
            if let Some((ino, kind)) = mount.lookup_path_from(base_ino, relpath) {
                Some(ResolvedOpen::Existing(ResolvedFile {
                    mount_id,
                    ino,
                    kind,
                }))
            } else {
                let (parent_ino, leaf_name) = mount.resolve_parent_from(base_ino, relpath)?;
                Some(ResolvedOpen::Create(CreateTarget {
                    mount_id,
                    parent_ino,
                    leaf_name,
                }))
            }
        } else {
            let (ino, kind) = mount.lookup_path_from(base_ino, relpath)?;
            Some(ResolvedOpen::Existing(ResolvedFile {
                mount_id,
                ino,
                kind,
            }))
        }
    })
    .flatten()
}

pub(super) fn resolve_open_target(
    cwd: Option<WorkingDir>,
    path: &str,
    _require_writable: bool,
    for_create: bool,
) -> Option<ResolvedOpen<'_>> {
    if path.starts_with('/') {
        let (mount_id, relpath) = resolve_absolute_mount(path)?;
        return resolve_on_mount(mount_id, EXT4_ROOT_INO, relpath, for_create);
    }

    if let Some(cwd) = cwd {
        return resolve_on_mount(cwd.mount_id(), cwd.ino(), path, for_create);
    }

    if is_bare_name(path) {
        return resolve_on_mount(primary_mount_id(), EXT4_ROOT_INO, path, for_create);
    }

    None
}

pub(super) fn resolve_parent_target(
    cwd: Option<WorkingDir>,
    path: &str,
) -> Option<ParentTarget<'_>> {
    if path.starts_with('/') {
        let (mount_id, relpath) = resolve_absolute_mount(path)?;
        let (parent_ino, leaf_name) = with_mount(mount_id, |mount| {
            mount.resolve_parent_from(EXT4_ROOT_INO, relpath)
        })??;
        return Some(ParentTarget {
            mount_id,
            parent_ino,
            leaf_name,
        });
    }

    if let Some(cwd) = cwd {
        let (parent_ino, leaf_name) = with_mount(cwd.mount_id(), |mount| {
            mount.resolve_parent_from(cwd.ino(), path)
        })??;
        return Some(ParentTarget {
            mount_id: cwd.mount_id(),
            parent_ino,
            leaf_name,
        });
    }

    if is_bare_name(path) {
        let mount_id = primary_mount_id();
        let (parent_ino, leaf_name) = with_mount(mount_id, |mount| {
            mount.resolve_parent_from(EXT4_ROOT_INO, path)
        })??;
        return Some(ParentTarget {
            mount_id,
            parent_ino,
            leaf_name,
        });
    }

    None
}

pub(crate) fn normalize_path(cwd_path: &str, path: &str) -> Option<alloc::string::String> {
    let mut segments = alloc::vec::Vec::new();
    if path.starts_with('/') {
        for segment in path.split('/') {
            if segment.is_empty() || segment == "." {
                continue;
            }
            if segment == ".." {
                return None;
            }
            segments.push(segment);
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
            // TODO: handle ".." segments
            if segment == ".." {
                return None;
            }
            segments.push(segment);
        }
    }

    if segments.is_empty() {
        Some("/".into())
    } else {
        Some(alloc::format!("/{}", segments.join("/")))
    }
}
