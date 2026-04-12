use super::ext4::FsNodeKind;
use super::mount::{MountId, is_read_only, resolve_mount, with_mount};

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

pub(super) enum ResolvedOpen<'a> {
    Existing(ResolvedFile),
    Create(CreateTarget<'a>),
}

pub(super) fn resolve_open_target(
    path: &str,
    require_writable: bool,
    for_create: bool,
) -> Option<ResolvedOpen<'_>> {
    let (mount_id, relpath) = resolve_mount(path)?;
    if is_read_only(mount_id) && (require_writable || for_create) {
        return None;
    }

    with_mount(mount_id, |mount| {
        if for_create {
            if let Some((ino, kind)) = mount.lookup_path(relpath) {
                Some(ResolvedOpen::Existing(ResolvedFile {
                    mount_id,
                    ino,
                    kind,
                }))
            } else {
                let (parent_ino, leaf_name) = mount.resolve_parent(relpath)?;
                Some(ResolvedOpen::Create(CreateTarget {
                    mount_id,
                    parent_ino,
                    leaf_name,
                }))
            }
        } else {
            let (ino, kind) = mount.lookup_path(relpath)?;
            Some(ResolvedOpen::Existing(ResolvedFile {
                mount_id,
                ino,
                kind,
            }))
        }
    })
    .flatten()
}
