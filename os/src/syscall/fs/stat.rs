use crate::fs::{
    FileStat, FileSystemStat, FsNodeKind, MountId, OpenFlags, S_IFBLK, S_IFCHR, S_IFDIR, S_IFIFO,
    S_IFLNK, S_IFMT, S_IFREG, S_IFSOCK, VfsNodeId, chmod_in, chown_in, lookup_path_in,
    mount_is_read_only, nfs_compat_source_path, open_file_in, stat_devfs_child,
    stat_devfs_input_child, stat_devfs_misc_child, stat_devfs_net_child, stat_devfs_pts_child,
    stat_in, stat_static_path, statfs_for_mount,
};
use crate::sync::SleepMutex;
use crate::task::{PathSnapshot, current_process, current_user_token};

use super::super::SyscallContext;
use super::super::errno::{SysError, SysResult};
use super::super::user_ptr::{
    PATH_MAX, UserBufferAccess, copy_to_user, read_user_c_string, read_user_c_string_ctx,
    translated_byte_buffer_checked_with_mmap_fault, write_user_value_ctx,
};
use super::fanotify::fanotify_notify_attrib;
use super::fd::{get_fd_entry_by_fd, get_fd_entry_by_fd_for_process, get_file_by_fd};
use super::inotify::inotify_notify_attrib;
use super::path::{
    AtPath, check_access_path_prefixes_for_process, check_current_access_path_prefixes_from,
    normalize_path_from, path_context_from, resolve_at_path,
};
use super::uapi::{
    AT_EMPTY_PATH, AT_FDCWD, AT_STATX_DONT_SYNC, AT_STATX_FORCE_SYNC, AT_SYMLINK_NOFOLLOW,
    LinuxKstat, LinuxStatfs, LinuxStatx, STATX_RESERVED, VALID_FCHOWNAT_FLAGS, VALID_FSTATAT_FLAGS,
    VALID_STATX_FLAGS,
};
use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec::Vec;
use lazy_static::lazy_static;

const UID_GID_NO_CHANGE: u32 = u32::MAX;
const MODE_SETUID: u32 = 0o4000;
const MODE_SETGID: u32 = 0o2000;
const MODE_GROUP_EXEC: u32 = 0o0010;
const XATTR_NAME_MAX: usize = 255;
const XATTR_SIZE_MAX: usize = 64 * 1024;
const XATTR_LIST_MAX: usize = 64 * 1024;
const XATTR_CREATE: u32 = 1;
const XATTR_REPLACE: u32 = 2;
const PIPEFS_MAGIC: i64 = 0x5049_5045;

lazy_static! {
    static ref XATTRS: SleepMutex<BTreeMap<(VfsNodeId, String), Vec<u8>>> =
        SleepMutex::new(BTreeMap::new());
    static ref NFS_STATX_CACHE: SleepMutex<BTreeMap<(crate::fs::MountNamespaceId, String), FileStat>> =
        SleepMutex::new(BTreeMap::new());
}

#[derive(Clone, Copy)]
struct XattrTarget {
    node: VfsNodeId,
    kind: FsNodeKind,
}

fn write_stat_result_ctx<T: From<FileStat> + Copy>(
    ctx: &SyscallContext,
    buf: *mut T,
    stat: FileStat,
) -> SysResult {
    write_user_value_ctx(ctx, buf, &stat.into())?;
    Ok(0)
}

fn reject_proc_self_fd_o_path(path: &str) -> SysResult<()> {
    let Some(fd_text) = path.strip_prefix("/proc/self/fd/") else {
        return Ok(());
    };
    if fd_text.is_empty() || fd_text.contains('/') {
        return Ok(());
    }
    let Ok(fd) = fd_text.parse::<usize>() else {
        return Ok(());
    };
    let entry = get_fd_entry_by_fd(fd)?;
    if entry.status_flags().contains(OpenFlags::PATH) {
        return Err(SysError::EBADF);
    }
    Ok(())
}

