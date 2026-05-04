use super::super::errno::{SysError, SysResult};
use super::fd::{get_fd_entry_by_fd, get_file_by_fd};
use super::stat::resolve_stat;
use super::uapi::{
    AT_EACCESS, AT_EMPTY_PATH, AT_FDCWD, AT_REMOVEDIR, AT_SYMLINK_NOFOLLOW, F_OK, LinuxTimeSpec,
    RENAME_EXCHANGE, RENAME_NOREPLACE, RENAME_WHITEOUT, UTIME_NOW, UTIME_OMIT, VALID_ACCESS_MODE,
    VALID_FACCESSAT_FLAGS, VALID_FACCESSAT2_FLAGS, VALID_RENAME_FLAGS, VALID_UTIMENSAT_FLAGS, X_OK,
};
use super::user_ptr::read_user_value;
use super::user_ptr::{
    PATH_MAX, UserBufferAccess, copy_to_user, read_user_c_string, translated_byte_buffer_checked,
};
use crate::fs::{
    File, FileStat, FileTimestamp, OpenFlags, WorkingDir, link_file_at, lookup_dir_at, mkdir_at,
    normalize_path, open_devfs_child, open_devfs_misc_child, open_file_at, rename_at, rmdir_at,
    symlink_at, unlink_file_at,
};
use crate::mm::UserBuffer;
use crate::task::{FdTableEntry, current_process, current_user_token};
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec;

pub(super) fn dirfd_base(dirfd: isize) -> SysResult<WorkingDir> {
    if dirfd == AT_FDCWD {
        return Ok(current_process().working_dir());
    }
    if dirfd < 0 {
        return Err(SysError::EBADF);
    }
    let file = get_file_by_fd(dirfd as usize)?;
    file.working_dir().ok_or(SysError::ENOTDIR)
}

fn path_base(dirfd: isize, path: &str) -> SysResult<WorkingDir> {
    if path.starts_with('/') {
        Ok(WorkingDir::root())
    } else {
        dirfd_base(dirfd)
    }
}

fn check_access_mode(stat: &FileStat, mode: i32) -> SysResult<()> {
    if mode == F_OK {
        return Ok(());
    }

    // UNFINISHED: Linux access checks depend on real/effective uid, gid,
    // supplementary groups, path-prefix search permissions, read-only
    // filesystems, immutable bits, and ETXTBSY. This kernel currently has no
    // full credential/capability model, so R_OK/W_OK are root-like once the
    // target resolves, while X_OK still requires any execute bit.
    if mode & X_OK != 0 && stat.mode & 0o111 == 0 {
        return Err(SysError::EACCES);
    }
    Ok(())
}

fn parse_utimensat_time(
    time: LinuxTimeSpec,
    now: FileTimestamp,
) -> SysResult<Option<FileTimestamp>> {
    match time.tv_nsec {
        UTIME_NOW => Ok(Some(now)),
        UTIME_OMIT => Ok(None),
        0..=999_999_999 => {
            if time.tv_sec < 0 {
                // UNFINISHED: Linux accepts negative timestamps on filesystems
                // that can represent pre-epoch times. This kernel timestamp
                // model is unsigned for now, so reject them.
                return Err(SysError::EINVAL);
            }
            Ok(Some(FileTimestamp {
                sec: time.tv_sec as u64,
                nsec: time.tv_nsec as u32,
            }))
        }
        _ => Err(SysError::EINVAL),
    }
}

fn read_utimensat_times(
    times: *const LinuxTimeSpec,
    now: FileTimestamp,
) -> SysResult<(Option<FileTimestamp>, Option<FileTimestamp>, bool)> {
    if times.is_null() {
        return Ok((Some(now), Some(now), false));
    }

    let token = current_user_token();
    let atime = parse_utimensat_time(read_user_value(token, times)?, now)?;
    let mtime = parse_utimensat_time(read_user_value(token, times.wrapping_add(1))?, now)?;
    Ok((atime, mtime, atime.is_none() && mtime.is_none()))
}

fn apply_utimensat_to_file(
    file: Arc<dyn File + Send + Sync>,
    atime: Option<FileTimestamp>,
    mtime: Option<FileTimestamp>,
    ctime: FileTimestamp,
) -> SysResult {
    file.set_times(atime, mtime, ctime)?;
    Ok(0)
}

