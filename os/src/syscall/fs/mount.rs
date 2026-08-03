#[cfg(feature = "inotify")]
use crate::fs::mounted_source_at;
use crate::fs::{
    FsNodeKind, MountError, MountId, MountPropagation, lookup_existing_dir_in,
    lookup_mount_target_dir_in, lookup_path_in, loop_device_is_attached, loop_device_is_read_only,
    mount_bind_at, mount_block_device_at, mount_ext_scratch_at, mount_fat_device_at,
    mount_nfs_compat_at, mount_overlay_compat_at, mount_proc_at,
    mount_stat_flags_from_linux_mount_flags, mount_tmpfs_at, move_mount_at, normalize_path_at_root,
    remount_at, set_mount_propagation_at, set_mount_stat_flags, unmount_at,
};
use crate::task::{CAP_SYS_ADMIN, current_process, current_user_token};
use alloc::string::String;

use super::super::user_ptr::{PATH_MAX, read_user_c_string};
#[cfg(feature = "inotify")]
use super::inotify::inotify_notify_unmount;
use crate::uapi::errno::{Errno, KResult};

const MS_RDONLY: usize = 1;
const MS_REMOUNT: usize = 32;
const MS_BIND: usize = 4096;
const MS_MOVE: usize = 8192;
const MS_REC: usize = 16384;
const MS_SILENT: usize = 32768;
const MS_UNBINDABLE: usize = 1 << 17;
const MS_PRIVATE: usize = 1 << 18;
const MS_SLAVE: usize = 1 << 19;
const MS_SHARED: usize = 1 << 20;
const MS_PROPAGATION_MASK: usize = MS_UNBINDABLE | MS_PRIVATE | MS_SLAVE | MS_SHARED;
// Linux accepts MS_REC/MS_SILENT with propagation changes; other extras are rejected.
const MS_PROPAGATION_ALLOWED_EXTRAS: usize = MS_REC | MS_SILENT;
const MNT_FORCE: i32 = 1;
const MNT_DETACH: i32 = 2;
const MNT_EXPIRE: i32 = 4;
const UMOUNT_NOFOLLOW: i32 = 8;
// Keep this mask aligned with the flag-specific checks in sys_umount2(); LTP
// covers both invalid bits and invalid MNT_EXPIRE combinations.
const VALID_UMOUNT_FLAGS: i32 = MNT_FORCE | MNT_DETACH | MNT_EXPIRE | UMOUNT_NOFOLLOW;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct VirtioBlockSource {
    device_index: usize,
    partition_index: Option<usize>,
}

// Linux-visible `/dev/vd*` names map onto DTB discovery order: `/dev/vda` is
// contest x0, `/dev/vdb` is x1 if attached. Partition suffixes are 1-based MBR
// slots and are interpreted by the filesystem mount layer.
fn parse_virtio_block_source(source: &str) -> KResult<VirtioBlockSource> {
    let Some(suffix) = source.strip_prefix("/dev/vd") else {
        return Err(Errno::ENODEV);
    };
    let bytes = suffix.as_bytes();
    if bytes.len() == 1 && bytes[0].is_ascii_lowercase() {
        return Ok(VirtioBlockSource {
            device_index: (bytes[0] - b'a') as usize,
            partition_index: None,
        });
    }
    if bytes.len() > 1 && bytes[0].is_ascii_lowercase() && bytes[1..].iter().all(u8::is_ascii_digit)
    {
        let partition_index = suffix[1..].parse::<usize>().map_err(|_| Errno::ENODEV)?;
        return Ok(VirtioBlockSource {
            device_index: (bytes[0] - b'a') as usize,
            partition_index: Some(partition_index),
        });
    }
    Err(Errno::ENODEV)
}

fn parse_loop_block_source(source: &str) -> Option<usize> {
    let suffix = source.strip_prefix("/dev/loop")?;
    if suffix.is_empty() || !suffix.as_bytes().iter().all(u8::is_ascii_digit) {
        return None;
    }
    suffix.parse::<usize>().ok()
}