pub(super) fn resolve_stat_from(
    snapshot: &PathSnapshot,
    dirfd: isize,
    path: &str,
    follow_final_symlink: bool,
) -> SysResult<FileStat> {
    let path = match resolve_at_path(snapshot, dirfd, path, true)? {
        AtPath::Empty(empty) => return Ok(empty.file().stat()?),
        AtPath::Path(path) => path,
    };
    let is_absolute = path.starts_with('/');
    if !is_absolute && dirfd != AT_FDCWD && dirfd >= 0 {
        let file = get_file_by_fd(dirfd as usize)?;
        if file.is_devfs_dir() {
            let stat = if file.is_devfs_misc_dir() {
                stat_devfs_misc_child(path)
            } else if file.is_devfs_input_dir() {
                stat_devfs_input_child(path)
            } else if file.is_devfs_net_dir() {
                stat_devfs_net_child(path)
            } else if file.is_devfs_pts_dir() {
                stat_devfs_pts_child(path)
            } else {
                stat_devfs_child(path)
            };
            return stat.ok_or(SysError::ENOENT);
        }
    }
    if let Ok(global_path) = normalize_path_from(snapshot, dirfd, path)
        && let Some(stat) = stat_static_path(global_path.as_str())
    {
        return Ok(stat);
    }
    Ok(stat_in(
        path_context_from(snapshot, dirfd, path)?,
        path,
        follow_final_symlink,
    )?)
}

pub fn sys_fstat_ctx(ctx: &SyscallContext, fd: usize, statbuf: *mut LinuxKstat) -> SysResult {
    if statbuf.is_null() {
        return Err(SysError::EFAULT);
    }
    let file = get_file_by_fd(fd)?;
    write_stat_result_ctx(ctx, statbuf, file.stat()?)
}

pub fn sys_newfstatat_ctx(
    ctx: &SyscallContext,
    dirfd: isize,
    pathname: *const u8,
    statbuf: *mut LinuxKstat,
    flags: i32,
) -> SysResult {
    if statbuf.is_null() || pathname.is_null() {
        return Err(SysError::EFAULT);
    }
    if flags & !VALID_FSTATAT_FLAGS != 0 {
        return Err(SysError::EINVAL);
    }

    let path = read_user_c_string_ctx(ctx, pathname, PATH_MAX)?;
    if path.is_empty() && flags & AT_EMPTY_PATH == 0 {
        return Err(SysError::ENOENT);
    }
    let follow_final_symlink = flags & AT_SYMLINK_NOFOLLOW == 0;
    let snapshot = ctx.process().path_snapshot();
    if !path.is_empty() {
        check_access_path_prefixes_for_process(ctx.process(), &snapshot, dirfd, path.as_str())?;
    }
    write_stat_result_ctx(
        ctx,
        statbuf,
        resolve_stat_from(&snapshot, dirfd, path.as_str(), follow_final_symlink)?,
    )
}

fn prepare_mode_change(stat: FileStat, mode: u32) -> SysResult<u32> {
    if mount_is_read_only(MountId(stat.dev as usize)) {
        return Err(SysError::EROFS);
    }
    let credentials = current_process().credentials();
    // UNFINISHED: Linux chmod checks CAP_FOWNER and filesystem uid in the
    // caller's user namespace. This kernel only has root-equivalent uid 0 plus
    // stored fsuid.
    if credentials.euid != 0 && credentials.fsuid != stat.uid {
        return Err(SysError::EPERM);
    }
    let mut mode = mode;
    if mode & MODE_SETGID != 0
        && credentials.euid != 0
        && credentials.egid != stat.gid
        && credentials.fsgid != stat.gid
        && !credentials.groups.contains(&stat.gid)
    {
        mode &= !MODE_SETGID;
    }
    Ok(mode)
}

fn ensure_can_change_owner(stat: FileStat, uid: Option<u32>, gid: Option<u32>) -> SysResult<()> {
    let credentials = current_process().credentials();
    if credentials.euid == 0 {
        return Ok(());
    }
    if uid.is_none() && gid.is_none() {
        return Ok(());
    }
    if uid.is_none()
        && stat.uid == credentials.fsuid
        && let Some(group) = gid
        && (group == credentials.egid
            || group == credentials.fsgid
            || credentials.groups.contains(&group))
    {
        return Ok(());
    }
    Err(SysError::EPERM)
}

fn mode_after_chown(stat: FileStat, uid: Option<u32>, gid: Option<u32>) -> Option<u32> {
    if uid.is_none() && gid.is_none() {
        return None;
    }
    let mut mode = stat.mode;
    mode &= !MODE_SETUID;
    if mode & MODE_GROUP_EXEC != 0 {
        mode &= !MODE_SETGID;
    }
    (mode != stat.mode).then_some(mode)
}