fn copy_c_string_to_user(ptr: *mut u8, buf_len: usize, string: &str) -> SysResult {
    let total_len = string.len() + 1;
    if buf_len < total_len {
        return Err(SysError::ERANGE);
    }
    let token = current_user_token();
    let mut written = 0usize;
    let buffers = translated_byte_buffer_checked(
        token,
        ptr.cast_const(),
        total_len,
        UserBufferAccess::Write,
    )?;
    for byte_ref in UserBuffer::new(buffers) {
        unsafe {
            *byte_ref = if written < string.len() {
                string.as_bytes()[written]
            } else {
                0
            };
        }
        written += 1;
    }
    Ok(ptr as isize)
}

fn readlink_file_to_user(
    file: Arc<dyn File + Send + Sync>,
    buf: *mut u8,
    bufsiz: usize,
) -> SysResult {
    // UNFINISHED: double-buffers through a kernel-side allocation; ideally
    // the VFS readlink would accept a UserBuffer to write into user pages
    // directly and avoid the extra copy.
    let mut kernel_buf = vec![0u8; PATH_MAX];
    let read_len = file.readlink(&mut kernel_buf)?;
    let copy_len = read_len.min(bufsiz);
    copy_to_user(current_user_token(), buf, &kernel_buf[..copy_len])?;
    Ok(copy_len as isize)
}

fn openat_dir_path(dirfd: isize, path: &str) -> SysResult<Option<String>> {
    if path.starts_with('/') {
        return Ok(normalize_path("/", path));
    }
    if dirfd == AT_FDCWD {
        return Ok(normalize_path(&current_process().working_dir_path(), path));
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
        .and_then(|base_path| normalize_path(base_path, path)))
}