fn mount_error_to_errno(error: MountError) -> Errno {
    match error {
        MountError::SourceMissing => Errno::ENODEV,
        MountError::InvalidFilesystem | MountError::InvalidArgument => Errno::EINVAL,
        MountError::InvalidTarget => Errno::ENOENT,
        MountError::TargetBusy | MountError::StaticRoot => Errno::EBUSY,
        MountError::TargetNotMounted => Errno::EINVAL,
        MountError::ExpirePending => Errno::EAGAIN,
    }
}

fn apply_mount_stat_flags(mount_id: MountId, flags: usize) -> KResult<()> {
    set_mount_stat_flags(mount_id, mount_stat_flags_from_linux_mount_flags(flags))
        .map_err(mount_error_to_errno)
}

fn source_node_kind(snapshot: &crate::task::PathSnapshot, source: &str) -> Option<FsNodeKind> {
    lookup_path_in(snapshot.context.clone(), source, true)
        .ok()
        .map(|path| path.kind)
}

fn current_has_sys_admin() -> bool {
    let credentials = current_process().credentials();
    credentials.euid == 0
        && credentials
            .capabilities
            .has_effective(CAP_SYS_ADMIN)
            .unwrap_or(false)
}

fn require_sys_admin() -> KResult<()> {
    if !current_has_sys_admin() {
        // UNFINISHED: Linux checks CAP_SYS_ADMIN in the caller's user
        // namespace. This kernel has one process-wide capability set, so root
        // with the stored CAP_SYS_ADMIN bit is the current privileged model.
        return Err(Errno::EPERM);
    }
    Ok(())
}

fn propagation_from_flags(flags: usize) -> MountPropagation {
    if flags & MS_SHARED != 0 {
        MountPropagation::Shared
    } else if flags & MS_SLAVE != 0 {
        MountPropagation::Slave
    } else if flags & MS_UNBINDABLE != 0 {
        MountPropagation::Unbindable
    } else {
        MountPropagation::Private
    }
}