fn prepare_owner_change(stat: FileStat, uid: Option<u32>, gid: Option<u32>) -> SysResult<()> {
    if mount_is_read_only(MountId(stat.dev as usize)) {
        return Err(SysError::EROFS);
    }
    ensure_can_change_owner(stat, uid, gid)
}

fn finish_file_owner_change(
    file: &dyn crate::fs::File,
    stat: FileStat,
    uid: Option<u32>,
    gid: Option<u32>,
) -> SysResult {
    prepare_owner_change(stat, uid, gid)?;
    file.set_owner(uid, gid)?;
    if let Some(mode) = mode_after_chown(stat, uid, gid) {
        file.set_mode(mode)?;
    }
    Ok(0)
}

fn finish_path_owner_change(
    snapshot: &PathSnapshot,
    dirfd: isize,
    path: &str,
    follow_final_symlink: bool,
    stat: FileStat,
    uid: Option<u32>,
    gid: Option<u32>,
) -> SysResult {
    prepare_owner_change(stat, uid, gid)?;
    let context = path_context_from(snapshot, dirfd, path)?;
    chown_in(context.clone(), path, follow_final_symlink, uid, gid)?;
    if let Some(mode) = mode_after_chown(stat, uid, gid) {
        chmod_in(context, path, follow_final_symlink, mode)?;
    }
    Ok(0)
}

pub fn sys_fchmodat(dirfd: isize, pathname: *const u8, mode: u32) -> SysResult {
    if pathname.is_null() {
        return Err(SysError::EFAULT);
    }
    let token = current_user_token();
    let path = read_user_c_string(token, pathname, PATH_MAX)?;
    if path.is_empty() {
        if dirfd >= 0
            && let Ok(entry) = get_fd_entry_by_fd(dirfd as usize)
            && entry.status_flags().contains(OpenFlags::PATH)
        {
            return Err(SysError::EBADF);
        }
        return Err(SysError::ENOENT);
    }
    reject_proc_self_fd_o_path(path.as_str())?;
    let snapshot = current_process().path_snapshot();
    check_current_access_path_prefixes_from(&snapshot, dirfd, path.as_str())?;
    let stat = resolve_stat_from(&snapshot, dirfd, path.as_str(), true)?;
    let mode = prepare_mode_change(stat, mode)?;
    // UNFINISHED: Linux clears setuid bits in additional cases depending on
    // capabilities and executable file state. This kernel implements the LTP
    // visible setgid clearing rule but still lacks full capability handling.
    let context = path_context_from(&snapshot, dirfd, path.as_str())?;
    chmod_in(context.clone(), path.as_str(), true, mode)?;
    if let Ok(file) = open_file_in(context, path.as_str(), OpenFlags::PATH) {
        fanotify_notify_attrib(&file);
        inotify_notify_attrib(&file);
    }
    Ok(0)
}

pub fn sys_fchmod(fd: usize, mode: u32) -> SysResult {
    let entry = get_fd_entry_by_fd(fd)?;
    if entry.status_flags().contains(OpenFlags::PATH) {
        return Err(SysError::EBADF);
    }
    let file = entry.file();
    let stat = file.stat()?;
    let mode = prepare_mode_change(stat, mode)?;
    file.set_mode(mode)?;
    fanotify_notify_attrib(&file);
    inotify_notify_attrib(&file);
    Ok(0)
}

fn decode_chown_id(raw: u32) -> Option<u32> {
    (raw != UID_GID_NO_CHANGE).then_some(raw)
}

pub fn sys_fchown(fd: usize, owner: u32, group: u32) -> SysResult {
    let uid = decode_chown_id(owner);
    let gid = decode_chown_id(group);
    let entry = get_fd_entry_by_fd(fd)?;
    if entry.status_flags().contains(OpenFlags::PATH) {
        return Err(SysError::EBADF);
    }
    let file = entry.file();
    let stat = file.stat()?;
    finish_file_owner_change(file.as_ref(), stat, uid, gid)
}