fn install_open_file(
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

fn open_devfs_child_from_dirfd(
    dirfd: isize,
    path: &str,
    flags: OpenFlags,
) -> SysResult<Option<Arc<dyn File + Send + Sync>>> {
    if path.starts_with('/') || dirfd == AT_FDCWD || dirfd < 0 {
        return Ok(None);
    }
    let file = get_file_by_fd(dirfd as usize)?;
    if !file.is_devfs_dir() {
        return Ok(None);
    }
    let child = if file.is_devfs_misc_dir() {
        open_devfs_misc_child(path, flags)?
    } else {
        open_devfs_child(path, flags)?
    };
    child.map(Some).ok_or(SysError::ENOENT)
}

pub fn sys_openat(dirfd: isize, path: *const u8, flags: u32, _mode: u32) -> SysResult {
    let token = current_user_token();
    let path = read_user_c_string(token, path, PATH_MAX)?;
    let Some(flags) = OpenFlags::from_bits(flags) else {
        return Err(SysError::EINVAL);
    };
    if flags.bits() & 0b11 == 0b11 {
        return Err(SysError::EINVAL);
    }
    if let Some(file) = open_devfs_child_from_dirfd(dirfd, path.as_str(), flags)? {
        return install_open_file(file, flags, None);
    }
    let dir_path = openat_dir_path(dirfd, path.as_str())?;
    let base = path_base(dirfd, path.as_str())?;
    let file = open_file_at(base, path.as_str(), flags)?;
    install_open_file(file, flags, dir_path)
}

fn do_faccessat(
    dirfd: isize,
    path: *const u8,
    mode: i32,
    flags: i32,
    valid_flags: i32,
) -> SysResult {
    if mode & !VALID_ACCESS_MODE != 0 || flags & !valid_flags != 0 {
        return Err(SysError::EINVAL);
    }
    // CONTEXT: AT_EACCESS is accepted as a no-op because this kernel does not
    // yet distinguish real and effective credentials for user tasks.
    let _use_effective_ids = flags & AT_EACCESS != 0;

    let token = current_user_token();
    let path = read_user_c_string(token, path, PATH_MAX)?;
    if path.is_empty() && flags & AT_EMPTY_PATH == 0 {
        return Err(SysError::ENOENT);
    }

    let follow_final_symlink = flags & AT_SYMLINK_NOFOLLOW == 0;
    let stat = resolve_stat(dirfd, path.as_str(), follow_final_symlink)?;
    check_access_mode(&stat, mode)?;
    Ok(0)
}

pub fn sys_faccessat(dirfd: isize, path: *const u8, mode: i32, flags: i32) -> SysResult {
    do_faccessat(dirfd, path, mode, flags, VALID_FACCESSAT_FLAGS)
}

pub fn sys_faccessat2(dirfd: isize, path: *const u8, mode: i32, flags: i32) -> SysResult {
    do_faccessat(dirfd, path, mode, flags, VALID_FACCESSAT2_FLAGS)
}

pub fn sys_utimensat(
    dirfd: isize,
    pathname: *const u8,
    times: *const LinuxTimeSpec,
    flags: i32,
) -> SysResult {
    if flags & !VALID_UTIMENSAT_FLAGS != 0 {
        return Err(SysError::EINVAL);
    }

    let now = FileTimestamp::now();
    let (atime, mtime, all_omitted) = read_utimensat_times(times, now)?;
    if all_omitted {
        return Ok(0);
    }
    // UNFINISHED: Linux checks write access, ownership, capabilities, and
    // immutable/append-only inode flags before changing timestamps. This kernel
    // currently has no real credential or inode-flag model, so existing targets
    // are allowed once pathname/fd resolution succeeds.

    if pathname.is_null() {
        if flags != 0 {
            return Err(SysError::EINVAL);
        }
        if dirfd == AT_FDCWD {
            return Err(SysError::EFAULT);
        }
        if dirfd < 0 {
            return Err(SysError::EBADF);
        }
        let file = get_file_by_fd(dirfd as usize)?;
        return apply_utimensat_to_file(file, atime, mtime, now);
    }

    let token = current_user_token();
    let path = read_user_c_string(token, pathname, PATH_MAX)?;
    if path.is_empty() {
        if flags & AT_EMPTY_PATH == 0 {
            return Err(SysError::ENOENT);
        }
        if dirfd == AT_FDCWD {
            let file = open_file_at(current_process().working_dir(), ".", OpenFlags::PATH)?;
            return apply_utimensat_to_file(file, atime, mtime, now);
        }
        if dirfd < 0 {
            return Err(SysError::EBADF);
        }
        let file = get_file_by_fd(dirfd as usize)?;
        return apply_utimensat_to_file(file, atime, mtime, now);
    }

    let open_flags = if flags & AT_SYMLINK_NOFOLLOW != 0 {
        OpenFlags::PATH | OpenFlags::NOFOLLOW
    } else {
        OpenFlags::PATH
    };

    if let Some(file) = open_devfs_child_from_dirfd(dirfd, path.as_str(), open_flags)? {
        return apply_utimensat_to_file(file, atime, mtime, now);
    }
    let base = path_base(dirfd, path.as_str())?;
    let file = open_file_at(base, path.as_str(), open_flags)?;
    apply_utimensat_to_file(file, atime, mtime, now)
}

pub fn sys_chdir(path: *const u8) -> SysResult {
    let process = current_process();
    let token = current_user_token();
    let path = read_user_c_string(token, path, PATH_MAX)?;
    let cwd = process.working_dir();
    let next_cwd = lookup_dir_at(cwd, path.as_str())?;
    let Some(next_path) = normalize_path(&process.working_dir_path(), path.as_str()) else {
        return Err(SysError::ENOENT);
    };
    process.set_working_dir(next_cwd, next_path);
    Ok(0)
}

pub fn sys_fchdir(fd: usize) -> SysResult {
    let entry = get_fd_entry_by_fd(fd)?;
    let file = entry.file();
    if file.is_devfs_dir() {
        // UNFINISHED: Lightweight devfs directories do not have a WorkingDir
        // representation, so they cannot become the process cwd yet.
        return Err(SysError::ENOTSUP);
    }
    let Some(next_cwd) = file.working_dir() else {
        return Err(SysError::ENOTDIR);
    };
    let Some(next_path) = entry.dir_path() else {
        // UNFINISHED: Linux fchdir keeps cwd as a directory object even when
        // the original pathname is unavailable. This kernel's getcwd path is
        // still string-backed, so fchdir needs path metadata from openat.
        return Err(SysError::ENOTSUP);
    };
    // UNFINISHED: Linux fchdir checks search permission unless credentials can
    // bypass it. This kernel currently runs user tasks as uid 0 and has no
    // real/effective credential or supplementary-group model.
    current_process().set_working_dir(next_cwd, String::from(next_path));
    Ok(0)
}

pub fn sys_getcwd(buf: *mut u8, size: usize) -> SysResult {
    let process = current_process();
    let cwd_path = process.working_dir_path();
    copy_c_string_to_user(buf, size, cwd_path.as_str())
}

pub fn sys_mkdirat(dirfd: isize, path: *const u8, mode: u32) -> SysResult {
    let token = current_user_token();
    let path = read_user_c_string(token, path, PATH_MAX)?;
    let base = path_base(dirfd, path.as_str())?;
    mkdir_at(base, path.as_str(), mode)?;
    Ok(0)
}

pub fn sys_unlinkat(dirfd: isize, path: *const u8, flags: u32) -> SysResult {
    if flags & !AT_REMOVEDIR != 0 {
        return Err(SysError::EINVAL);
    }
    let token = current_user_token();
    let path = read_user_c_string(token, path, PATH_MAX)?;
    let base = path_base(dirfd, path.as_str())?;
    if flags & AT_REMOVEDIR != 0 {
        rmdir_at(base, path.as_str())?;
    } else {
        unlink_file_at(base, path.as_str())?;
    }
    Ok(0)
}

pub fn sys_linkat(
    olddirfd: isize,
    oldpath: *const u8,
    newdirfd: isize,
    newpath: *const u8,
    flags: u32,
) -> SysResult {
    // UNFINISHED: Linux linkat supports AT_SYMLINK_FOLLOW and AT_EMPTY_PATH; this kernel currently implements pathname hard links only.
    if flags != 0 {
        return Err(SysError::EINVAL);
    }

    let token = current_user_token();
    let oldpath = read_user_c_string(token, oldpath, PATH_MAX)?;
    let newpath = read_user_c_string(token, newpath, PATH_MAX)?;
    let old_base = path_base(olddirfd, oldpath.as_str())?;
    let new_base = path_base(newdirfd, newpath.as_str())?;
    link_file_at(old_base, oldpath.as_str(), new_base, newpath.as_str())?;
    Ok(0)
}

pub fn sys_symlinkat(target: *const u8, newdirfd: isize, linkpath: *const u8) -> SysResult {
    let token = current_user_token();
    let target = read_user_c_string(token, target, PATH_MAX)?;
    let linkpath = read_user_c_string(token, linkpath, PATH_MAX)?;
    if target.is_empty() || linkpath.is_empty() {
        return Err(SysError::ENOENT);
    }
    let base = path_base(newdirfd, linkpath.as_str())?;
    symlink_at(base, target.as_str(), linkpath.as_str())?;
    Ok(0)
}

pub fn sys_readlinkat(dirfd: isize, path: *const u8, buf: *mut u8, bufsiz: usize) -> SysResult {
    if bufsiz == 0 {
        return Err(SysError::EINVAL);
    }

    let token = current_user_token();
    let path = read_user_c_string(token, path, PATH_MAX)?;
    let file = if path.is_empty() {
        if dirfd == AT_FDCWD {
            return Err(SysError::ENOENT);
        }
        if dirfd < 0 {
            return Err(SysError::EBADF);
        }
        get_file_by_fd(dirfd as usize)?
    } else {
        let base = path_base(dirfd, path.as_str())?;
        open_file_at(base, path.as_str(), OpenFlags::PATH | OpenFlags::NOFOLLOW)?
    };
    readlink_file_to_user(file, buf, bufsiz)
}

pub fn sys_renameat2(
    olddirfd: isize,
    oldpath: *const u8,
    newdirfd: isize,
    newpath: *const u8,
    flags: u32,
) -> SysResult {
    if flags & !VALID_RENAME_FLAGS != 0 {
        return Err(SysError::EINVAL);
    }
    if flags & RENAME_EXCHANGE != 0 {
        // UNFINISHED: Linux RENAME_EXCHANGE atomically swaps two existing pathnames.
        // The current EXT4/VFS wrapper only supports one-way rename.
        return Err(SysError::EINVAL);
    }
    if flags & RENAME_WHITEOUT != 0 {
        // UNFINISHED: Linux RENAME_WHITEOUT creates an overlay/union whiteout
        // device while renaming. This kernel has no overlay filesystem support.
        return Err(SysError::EINVAL);
    }

    let token = current_user_token();
    let oldpath = read_user_c_string(token, oldpath, PATH_MAX)?;
    let newpath = read_user_c_string(token, newpath, PATH_MAX)?;
    let old_base = path_base(olddirfd, oldpath.as_str())?;
    let new_base = path_base(newdirfd, newpath.as_str())?;
    rename_at(
        old_base,
        oldpath.as_str(),
        new_base,
        newpath.as_str(),
        flags & RENAME_NOREPLACE != 0,
    )?;
    Ok(0)
}

pub fn sys_getdents64(fd: usize, buf: *mut u8, len: usize) -> SysResult {
    if len == 0 {
        return Err(SysError::EINVAL);
    }
    let token = current_user_token();
    let buffers =
        translated_byte_buffer_checked(token, buf.cast_const(), len, UserBufferAccess::Write)?;
    let process = current_process();
    let inner = process.inner_exclusive_access();
    let Some(file) = inner
        .fd_table
        .get(fd)
        .and_then(|entry| entry.as_ref())
        .map(|entry| entry.file())
    else {
        return Err(SysError::EBADF);
    };
    drop(inner);
    Ok(file.read_dirent64(UserBuffer::new(buffers))?)
}
