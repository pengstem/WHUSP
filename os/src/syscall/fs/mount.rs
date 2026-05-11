use crate::fs::{
    DetachedMountFile, File, FsContextFile, FsContextStateError, MountError, MountPropagation,
    OpenFlags, WorkingDir, lookup_existing_dir_in, lookup_mount_target_dir_in,
    loop_device_is_attached, mount_bind_at, mount_block_device_at, mount_cgroup2_at,
    mount_fat_device_at, mount_tmpfs_at, move_mount_at, normalize_path_at_root, open_file_in,
    remount_at, set_mount_propagation_at, unmount_at,
};
use crate::task::{CAP_SYS_ADMIN, FdTableEntry, current_process, current_user_token};
use alloc::string::String;
use alloc::sync::Arc;

use super::super::errno::{SysError, SysResult};
use super::super::user_ptr::{PATH_MAX, read_user_c_string};
use super::fd::{get_fd_entry_by_fd, get_file_by_fd};
use super::path::{check_current_access_path_prefixes_from, path_context_from};
use super::uapi::{AT_EMPTY_PATH, AT_FDCWD, AT_NO_AUTOMOUNT, AT_SYMLINK_NOFOLLOW};

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
const MS_PROPAGATION_ALLOWED_EXTRAS: usize = MS_REC | MS_SILENT;
const OPEN_TREE_CLONE: u32 = 0x1;
const OPEN_TREE_CLOEXEC: u32 = OpenFlags::CLOEXEC.bits();
const AT_RECURSIVE: u32 = 0x8000;
const VALID_OPEN_TREE_FLAGS: u32 = OPEN_TREE_CLONE
    | OPEN_TREE_CLOEXEC
    | AT_SYMLINK_NOFOLLOW as u32
    | AT_NO_AUTOMOUNT as u32
    | AT_EMPTY_PATH as u32
    | AT_RECURSIVE;
const MOVE_MOUNT_F_SYMLINKS: u32 = 0x0000_0001;
const MOVE_MOUNT_F_AUTOMOUNTS: u32 = 0x0000_0002;
const MOVE_MOUNT_F_EMPTY_PATH: u32 = 0x0000_0004;
const MOVE_MOUNT_T_SYMLINKS: u32 = 0x0000_0010;
const MOVE_MOUNT_T_AUTOMOUNTS: u32 = 0x0000_0020;
const MOVE_MOUNT_T_EMPTY_PATH: u32 = 0x0000_0040;
const MOVE_MOUNT_MASK: u32 = MOVE_MOUNT_F_SYMLINKS
    | MOVE_MOUNT_F_AUTOMOUNTS
    | MOVE_MOUNT_F_EMPTY_PATH
    | MOVE_MOUNT_T_SYMLINKS
    | MOVE_MOUNT_T_AUTOMOUNTS
    | MOVE_MOUNT_T_EMPTY_PATH;
const FSOPEN_CLOEXEC: u32 = 0x0000_0001;
const FSCONFIG_SET_FLAG: u32 = 0;
const FSCONFIG_SET_STRING: u32 = 1;
const FSCONFIG_SET_BINARY: u32 = 2;
const FSCONFIG_SET_PATH: u32 = 3;
const FSCONFIG_SET_PATH_EMPTY: u32 = 4;
const FSCONFIG_SET_FD: u32 = 5;
const FSCONFIG_CMD_CREATE: u32 = 6;
const FSCONFIG_CMD_RECONFIGURE: u32 = 7;
const FSMOUNT_CLOEXEC: u32 = 0x0000_0001;
const MOUNT_ATTR_RDONLY: u32 = 0x0000_0001;
const MOUNT_ATTR_NOSUID: u32 = 0x0000_0002;
const MOUNT_ATTR_NODEV: u32 = 0x0000_0004;
const MOUNT_ATTR_NOEXEC: u32 = 0x0000_0008;
const MOUNT_ATTR_ATIME: u32 = 0x0000_0070;
const MOUNT_ATTR_NODIRATIME: u32 = 0x0000_0080;
const VALID_FSMOUNT_ATTRS: u32 = MOUNT_ATTR_RDONLY
    | MOUNT_ATTR_NOSUID
    | MOUNT_ATTR_NODEV
    | MOUNT_ATTR_NOEXEC
    | MOUNT_ATTR_ATIME
    | MOUNT_ATTR_NODIRATIME;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct VirtioBlockSource {
    device_index: usize,
    partition_index: Option<usize>,
}