pub fn sys_fchownat(
    dirfd: isize,
    pathname: *const u8,
    owner: u32,
    group: u32,
    flags: i32,
) -> SysResult {
    if pathname.is_null() {
        return Err(SysError::EFAULT);
    }
    if flags & !VALID_FCHOWNAT_FLAGS != 0 {
        return Err(SysError::EINVAL);
    }
    let uid = decode_chown_id(owner);
    let gid = decode_chown_id(group);
    let token = current_user_token();
    let path = read_user_c_string(token, pathname, PATH_MAX)?;
    reject_proc_self_fd_o_path(path.as_str())?;
    let follow_final_symlink = flags & AT_SYMLINK_NOFOLLOW == 0;
    let snapshot = current_process().path_snapshot();

    if path.is_empty() {
        if flags & AT_EMPTY_PATH == 0 {
            return Err(SysError::ENOENT);
        }
        if dirfd == AT_FDCWD {
            let stat = stat_in(snapshot.context.clone(), ".", follow_final_symlink)?;
            return finish_path_owner_change(
                &snapshot,
                dirfd,
                ".",
                follow_final_symlink,
                stat,
                uid,
                gid,
            );
        }
        if dirfd < 0 {
            return Err(SysError::EBADF);
        }
        let entry = get_fd_entry_by_fd(dirfd as usize)?;
        if entry.status_flags().contains(OpenFlags::PATH) {
            return Err(SysError::EBADF);
        }
        let file = entry.file();
        let stat = file.stat()?;
        return finish_file_owner_change(file.as_ref(), stat, uid, gid);
    }

    check_current_access_path_prefixes_from(&snapshot, dirfd, path.as_str())?;
    let stat = resolve_stat_from(&snapshot, dirfd, path.as_str(), follow_final_symlink)?;
    finish_path_owner_change(
        &snapshot,
        dirfd,
        path.as_str(),
        follow_final_symlink,
        stat,
        uid,
        gid,
    )
}

fn read_xattr_name(token: usize, name: *const u8) -> SysResult<String> {
    let name = read_user_c_string(token, name, XATTR_NAME_MAX + 1)?;
    if !xattr_name_supported(name.as_str()) {
        return Err(SysError::ENOTSUP);
    }
    Ok(name)
}

fn read_xattr_value(token: usize, value: *const u8, size: usize) -> SysResult<Vec<u8>> {
    if size > XATTR_SIZE_MAX {
        return Err(SysError::ERANGE);
    }
    if size == 0 {
        return Ok(Vec::new());
    }
    if value.is_null() {
        return Err(SysError::EFAULT);
    }
    let buffers =
        translated_byte_buffer_checked_with_mmap_fault(token, value, size, UserBufferAccess::Read)?;
    let mut bytes = Vec::with_capacity(size);
    for buffer in buffers {
        bytes.extend_from_slice(buffer);
    }
    Ok(bytes)
}

fn xattr_name_supported(name: &str) -> bool {
    matches!(
        name.split_once('.'),
        Some(("user" | "trusted" | "security" | "system", suffix)) if !suffix.is_empty()
    )
}

fn xattr_user_namespace_allowed(kind: FsNodeKind) -> bool {
    matches!(kind, FsNodeKind::RegularFile | FsNodeKind::Directory)
}

fn xattr_kind_from_mode(mode: u32) -> FsNodeKind {
    match mode & S_IFMT {
        S_IFDIR => FsNodeKind::Directory,
        S_IFREG => FsNodeKind::RegularFile,
        S_IFLNK => FsNodeKind::Symlink,
        S_IFIFO => FsNodeKind::Fifo,
        S_IFCHR => FsNodeKind::CharacterDevice,
        S_IFBLK => FsNodeKind::BlockDevice,
        S_IFSOCK => FsNodeKind::Socket,
        _ => FsNodeKind::Other,
    }
}

fn xattr_target_from_path(path: *const u8, follow_final_symlink: bool) -> SysResult<XattrTarget> {
    let token = current_user_token();
    let path = read_user_c_string(token, path, PATH_MAX)?;
    if path.is_empty() {
        return Err(SysError::ENOENT);
    }
    let snapshot = current_process().path_snapshot();
    let context = path_context_from(&snapshot, AT_FDCWD, path.as_str())?;
    let resolved = lookup_path_in(context, path.as_str(), follow_final_symlink)?;
    Ok(XattrTarget {
        node: resolved.node,
        kind: resolved.kind,
    })
}

