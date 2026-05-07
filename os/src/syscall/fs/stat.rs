use crate::fs::{
    chmod_in, chown_in, stat_devfs_child, stat_devfs_misc_child, stat_in, stat_static_path,
    statfs_for_mount, FileStat, MountId,
};
use crate::task::{current_process, current_user_token, PathSnapshot};

use super::super::errno::{SysError, SysResult};
use super::super::user_ptr::{read_user_c_string, write_user_value, PATH_MAX};
use super::fd::get_file_by_fd;
use super::path::path_context_from;
use super::uapi::{
    LinuxKstat, LinuxStatfs, LinuxStatx, AT_EMPTY_PATH, AT_FDCWD, AT_SYMLINK_NOFOLLOW,
    STATX_RESERVED, VALID_FCHOWNAT_FLAGS, VALID_FSTATAT_FLAGS, VALID_STATX_FLAGS,
};

const UID_GID_NO_CHANGE: u32 = u32::MAX;

fn write_stat_result<T: From<FileStat> + Copy>(
    token: usize,
    buf: *mut T,
    stat: FileStat,
) -> SysResult {
    write_user_value(token, buf, &stat.into())?;
    Ok(0)
}

fn stat_by_dirfd_from(snapshot: &PathSnapshot, dirfd: isize) -> SysResult<FileStat> {
    if dirfd == AT_FDCWD {
        return Ok(stat_in(snapshot.context, ".", true)?);
    }
    if dirfd < 0 {
        return Err(SysError::EBADF);
    }
    Ok(get_file_by_fd(dirfd as usize)?.stat()?)
}

pub(super) fn resolve_stat_from(
    snapshot: &PathSnapshot,
    dirfd: isize,
    path: &str,
    follow_final_symlink: bool,
) -> SysResult<FileStat> {
    if path.is_empty() {
        return stat_by_dirfd_from(snapshot, dirfd);
    }
    let is_absolute = path.starts_with('/');
    if !is_absolute && dirfd != AT_FDCWD && dirfd >= 0 {
        let file = get_file_by_fd(dirfd as usize)?;
        if file.is_devfs_dir() {
            let stat = if file.is_devfs_misc_dir() {
                stat_devfs_misc_child(path)
            } else {
                stat_devfs_child(path)
            };
            return stat.ok_or(SysError::ENOENT);
        }
    }
    if is_absolute
        && snapshot.context.is_global_root()
        && let Some(stat) = stat_static_path(path)
    {
        return Ok(stat);
    }
    Ok(stat_in(
        path_context_from(snapshot, dirfd, path)?,
        path,
        follow_final_symlink,
    )?)
}

pub fn sys_fstat(fd: usize, statbuf: *mut LinuxKstat) -> SysResult {
    if statbuf.is_null() {
        return Err(SysError::EFAULT);
    }
    let token = current_user_token();
    let file = get_file_by_fd(fd)?;
    write_stat_result(token, statbuf, file.stat()?)
}

pub fn sys_newfstatat(
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

    let token = current_user_token();
    let path = read_user_c_string(token, pathname, PATH_MAX)?;
    if path.is_empty() && flags & AT_EMPTY_PATH == 0 {
        return Err(SysError::ENOENT);
    }
    let follow_final_symlink = flags & AT_SYMLINK_NOFOLLOW == 0;
    let snapshot = current_process().path_snapshot();
    write_stat_result(
        token,
        statbuf,
        resolve_stat_from(&snapshot, dirfd, path.as_str(), follow_final_symlink)?,
    )
}

fn can_change_mode(stat: FileStat) -> bool {
    let credentials = current_process().credentials();
    // UNFINISHED: Linux chmod checks CAP_FOWNER and filesystem uid in the
    // caller's user namespace. This kernel only has root-equivalent uid 0 plus
    // stored fsuid.
    credentials.euid == 0 || credentials.fsuid == stat.uid
}