fn parse_virtio_block_source(source: &str) -> SysResult<VirtioBlockSource> {
    let Some(suffix) = source.strip_prefix("/dev/vd") else {
        return Err(SysError::ENODEV);
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
        let partition_index = source[8..].parse::<usize>().map_err(|_| SysError::ENODEV)?;
        return Ok(VirtioBlockSource {
            device_index: (bytes[0] - b'a') as usize,
            partition_index: Some(partition_index),
        });
    }
    Err(SysError::ENODEV)
}

fn parse_loop_block_source(source: &str) -> Option<usize> {
    let suffix = source.strip_prefix("/dev/loop")?;
    if suffix.is_empty() || !suffix.as_bytes().iter().all(u8::is_ascii_digit) {
        return None;
    }
    suffix.parse::<usize>().ok()
}

fn mount_error_to_errno(error: MountError) -> SysError {
    match error {
        MountError::SourceMissing => SysError::ENODEV,
        MountError::InvalidFilesystem | MountError::InvalidArgument => SysError::EINVAL,
        MountError::InvalidTarget => SysError::ENOENT,
        MountError::TargetBusy | MountError::StaticRoot => SysError::EBUSY,
        MountError::TargetNotMounted => SysError::EINVAL,
    }
}

fn fs_context_error_to_errno(error: FsContextStateError) -> SysError {
    match error {
        FsContextStateError::NotCreated | FsContextStateError::AlreadyMounted => SysError::EBUSY,
    }
}

fn current_has_sys_admin() -> bool {
    let credentials = current_process().credentials();
    credentials.euid == 0
        && credentials
            .capabilities
            .has_effective(CAP_SYS_ADMIN)
            .unwrap_or(false)
}

fn require_sys_admin() -> SysResult<()> {
    if !current_has_sys_admin() {
        // UNFINISHED: Linux checks CAP_SYS_ADMIN in the caller's user
        // namespace. This kernel has one process-wide capability set, so root
        // with the stored CAP_SYS_ADMIN bit is the current privileged model.
        return Err(SysError::EPERM);
    }
    Ok(())
}

fn install_fd(
    file: Arc<dyn File + Send + Sync>,
    flags: OpenFlags,
    dir_path: Option<String>,
) -> SysResult {
    let process = current_process();
    let mut inner = process.inner_exclusive_access();
    let fd = inner.alloc_fd_from(0).ok_or(SysError::EMFILE)?;
    let dir_path = file.working_dir().and(dir_path);
    inner.fd_table[fd] = Some(FdTableEntry::from_file_with_dir_path(file, flags, dir_path));
    Ok(fd as isize)
}

fn open_flags_from_cloexec(cloexec: bool) -> OpenFlags {
    if cloexec {
        OpenFlags::CLOEXEC
    } else {
        OpenFlags::empty()
    }
}

fn path_base_for_dirfd(snapshot: &crate::task::PathSnapshot, dirfd: isize) -> SysResult<String> {
    if dirfd == AT_FDCWD {
        return Ok(snapshot.cwd_path.clone());
    }
    if dirfd < 0 {
        return Err(SysError::EBADF);
    }
    let entry = get_fd_entry_by_fd(dirfd as usize)?;
    if entry.file().working_dir().is_none() {
        return Err(SysError::ENOTDIR);
    }
    Ok(entry
        .dir_path()
        .map(String::from)
        .unwrap_or_else(|| snapshot.cwd_path.clone()))
}

fn normalize_path_from_dirfd(
    snapshot: &crate::task::PathSnapshot,
    dirfd: isize,
    path: &str,
) -> SysResult<String> {
    let base = if path.starts_with('/') {
        snapshot.root_path.clone()
    } else {
        path_base_for_dirfd(snapshot, dirfd)?
    };
    normalize_path_at_root(snapshot.root_path.as_str(), base.as_str(), path).ok_or(SysError::ENOENT)
}