fn xattr_target_from_fd(fd: usize) -> SysResult<XattrTarget> {
    let entry = get_fd_entry_by_fd(fd)?;
    if entry.status_flags().contains(OpenFlags::PATH) {
        return Err(SysError::EBADF);
    }
    let file = entry.file();
    let node = file.vfs_node_id().ok_or(SysError::ENOTSUP)?;
    let stat = file.stat()?;
    Ok(XattrTarget {
        node,
        kind: xattr_kind_from_mode(stat.mode),
    })
}

fn xattr_get(target: XattrTarget, name: &str, value: *mut u8, size: usize) -> SysResult {
    let token = current_user_token();
    if name.starts_with("user.") && !xattr_user_namespace_allowed(target.kind) {
        return Err(SysError::ENODATA);
    }
    let key = (target.node, String::from(name));
    let attrs = XATTRS.lock();
    let stored = attrs.get(&key).ok_or(SysError::ENODATA)?;
    if size == 0 {
        return Ok(stored.len() as isize);
    }
    if value.is_null() {
        return Err(SysError::EFAULT);
    }
    if size < stored.len() {
        return Err(SysError::ERANGE);
    }
    copy_to_user(token, value, stored)?;
    Ok(stored.len() as isize)
}

fn copy_xattr_list_to_user(token: usize, list: *mut u8, bytes: &[u8]) -> SysResult {
    if bytes.is_empty() {
        return Ok(0);
    }
    let buffers = translated_byte_buffer_checked_with_mmap_fault(
        token,
        list.cast_const(),
        bytes.len(),
        UserBufferAccess::Write,
    )?;
    let mut copied = 0usize;
    for buffer in buffers {
        let remaining = bytes.len() - copied;
        let count = buffer.len().min(remaining);
        buffer[..count].copy_from_slice(&bytes[copied..copied + count]);
        copied += count;
        if copied == bytes.len() {
            break;
        }
    }
    Ok(0)
}

fn xattr_list_bytes(target: XattrTarget) -> SysResult<Vec<u8>> {
    let attrs = XATTRS.lock();
    let mut list = Vec::new();
    for ((node, name), _) in attrs.iter() {
        if *node != target.node {
            continue;
        }
        if name.starts_with("user.") && !xattr_user_namespace_allowed(target.kind) {
            continue;
        }
        let entry_len = name.len().checked_add(1).ok_or(SysError::E2BIG)?;
        if list.len().checked_add(entry_len).ok_or(SysError::E2BIG)? > XATTR_LIST_MAX {
            return Err(SysError::E2BIG);
        }
        list.extend_from_slice(name.as_bytes());
        list.push(0);
    }
    Ok(list)
}

fn xattr_list(target: XattrTarget, list: *mut u8, size: usize) -> SysResult {
    let bytes = xattr_list_bytes(target)?;
    if size == 0 {
        return Ok(bytes.len() as isize);
    }
    if list.is_null() {
        return Err(SysError::EFAULT);
    }
    if size < bytes.len() {
        return Err(SysError::ERANGE);
    }
    copy_xattr_list_to_user(current_user_token(), list, bytes.as_slice())?;
    Ok(bytes.len() as isize)
}

fn xattr_set(target: XattrTarget, name: &str, value: Vec<u8>, flags: u32) -> SysResult {
    if flags & !(XATTR_CREATE | XATTR_REPLACE) != 0 || flags == (XATTR_CREATE | XATTR_REPLACE) {
        return Err(SysError::EINVAL);
    }
    if name.starts_with("user.") && !xattr_user_namespace_allowed(target.kind) {
        return Err(SysError::ENOTSUP);
    }
    // UNFINISHED: xattrs are kept in a kernel in-memory VFS side table. They
    // are enough for one-boot LTP syscall semantics but are not persisted into
    // EXT4/TMPFS backing storage and are not reclaimed on every inode reuse.
    let key = (target.node, String::from(name));
    let mut attrs = XATTRS.lock();
    let exists = attrs.contains_key(&key);
    if flags & XATTR_CREATE != 0 && exists {
        return Err(SysError::EEXIST);
    }
    if flags & XATTR_REPLACE != 0 && !exists {
        return Err(SysError::ENODATA);
    }
    attrs.insert(key, value);
    Ok(0)
}