fn ensure_can_change_owner(_stat: FileStat, uid: Option<u32>, gid: Option<u32>) -> SysResult<()> {
    let credentials = current_process().credentials();
    if credentials.euid == 0 {
        return Ok(());
    }
    if uid.is_none() && gid.is_none() {
        return Ok(());
    }
    // UNFINISHED: Linux permits a limited non-root chown group-change case
    // when the file is owned by the caller and the new group is effective or
    // supplementary. This first pass keeps non-root ownership mutation denied.
    Err(SysError::EPERM)
}

pub fn sys_fchmodat(dirfd: isize, pathname: *const u8, mode: u32) -> SysResult {
    if pathname.is_null() {
        return Err(SysError::EFAULT);
    }
    let token = current_user_token();
    let path = read_user_c_string(token, pathname, PATH_MAX)?;
    if path.is_empty() {
        return Err(SysError::ENOENT);
    }
    let snapshot = current_process().path_snapshot();
    let stat = resolve_stat_from(&snapshot, dirfd, path.as_str(), true)?;
    if !can_change_mode(stat) {
        return Err(SysError::EPERM);
    }
    // UNFINISHED: Linux clears setuid/setgid bits in additional cases depending
    // on ownership, group membership, and capabilities. The current credential
    // model is still root-compatible and does not implement those transitions.
    chmod_in(
        path_context_from(&snapshot, dirfd, path.as_str())?,
        path.as_str(),
        true,
        mode,
    )?;
    Ok(0)
}

fn decode_chown_id(raw: u32) -> Option<u32> {
    (raw != UID_GID_NO_CHANGE).then_some(raw)
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
    let follow_final_symlink = flags & AT_SYMLINK_NOFOLLOW == 0;
    let snapshot = current_process().path_snapshot();

    if path.is_empty() {
        if flags & AT_EMPTY_PATH == 0 {
            return Err(SysError::ENOENT);
        }
        if dirfd == AT_FDCWD {
            let stat = stat_in(snapshot.context, ".", follow_final_symlink)?;
            ensure_can_change_owner(stat, uid, gid)?;
            chown_in(snapshot.context, ".", follow_final_symlink, uid, gid)?;
            return Ok(0);
        }
        if dirfd < 0 {
            return Err(SysError::EBADF);
        }
        let file = get_file_by_fd(dirfd as usize)?;
        ensure_can_change_owner(file.stat()?, uid, gid)?;
        file.set_owner(uid, gid)?;
        return Ok(0);
    }

    let stat = resolve_stat_from(&snapshot, dirfd, path.as_str(), follow_final_symlink)?;
    ensure_can_change_owner(stat, uid, gid)?;
    chown_in(
        path_context_from(&snapshot, dirfd, path.as_str())?,
        path.as_str(),
        follow_final_symlink,
        uid,
        gid,
    )?;
    Ok(0)
}

pub fn sys_statfs(pathname: *const u8, statfsbuf: *mut LinuxStatfs) -> SysResult {
    if statfsbuf.is_null() || pathname.is_null() {
        return Err(SysError::EFAULT);
    }
    let token = current_user_token();
    let path = read_user_c_string(token, pathname, PATH_MAX)?;
    if path.is_empty() {
        return Err(SysError::ENOENT);
    }
    let snapshot = current_process().path_snapshot();
    let stat = resolve_stat_from(&snapshot, AT_FDCWD, path.as_str(), true)?;
    let fs_stat = statfs_for_mount(MountId(stat.dev as usize)).ok_or(SysError::ENOSYS)?;
    write_user_value(token, statfsbuf, &LinuxStatfs::from(fs_stat))?;
    Ok(0)
}

pub fn sys_statx(
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

    let token = current_user_token();
    let path = read_user_c_string(token, pathname, PATH_MAX)?;
    if path.is_empty() && flags & AT_EMPTY_PATH == 0 {
        return Err(SysError::ENOENT);
    }
    let follow_final_symlink = flags & AT_SYMLINK_NOFOLLOW == 0;
    let snapshot = current_process().path_snapshot();
    write_stat_result(
        token,
        statxbuf,
        resolve_stat_from(&snapshot, dirfd, path.as_str(), follow_final_symlink)?,
    )
}