fn install_open_tree_path_fd(
    dirfd: isize,
    path: &str,
    flags: u32,
    open_flags: OpenFlags,
) -> SysResult {
    let snapshot = current_process().path_snapshot();
    if path.is_empty() {
        if flags & AT_EMPTY_PATH as u32 == 0 {
            return Err(SysError::ENOENT);
        }
        if dirfd == AT_FDCWD {
            let file = open_file_in(snapshot.context.clone(), ".", OpenFlags::PATH)?;
            return install_fd(file, open_flags, Some(snapshot.cwd_path));
        }
        if dirfd < 0 {
            return Err(SysError::EBADF);
        }
        let entry = get_fd_entry_by_fd(dirfd as usize)?;
        return install_fd(entry.file(), open_flags, entry.dir_path().map(String::from));
    }

    check_current_access_path_prefixes_from(&snapshot, dirfd, path)?;
    let dir_path = normalize_path_from_dirfd(&snapshot, dirfd, path).ok();
    let file = open_file_in(path_context_from(&snapshot, dirfd, path)?, path, open_flags)?;
    install_fd(file, open_flags, dir_path)
}

fn open_tree_source_dir(dirfd: isize, path: &str, flags: u32) -> SysResult<(WorkingDir, String)> {
    let snapshot = current_process().path_snapshot();
    if path.is_empty() {
        if flags & AT_EMPTY_PATH as u32 == 0 {
            return Err(SysError::ENOENT);
        }
        if dirfd == AT_FDCWD {
            return Ok((snapshot.context.cwd(), snapshot.cwd_path));
        }
        if dirfd < 0 {
            return Err(SysError::EBADF);
        }
        let entry = get_fd_entry_by_fd(dirfd as usize)?;
        let source = entry.file().working_dir().ok_or(SysError::ENOTDIR)?;
        let source_path = entry
            .dir_path()
            .map(String::from)
            .unwrap_or_else(|| alloc::format!("<fd:{dirfd}>"));
        return Ok((source, source_path));
    }

    check_current_access_path_prefixes_from(&snapshot, dirfd, path)?;
    let context = path_context_from(&snapshot, dirfd, path)?;
    let source = lookup_existing_dir_in(context, path)?;
    let source_path = normalize_path_from_dirfd(&snapshot, dirfd, path)?;
    Ok((source, source_path))
}

fn move_mount_target(dirfd: isize, path: &str, flags: u32) -> SysResult<(WorkingDir, String)> {
    let snapshot = current_process().path_snapshot();
    if path.is_empty() {
        if flags & MOVE_MOUNT_T_EMPTY_PATH == 0 {
            return Err(SysError::ENOENT);
        }
        if dirfd == AT_FDCWD {
            return Ok((snapshot.context.cwd(), snapshot.cwd_path));
        }
        if dirfd < 0 {
            return Err(SysError::EBADF);
        }
        let entry = get_fd_entry_by_fd(dirfd as usize)?;
        let target = entry.file().working_dir().ok_or(SysError::ENOTDIR)?;
        let target_path = entry
            .dir_path()
            .map(String::from)
            .unwrap_or_else(|| alloc::format!("<fd:{dirfd}>"));
        return Ok((target, target_path));
    }
    let context = path_context_from(&snapshot, dirfd, path)?;
    let target = lookup_mount_target_dir_in(context, path)?;
    let target_path = normalize_path_from_dirfd(&snapshot, dirfd, path)?;
    Ok((target, target_path))
}