fn xattr_remove(target: XattrTarget, name: &str) -> SysResult {
    let key = (target.node, String::from(name));
    if XATTRS.lock().remove(&key).is_some() {
        Ok(0)
    } else {
        Err(SysError::ENODATA)
    }
}

pub fn sys_setxattr(
    path: *const u8,
    name: *const u8,
    value: *const u8,
    size: usize,
    flags: u32,
) -> SysResult {
    let token = current_user_token();
    let name = read_xattr_name(token, name)?;
    let value = read_xattr_value(token, value, size)?;
    let target = xattr_target_from_path(path, true)?;
    xattr_set(target, name.as_str(), value, flags)
}

pub fn sys_lsetxattr(
    path: *const u8,
    name: *const u8,
    value: *const u8,
    size: usize,
    flags: u32,
) -> SysResult {
    let token = current_user_token();
    let name = read_xattr_name(token, name)?;
    let value = read_xattr_value(token, value, size)?;
    let target = xattr_target_from_path(path, false)?;
    xattr_set(target, name.as_str(), value, flags)
}

pub fn sys_fsetxattr(
    fd: usize,
    name: *const u8,
    value: *const u8,
    size: usize,
    flags: u32,
) -> SysResult {
    let token = current_user_token();
    let name = read_xattr_name(token, name)?;
    let value = read_xattr_value(token, value, size)?;
    let target = xattr_target_from_fd(fd)?;
    xattr_set(target, name.as_str(), value, flags)
}

pub fn sys_getxattr(path: *const u8, name: *const u8, value: *mut u8, size: usize) -> SysResult {
    let token = current_user_token();
    let name = read_xattr_name(token, name)?;
    let target = xattr_target_from_path(path, true)?;
    xattr_get(target, name.as_str(), value, size)
}

pub fn sys_lgetxattr(path: *const u8, name: *const u8, value: *mut u8, size: usize) -> SysResult {
    let token = current_user_token();
    let name = read_xattr_name(token, name)?;
    let target = xattr_target_from_path(path, false)?;
    xattr_get(target, name.as_str(), value, size)
}

pub fn sys_fgetxattr(fd: usize, name: *const u8, value: *mut u8, size: usize) -> SysResult {
    let token = current_user_token();
    let name = read_xattr_name(token, name)?;
    let target = xattr_target_from_fd(fd)?;
    xattr_get(target, name.as_str(), value, size)
}

pub fn sys_listxattr(path: *const u8, list: *mut u8, size: usize) -> SysResult {
    let target = xattr_target_from_path(path, true)?;
    xattr_list(target, list, size)
}

pub fn sys_llistxattr(path: *const u8, list: *mut u8, size: usize) -> SysResult {
    let target = xattr_target_from_path(path, false)?;
    xattr_list(target, list, size)
}

pub fn sys_flistxattr(fd: usize, list: *mut u8, size: usize) -> SysResult {
    let target = xattr_target_from_fd(fd)?;
    xattr_list(target, list, size)
}

pub fn sys_removexattr(path: *const u8, name: *const u8) -> SysResult {
    let token = current_user_token();
    let name = read_xattr_name(token, name)?;
    let target = xattr_target_from_path(path, true)?;
    xattr_remove(target, name.as_str())
}

pub fn sys_lremovexattr(path: *const u8, name: *const u8) -> SysResult {
    let token = current_user_token();
    let name = read_xattr_name(token, name)?;
    let target = xattr_target_from_path(path, false)?;
    xattr_remove(target, name.as_str())
}

pub fn sys_fremovexattr(fd: usize, name: *const u8) -> SysResult {
    let token = current_user_token();
    let name = read_xattr_name(token, name)?;
    let target = xattr_target_from_fd(fd)?;
    xattr_remove(target, name.as_str())
}