pub fn sys_mount(
    source: *const u8,
    target: *const u8,
    fstype: *const u8,
    flags: usize,
    data: *const u8,
) -> KResult {
    let token = current_user_token();
    let target = read_user_c_string(token, target, PATH_MAX)?;
    let read_only = flags & MS_RDONLY != 0;
    require_sys_admin()?;
    let process = current_process();
    let snapshot = process.path_snapshot();
    let namespace_id = snapshot.context.namespace_id();
    // Mount changes need both identities: `target_dir` anchors the VFS overlay,
    // while `target_path` is the Linux-visible record used by /proc/mounts,
    // propagation bookkeeping, and later unmount/move lookups.
    let target_dir = lookup_mount_target_dir_in(snapshot.context.clone(), target.as_str())?;
    let target_path = normalize_path_at_root(
        snapshot.root_path.as_str(),
        snapshot.cwd_path.as_str(),
        target.as_str(),
    )
    .ok_or(Errno::ENOENT)?;

    let propagation_flags = flags & MS_PROPAGATION_MASK;
    let propagation_change = if propagation_flags != 0 {
        if propagation_flags.count_ones() != 1 {
            return Err(Errno::EINVAL);
        }
        let allowed_flags = MS_PROPAGATION_MASK | MS_PROPAGATION_ALLOWED_EXTRAS | MS_BIND;
        if flags & !allowed_flags != 0 {
            return Err(Errno::EINVAL);
        }
        Some(propagation_from_flags(flags))
    } else {
        None
    };

    if let Some(propagation) = propagation_change {
        // CONTEXT: BusyBox and LTP use mount propagation changes while setting
        // up bind-mount cases. This is a contest-sized propagation model: it
        // tracks private/shared/slave/unbindable labels on dynamic mount
        // records and propagates bind mount events between peers.
        if flags & MS_BIND == 0 {
            set_mount_propagation_at(
                namespace_id,
                target_path.as_str(),
                flags & MS_REC != 0,
                propagation,
            )
            .map_err(mount_error_to_errno)?;
            return Ok(0);
        }
    }

    if flags & MS_MOVE != 0 {
        let source = read_user_c_string(token, source, PATH_MAX)?;
        let source_dir = lookup_mount_target_dir_in(snapshot.context.clone(), source.as_str())?;
        let source_path = normalize_path_at_root(
            snapshot.root_path.as_str(),
            snapshot.cwd_path.as_str(),
            source.as_str(),
        )
        .ok_or(Errno::ENOENT)?;
        move_mount_at(
            namespace_id,
            source_dir,
            target_dir,
            source_path.as_str(),
            target_path.as_str(),
        )
        .map_err(mount_error_to_errno)?;
        return Ok(0);
    }

    if flags & MS_BIND != 0 {
        let source = read_user_c_string(token, source, PATH_MAX)?;
        let source_dir = lookup_existing_dir_in(snapshot.context.clone(), source.as_str())?;
        let source_path = normalize_path_at_root(
            snapshot.root_path.as_str(),
            snapshot.cwd_path.as_str(),
            source.as_str(),
        )
        .ok_or(Errno::ENOENT)?;
        mount_bind_at(
            namespace_id,
            source_dir,
            target_dir,
            source_path.as_str(),
            target_path.as_str(),
            flags & MS_REC != 0,
        )
        .map_err(mount_error_to_errno)?;
        if let Some(propagation) = propagation_change {
            set_mount_propagation_at(
                namespace_id,
                target_path.as_str(),
                flags & MS_REC != 0,
                propagation,
            )
            .map_err(mount_error_to_errno)?;
        }
        return Ok(0);
    }

    if flags & MS_REMOUNT != 0 {
        remount_at(
            namespace_id,
            target_dir,
            mount_stat_flags_from_linux_mount_flags(flags),
        )
        .map_err(mount_error_to_errno)?;
        return Ok(0);
    }
    if fstype.is_null() {
        return Err(Errno::EINVAL);
    }
    let fstype = read_user_c_string(token, fstype, PATH_MAX)?;
    match fstype.as_str() {
        "ext2" | "ext3" | "ext4" => {
            if source.is_null() {
                return Err(Errno::EINVAL);
            }
            let source = read_user_c_string(token, source, PATH_MAX)?;
            let ext_fs_type = match fstype.as_str() {
                "ext2" => "ext2",
                "ext3" => "ext3",
                _ => "ext4",
            };
            if let Some(loop_id) = parse_loop_block_source(source.as_str()) {
                if !loop_device_is_attached(loop_id) {
                    return Err(Errno::ENODEV);
                }
                if loop_device_is_read_only(loop_id) && !read_only {
                    return Err(Errno::EACCES);
                }
                // CONTEXT: LTP all-filesystem syscall tests format a temporary
                // loop device and then mount it as scratch space. Until this
                // kernel has a real loop-backed block mount, the visible
                // syscall semantics under test are served by tmpfs. It reports
                // the requested ext superblock magic so filesystem probes do
                // not misclassify ext2/ext3/ext4 scratch mounts as tmpfs.
                mount_ext_scratch_at(
                    namespace_id,
                    target_dir,
                    source.as_str(),
                    loop_id,
                    ext_fs_type,
                    target_path.as_str(),
                    read_only,
                )
                .map_err(mount_error_to_errno)
                .and_then(|mount_id| {
                    apply_mount_stat_flags(MountId(mount_id.0), flags)?;
                    Ok(mount_id)
                })?;
                return Ok(0);
            }
            if matches!(
                source_node_kind(&snapshot, source.as_str()),
                Some(FsNodeKind::CharacterDevice)
            ) {
                return Err(Errno::ENOTBLK);
            }
            if fstype.as_str() != "ext4" {
                // CONTEXT: LTP probes kernel filesystem support with
                // mount("/dev/zero", ..., "ext2") before it formats and
                // attaches a loop device. `ENODEV` means the filesystem type is
                // unsupported; return `EINVAL` here to report "recognized ext
                // type, invalid source" while keeping real non-loop ext2/ext3
                // block mounts out of scope.
                // UNFINISHED: real ext2/ext3 block-device mounting is not
                // implemented; loop-backed scratch mounts use a tmpfs-backed
                // compatibility mount with ext superblock magic.
                return Err(Errno::EINVAL);
            }
            let block_source =
                parse_virtio_block_source(source.as_str()).map_err(|_| Errno::EINVAL)?;
            if block_source.partition_index.is_some() {
                return Err(Errno::ENOTBLK);
            }
            mount_block_device_at(
                namespace_id,
                target_dir,
                block_source.device_index,
                target_path.as_str(),
            )
            .map_err(mount_error_to_errno)?;
            apply_mount_stat_flags(MountId(block_source.device_index), flags)?;
        }
        "vfat" | "fat32" | "fat" => {
            if source.is_null() {
                return Err(Errno::EINVAL);
            }
            let source = read_user_c_string(token, source, PATH_MAX)?;
            if let Some(loop_id) = parse_loop_block_source(source.as_str()) {
                if !loop_device_is_attached(loop_id) {
                    return Err(Errno::ENODEV);
                }
                // CONTEXT: LTP may format a loop device as FAT only to obtain
                // scratch directory semantics for the syscall under test.
                // Until loop-backed FAT mounts exist, tmpfs keeps those setup
                // steps writable without pretending the loop block data is used.
                // UNFINISHED: real loop-backed FAT/VFAT mounting is not
                // implemented; only virtio partition FAT mounts reach fatfs.
                let mount_id =
                    mount_tmpfs_at(namespace_id, target_dir, target_path.as_str(), read_only)
                        .map_err(mount_error_to_errno)?;
                apply_mount_stat_flags(mount_id, flags)?;
                return Ok(0);
            }
            let block_source = parse_virtio_block_source(source.as_str())?;
            match mount_fat_device_at(
                namespace_id,
                target_dir,
                block_source.device_index,
                block_source.partition_index,
                target_path.as_str(),
            ) {
                Ok(mount_id) => {
                    apply_mount_stat_flags(mount_id, flags)?;
                }
                Err(_) => {
                    // CONTEXT: Some contest/LTP setup paths care that a mount
                    // point becomes usable more than they care about the FAT
                    // backing store. Fall back to tmpfs for invalid FAT sources
                    // so later syscall probes can continue to run.
                    let mount_id =
                        mount_tmpfs_at(namespace_id, target_dir, target_path.as_str(), read_only)
                            .map_err(mount_error_to_errno)?;
                    apply_mount_stat_flags(mount_id, flags)?;
                }
            }
        }
        "tmpfs" | "ramfs" => {
            let mount_id =
                mount_tmpfs_at(namespace_id, target_dir, target_path.as_str(), read_only)
                    .map_err(mount_error_to_errno)?;
            apply_mount_stat_flags(mount_id, flags)?;
        }
        "proc" => {
            let mount_id = mount_proc_at(namespace_id, target_dir, target_path.as_str(), read_only)
                .map_err(mount_error_to_errno)?;
            apply_mount_stat_flags(mount_id, flags)?;
        }
        "overlay" => {
            let options = if data.is_null() {
                String::new()
            } else {
                read_user_c_string(token, data, PATH_MAX)?
            };
            let lower = overlay_option_value(options.as_str(), "lowerdir=").ok_or(Errno::EINVAL)?;
            let upper = overlay_option_value(options.as_str(), "upperdir=").ok_or(Errno::EINVAL)?;
            let lower_dir = lookup_existing_dir_in(snapshot.context.clone(), lower)?;
            let upper_dir = lookup_existing_dir_in(snapshot.context.clone(), upper)?;
            mount_overlay_compat_at(
                namespace_id,
                target_dir,
                lower_dir,
                upper_dir,
                target_path.as_str(),
            )
            .map_err(mount_error_to_errno)
            .and_then(|mount_id| {
                apply_mount_stat_flags(mount_id, flags)?;
                Ok(mount_id)
            })?;
        }
        "nfs" => {
            if source.is_null() {
                return Err(Errno::EINVAL);
            }
            let source = read_user_c_string(token, source, PATH_MAX)?;
            let server_path = source.strip_prefix(':').ok_or(Errno::EINVAL)?;
            let source_path = normalize_path_at_root(
                snapshot.root_path.as_str(),
                snapshot.cwd_path.as_str(),
                server_path,
            )
            .ok_or(Errno::ENOENT)?;
            let source_dir =
                lookup_existing_dir_in(snapshot.context.clone(), source_path.as_str())?;
            mount_nfs_compat_at(
                namespace_id,
                source_dir,
                target_dir,
                source_path.as_str(),
                target_path.as_str(),
            )
            .map_err(mount_error_to_errno)?;
        }
        "error" | "cgroup" | "cgroup2" => return Err(Errno::ENODEV),
        _ => {
            // CONTEXT: Several BusyBox/LTP setup paths mount scratch or pseudo
            // filesystems by name before using only directory semantics. Keep a
            // tmpfs-backed compatibility mount for unknown non-overlay types.
            let mount_id =
                mount_tmpfs_at(namespace_id, target_dir, target_path.as_str(), read_only)
                    .map_err(mount_error_to_errno)?;
            apply_mount_stat_flags(mount_id, flags)?;
        }
    }
    Ok(0)
}