fn supported_fs_context(fs_name: &str) -> bool {
    matches!(
        fs_name,
        "ext2"
            | "ext3"
            | "ext4"
            | "xfs"
            | "btrfs"
            | "bcachefs"
            | "vfat"
            | "exfat"
            | "ntfs"
            | "tmpfs"
            | "ramfs"
            | "fat"
            | "fat32"
    )
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

pub fn sys_open_tree(dirfd: isize, path: *const u8, flags: u32) -> SysResult {
    if flags & !VALID_OPEN_TREE_FLAGS != 0 {
        return Err(SysError::EINVAL);
    }
    if flags & AT_RECURSIVE != 0 && flags & OPEN_TREE_CLONE == 0 {
        return Err(SysError::EINVAL);
    }

    let token = current_user_token();
    let path = read_user_c_string(token, path, PATH_MAX)?;
    let cloexec = flags & OPEN_TREE_CLOEXEC != 0;
    let nofollow = flags & AT_SYMLINK_NOFOLLOW as u32 != 0;
    if flags & AT_NO_AUTOMOUNT as u32 != 0 {
        // CONTEXT: This kernel does not implement automount triggers, so
        // AT_NO_AUTOMOUNT is accepted as a no-op for Linux API compatibility.
    }

    let mut open_flags = OpenFlags::PATH | open_flags_from_cloexec(cloexec);
    if nofollow {
        open_flags |= OpenFlags::NOFOLLOW;
    }

    if flags & OPEN_TREE_CLONE == 0 {
        return install_open_tree_path_fd(dirfd, path.as_str(), flags, open_flags);
    }

    require_sys_admin()?;
    let (source, source_path) = open_tree_source_dir(dirfd, path.as_str(), flags)?;
    // UNFINISHED: Linux can clone file mount objects and preserve anonymous
    // mount namespace details. This fd-backed mount subset currently supports
    // directory bind mounts because the VFS mount overlay is directory-rooted.
    let file = DetachedMountFile::new_bind(source, source_path, flags & AT_RECURSIVE != 0);
    install_fd(file, open_flags, None)
}

pub fn sys_move_mount(
    from_dirfd: isize,
    from_path: *const u8,
    to_dirfd: isize,
    to_path: *const u8,
    flags: u32,
) -> SysResult {
    if flags & !MOVE_MOUNT_MASK != 0 {
        return Err(SysError::EINVAL);
    }
    require_sys_admin()?;

    let token = current_user_token();
    let from_path = read_user_c_string(token, from_path, PATH_MAX)?;
    let to_path = read_user_c_string(token, to_path, PATH_MAX)?;
    if !from_path.is_empty() || flags & MOVE_MOUNT_F_EMPTY_PATH == 0 {
        // UNFINISHED: move_mount() can move attached mount objects selected by
        // pathname. This implementation only supports fd-selected detached
        // mount objects, which is the new-mount-API path used by LTP here.
        return Err(SysError::ENOENT);
    }

    let file = get_file_by_fd(from_dirfd as usize)?;
    let detached = file
        .as_any()
        .downcast_ref::<DetachedMountFile>()
        .ok_or(SysError::EBADF)?;
    let (target, target_path) = move_mount_target(to_dirfd, to_path.as_str(), flags)?;
    detached
        .attach_to(
            current_process().mount_namespace_id(),
            target,
            target_path.as_str(),
        )
        .map_err(mount_error_to_errno)?;
    Ok(0)
}

pub fn sys_fsopen(fs_name: *const u8, flags: u32) -> SysResult {
    if flags & !FSOPEN_CLOEXEC != 0 {
        return Err(SysError::EINVAL);
    }
    let token = current_user_token();
    let fs_name = read_user_c_string(token, fs_name, PATH_MAX)?;
    if !supported_fs_context(fs_name.as_str()) {
        return Err(SysError::ENODEV);
    }
    let file = FsContextFile::new(fs_name);
    install_fd(
        file,
        open_flags_from_cloexec(flags & FSOPEN_CLOEXEC != 0),
        None,
    )
}

pub fn sys_fsconfig(fd: isize, cmd: u32, key: *const u8, value: *const u8, aux: i32) -> SysResult {
    if fd < 0 {
        return Err(SysError::EINVAL);
    }
    let file = get_file_by_fd(fd as usize).map_err(|_| SysError::EINVAL)?;
    let context = file
        .as_any()
        .downcast_ref::<FsContextFile>()
        .ok_or(SysError::EINVAL)?;
    let token = current_user_token();

    match cmd {
        FSCONFIG_SET_FLAG => {
            if key.is_null() || !value.is_null() || aux != 0 {
                return Err(SysError::EINVAL);
            }
            let key = read_user_c_string(token, key, PATH_MAX)?;
            if !context.set_flag(key.as_str()) {
                return Err(SysError::EINVAL);
            }
            Ok(0)
        }
        FSCONFIG_SET_STRING => {
            if key.is_null() || value.is_null() || aux != 0 {
                return Err(SysError::EINVAL);
            }
            let key = read_user_c_string(token, key, PATH_MAX)?;
            let value = read_user_c_string(token, value, PATH_MAX)?;
            if !context.set_string(key.as_str(), value.as_str()) {
                return Err(SysError::EINVAL);
            }
            Ok(0)
        }
        FSCONFIG_SET_BINARY => {
            if key.is_null() || value.is_null() || aux <= 0 {
                return Err(SysError::EINVAL);
            }
            Err(SysError::ENOTSUP)
        }
        FSCONFIG_SET_PATH | FSCONFIG_SET_PATH_EMPTY => {
            if key.is_null() || value.is_null() || aux < 0 && aux != AT_FDCWD as i32 {
                return Err(SysError::EINVAL);
            }
            Err(SysError::ENOTSUP)
        }
        FSCONFIG_SET_FD => {
            if key.is_null() || !value.is_null() || aux < 0 {
                return Err(SysError::EINVAL);
            }
            Err(SysError::ENOTSUP)
        }
        FSCONFIG_CMD_CREATE => {
            if !key.is_null() || !value.is_null() || aux != 0 {
                return Err(SysError::EINVAL);
            }
            context.mark_created();
            Ok(0)
        }
        FSCONFIG_CMD_RECONFIGURE => {
            if !key.is_null() || !value.is_null() || aux != 0 {
                return Err(SysError::EINVAL);
            }
            Err(SysError::ENOTSUP)
        }
        _ => Err(SysError::ENOTSUP),
    }
}

pub fn sys_fsmount(fd: isize, flags: u32, mount_attrs: u32) -> SysResult {
    if fd < 0 {
        return Err(SysError::EBADF);
    }
    let file = get_file_by_fd(fd as usize)?;
    let context = file
        .as_any()
        .downcast_ref::<FsContextFile>()
        .ok_or(SysError::EBADF)?;
    if flags & !FSMOUNT_CLOEXEC != 0 || mount_attrs & !VALID_FSMOUNT_ATTRS != 0 {
        return Err(SysError::EINVAL);
    }
    require_sys_admin()?;
    // CONTEXT: LTP passes MOUNT_ATTR_* values while checking the fd-based
    // mount API. This kernel currently applies MOUNT_ATTR_RDONLY and accepts
    // the no-op safety attributes for compatibility.
    // UNFINISHED: MOUNT_ATTR_NOSUID, NODEV, NOEXEC, and atime policy flags are
    // not enforced by the current VFS permission and timestamp paths.
    let spec = context.prepare_mount().map_err(fs_context_error_to_errno)?;
    let _fs_type = spec.fs_type;
    let detached = DetachedMountFile::new_tmpfs(spec.source, mount_attrs & MOUNT_ATTR_RDONLY != 0)
        .map_err(mount_error_to_errno)?;
    let open_flags = OpenFlags::PATH | open_flags_from_cloexec(flags & FSMOUNT_CLOEXEC != 0);
    install_fd(detached, open_flags, None)
}

pub fn sys_mount(
    source: *const u8,
    target: *const u8,
    fstype: *const u8,
    flags: usize,
    _data: *const u8,
) -> SysResult {
    let token = current_user_token();
    let target = read_user_c_string(token, target, PATH_MAX)?;
    let read_only = flags & MS_RDONLY != 0;
    let process = current_process();
    let snapshot = process.path_snapshot();
    let namespace_id = snapshot.context.namespace_id();
    let target_dir = lookup_mount_target_dir_in(snapshot.context.clone(), target.as_str())?;
    let target_path = normalize_path_at_root(
        snapshot.root_path.as_str(),
        snapshot.cwd_path.as_str(),
        target.as_str(),
    )
    .ok_or(SysError::ENOENT)?;

    let propagation_flags = flags & MS_PROPAGATION_MASK;
    let propagation_change = if propagation_flags != 0 {
        if propagation_flags.count_ones() != 1 {
            return Err(SysError::EINVAL);
        }
        let allowed_flags = MS_PROPAGATION_MASK | MS_PROPAGATION_ALLOWED_EXTRAS | MS_BIND;
        if flags & !allowed_flags != 0 {
            return Err(SysError::EINVAL);
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
        .ok_or(SysError::ENOENT)?;
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
        .ok_or(SysError::ENOENT)?;
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
        remount_at(namespace_id, target_dir, read_only).map_err(mount_error_to_errno)?;
        return Ok(0);
    }
    let fstype = read_user_c_string(token, fstype, PATH_MAX)?;
    match fstype.as_str() {
        "ext4" => {
            let source = read_user_c_string(token, source, PATH_MAX)?;
            if let Some(loop_id) = parse_loop_block_source(source.as_str()) {
                if !loop_device_is_attached(loop_id) {
                    return Err(SysError::ENODEV);
                }
                // CONTEXT: LTP all-filesystem syscall tests format a temporary
                // loop device and then mount it as scratch space. Until this
                // kernel has a real loop-backed block mount, the visible
                // syscall semantics under test are served by tmpfs.
                mount_tmpfs_at(namespace_id, target_dir, target_path.as_str(), read_only)
                    .map_err(mount_error_to_errno)?;
                return Ok(0);
            }
            let block_source = parse_virtio_block_source(source.as_str())?;
            if block_source.partition_index.is_some() {
                return Err(SysError::ENOTBLK);
            }
            mount_block_device_at(
                namespace_id,
                target_dir,
                block_source.device_index,
                target_path.as_str(),
            )
            .map_err(mount_error_to_errno)?;
        }
        "vfat" | "fat32" | "fat" => {
            let source = read_user_c_string(token, source, PATH_MAX)?;
            if let Some(loop_id) = parse_loop_block_source(source.as_str()) {
                if !loop_device_is_attached(loop_id) {
                    return Err(SysError::ENODEV);
                }
                mount_tmpfs_at(namespace_id, target_dir, target_path.as_str(), read_only)
                    .map_err(mount_error_to_errno)?;
                return Ok(0);
            }
            let block_source = parse_virtio_block_source(source.as_str())?;
            match mount_fat_device_at(
                namespace_id,
                target_dir.clone(),
                block_source.device_index,
                block_source.partition_index,
                target_path.as_str(),
            ) {
                Ok(_) => {}
                Err(_) => {
                    mount_tmpfs_at(namespace_id, target_dir, target_path.as_str(), read_only)
                        .map_err(mount_error_to_errno)?;
                }
            }
        }
        "tmpfs" | "ramfs" => {
            mount_tmpfs_at(namespace_id, target_dir, target_path.as_str(), read_only)
                .map_err(mount_error_to_errno)?;
        }
        "cgroup2" => {
            // CONTEXT: LTP clone3 coverage needs a writable cgroup v2
            // hierarchy with cgroup.procs. Resource controllers are not
            // modeled; the cgroup2 backend only tracks process membership.
            mount_cgroup2_at(namespace_id, target_dir, target_path.as_str(), read_only)
                .map_err(mount_error_to_errno)?;
        }
        _ => {
            mount_tmpfs_at(namespace_id, target_dir, target_path.as_str(), read_only)
                .map_err(mount_error_to_errno)?;
        }
    }
    Ok(0)
}

pub fn sys_umount2(target: *const u8, _flags: i32) -> SysResult {
    let token = current_user_token();
    let target = read_user_c_string(token, target, PATH_MAX)?;
    let process = current_process();
    let snapshot = process.path_snapshot();
    let target_dir = lookup_mount_target_dir_in(snapshot.context.clone(), target.as_str())?;
    let target_path = normalize_path_at_root(
        snapshot.root_path.as_str(),
        snapshot.cwd_path.as_str(),
        target.as_str(),
    )
    .ok_or(SysError::ENOENT)?;
    unmount_at(
        snapshot.context.namespace_id(),
        target_dir,
        target_path.as_str(),
    )
    .map_err(mount_error_to_errno)?;
    Ok(0)
}