pub fn sys_statfs_ctx(
    ctx: &SyscallContext,
    pathname: *const u8,
    statfsbuf: *mut LinuxStatfs,
) -> SysResult {
    if statfsbuf.is_null() || pathname.is_null() {
        return Err(SysError::EFAULT);
    }
    let path = read_user_c_string_ctx(ctx, pathname, PATH_MAX)?;
    if path.is_empty() {
        return Err(SysError::ENOENT);
    }
    let snapshot = ctx.process().path_snapshot();
    check_access_path_prefixes_for_process(ctx.process(), &snapshot, AT_FDCWD, path.as_str())?;
    let stat = resolve_stat_from(&snapshot, AT_FDCWD, path.as_str(), true)?;
    let fs_stat = statfs_for_mount(MountId(stat.dev as usize)).ok_or(SysError::ENOSYS)?;
    write_user_value_ctx(ctx, statfsbuf, &LinuxStatfs::from(fs_stat))?;
    Ok(0)
}

pub fn sys_fstatfs_ctx(ctx: &SyscallContext, fd: usize, statfsbuf: *mut LinuxStatfs) -> SysResult {
    let entry = get_fd_entry_by_fd_for_process(ctx.process(), fd)?;
    let stat = entry.file().stat()?;
    let fs_stat = statfs_for_mount(MountId(stat.dev as usize)).unwrap_or_else(anonymous_fd_statfs);
    write_user_value_ctx(ctx, statfsbuf, &LinuxStatfs::from(fs_stat))?;
    Ok(0)
}

fn anonymous_fd_statfs() -> FileSystemStat {
    // CONTEXT: Anonymous file descriptors such as pipes are not backed by a
    // mounted VFS object in this kernel, but Linux still lets fstatfs() report
    // synthetic pipefs-style statistics for them.
    FileSystemStat {
        magic: PIPEFS_MAGIC,
        block_size: 4096,
        blocks: 0,
        free_blocks: 0,
        available_blocks: 0,
        files: 1024,
        free_files: 1024,
        max_name_len: 255,
        flags: 0,
    }
}

pub fn sys_statx_ctx(
    ctx: &SyscallContext,
    dirfd: isize,
    pathname: *const u8,
    flags: i32,
    mask: u32,
    statxbuf: *mut LinuxStatx,
) -> SysResult {
    if statxbuf.is_null() || pathname.is_null() {
        return Err(SysError::EFAULT);
    }
    if flags & !VALID_STATX_FLAGS != 0 || mask & STATX_RESERVED != 0 {
        return Err(SysError::EINVAL);
    }

    let path = read_user_c_string_ctx(ctx, pathname, PATH_MAX)?;
    if path.is_empty() && flags & AT_EMPTY_PATH == 0 {
        return Err(SysError::ENOENT);
    }
    let follow_final_symlink = flags & AT_SYMLINK_NOFOLLOW == 0;
    let snapshot = ctx.process().path_snapshot();
    if !path.is_empty() {
        check_access_path_prefixes_for_process(ctx.process(), &snapshot, dirfd, path.as_str())?;
    }
    let stat = resolve_statx_stat(&snapshot, dirfd, path.as_str(), flags, follow_final_symlink)?;
    write_stat_result_ctx(ctx, statxbuf, stat)
}

fn resolve_statx_stat(
    snapshot: &PathSnapshot,
    dirfd: isize,
    path: &str,
    flags: i32,
    follow_final_symlink: bool,
) -> SysResult<FileStat> {
    // CONTEXT: Linux AT_STATX_FORCE_SYNC / AT_STATX_DONT_SYNC are cache
    // coherency hints. Local contest filesystems resolve stat synchronously;
    // only the NFS compatibility mount has a visible stat cache.
    if !path.is_empty()
        && flags & (AT_STATX_FORCE_SYNC | AT_STATX_DONT_SYNC) != 0
        && let Ok(client_path) = normalize_path_from(snapshot, dirfd, path)
        && let Some(source_path) =
            nfs_compat_source_path(snapshot.context.namespace_id(), client_path.as_str())
    {
        let cache_key = (snapshot.context.namespace_id(), client_path);
        if flags & AT_STATX_DONT_SYNC != 0
            && let Some(stat) = NFS_STATX_CACHE.lock().get(&cache_key).copied()
        {
            return Ok(stat);
        }
        let stat = resolve_stat_from(
            snapshot,
            AT_FDCWD,
            source_path.as_str(),
            follow_final_symlink,
        )?;
        NFS_STATX_CACHE.lock().insert(cache_key, stat);
        return Ok(stat);
    }

    resolve_stat_from(snapshot, dirfd, path, follow_final_symlink)
}