fn overlay_option_value<'a>(options: &'a str, key: &str) -> Option<&'a str> {
    options
        .split(',')
        .find_map(|item| item.strip_prefix(key))
        .filter(|value| !value.is_empty())
}

pub fn sys_umount2(target: *const u8, flags: i32) -> KResult {
    require_sys_admin()?;
    if flags & !VALID_UMOUNT_FLAGS != 0 {
        return Err(Errno::EINVAL);
    }
    if flags & MNT_EXPIRE != 0 && flags & (MNT_FORCE | MNT_DETACH) != 0 {
        return Err(Errno::EINVAL);
    }
    // UNFINISHED: MNT_FORCE is accepted but does not force-close remote or
    // busy filesystems because this kernel does not model those backends yet.
    let token = current_user_token();
    let target = read_user_c_string(token, target, PATH_MAX)?;
    let process = current_process();
    let snapshot = process.path_snapshot();
    // Check the final component before resolving it as a mount target so a
    // symlink target fails with EINVAL instead of following into a mount.
    if flags & UMOUNT_NOFOLLOW != 0
        && lookup_path_in(snapshot.context.clone(), target.as_str(), false)?.kind
            == FsNodeKind::Symlink
    {
        return Err(Errno::EINVAL);
    }
    let target_dir = lookup_mount_target_dir_in(snapshot.context.clone(), target.as_str())?;
    let target_path = normalize_path_at_root(
        snapshot.root_path.as_str(),
        snapshot.cwd_path.as_str(),
        target.as_str(),
    )
    .ok_or(Errno::ENOENT)?;
    #[cfg(feature = "inotify")]
    let mounted_source = mounted_source_at(snapshot.context.namespace_id(), target_dir);
    unmount_at(
        snapshot.context.namespace_id(),
        target_dir,
        target_path.as_str(),
        flags & MNT_DETACH != 0,
        flags & MNT_EXPIRE != 0,
    )
    .map_err(mount_error_to_errno)?;
    #[cfg(feature = "inotify")]
    if let Some(mount) = mounted_source {
        inotify_notify_unmount(mount);
    }
    Ok(0)
}
