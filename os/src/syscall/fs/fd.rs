use crate::config::PAGE_SIZE;
use crate::fs::{
    File, OpenFlags, default_pipe_capacity_for_current_process, make_memfd, make_pipe,
    pipe_max_size,
};
use crate::task::{FdFlags, FdTableEntry, current_process, current_user_token};
use alloc::sync::Arc;
use core::mem::size_of;

use super::super::errno::{SysError, SysResult};
use super::super::user_ptr::{
    UserBufferAccess, read_user_c_string, translated_byte_buffer_checked,
};
use super::fanotify::fanotify_notify_close;
use super::fd_lock::{
    fcntl_getlk, fcntl_ofd_getlk, fcntl_ofd_setlk, fcntl_ofd_setlkw, fcntl_setlk, fcntl_setlkw,
    flock_operation, release_flock_locks_for_close, release_ofd_record_locks_for_close,
    release_record_locks_for_close,
};

const F_DUPFD: usize = 0;
const F_GETFD: usize = 1;
const F_SETFD: usize = 2;
const F_GETFL: usize = 3;
const F_SETFL: usize = 4;
const F_GETLK: usize = 5;
const F_SETLK: usize = 6;
const F_SETLKW: usize = 7;
const F_OFD_GETLK: usize = 36;
const F_OFD_SETLK: usize = 37;
const F_OFD_SETLKW: usize = 38;
const F_SETLEASE: usize = 1024;
const F_GETLEASE: usize = 1025;
const F_DUPFD_CLOEXEC: usize = 1030;
const F_SETPIPE_SZ: usize = 1031;
const F_GETPIPE_SZ: usize = 1032;
const F_ADD_SEALS: usize = 1033;
const F_GET_SEALS: usize = 1034;
const VALID_PIPE2_FLAGS: u32 = OpenFlags::NONBLOCK.bits() | OpenFlags::CLOEXEC.bits();
const VALID_DUP3_FLAGS: u32 = OpenFlags::CLOEXEC.bits();
const MAX_PIPE_SIZE_ARG: usize = 1 << 31;
const MFD_CLOEXEC: u32 = 0x0001;
const MFD_ALLOW_SEALING: u32 = 0x0002;
const MFD_VALID_FLAGS: u32 = MFD_CLOEXEC | MFD_ALLOW_SEALING;
const MEMFD_NAME_MAX: usize = 249;

pub(super) fn get_fd_entry_by_fd(fd: usize) -> SysResult<FdTableEntry> {
    let process = current_process();
    let inner = process.inner_exclusive_access();
    inner.fd_entry(fd).ok_or(SysError::EBADF)
}

pub(super) fn get_file_by_fd(fd: usize) -> SysResult<Arc<dyn File + Send + Sync>> {
    Ok(get_fd_entry_by_fd(fd)?.file())
}

pub fn sys_close(fd: usize) -> SysResult {
    let process = current_process();
    let entry = {
        let mut inner = process.inner_exclusive_access();
        inner.take_fd_entry(fd).ok_or(SysError::EBADF)?
    };
    close_detached_fd_entry(entry);
    Ok(0)
}

/// Completes close cleanup after an fd entry has left the process table.
///
/// Call this without holding `ProcessControlBlockInner`; lock and fanotify
/// cleanup can inspect file state and must not run while the fd table is locked.
fn close_detached_fd_entry(entry: FdTableEntry) {
    release_record_locks_for_close(&entry);
    release_ofd_record_locks_for_close(&entry);
    release_flock_locks_for_close(&entry);
    let file = entry.file();
    fanotify_notify_close(&file, file.writable());
    drop(entry);
}

fn pipe2_open_flags(flags: u32) -> SysResult<OpenFlags> {
    if flags & !VALID_PIPE2_FLAGS != 0 {
        // UNFINISHED: pipe2 currently supports only O_NONBLOCK and O_CLOEXEC;
        // Linux O_DIRECT packet mode and notification pipes are not implemented.
        return Err(SysError::EINVAL);
    }
    Ok(OpenFlags::from_bits_truncate(flags))
}

fn validate_pipefd(token: usize, pipefd: *mut i32) -> SysResult<()> {
    translated_byte_buffer_checked(
        token,
        pipefd as *const u8,
        size_of::<[i32; 2]>(),
        UserBufferAccess::Write,
    )
    .map(|_| ())
}

fn write_pipefd_pair(token: usize, pipefd: *mut i32, fds: [i32; 2]) -> SysResult<()> {
    let mut bytes = [0u8; size_of::<[i32; 2]>()];
    let fd_size = size_of::<i32>();
    bytes[..fd_size].copy_from_slice(&fds[0].to_ne_bytes());
    bytes[fd_size..].copy_from_slice(&fds[1].to_ne_bytes());

    let mut copied = 0usize;
    for buffer in translated_byte_buffer_checked(
        token,
        pipefd as *const u8,
        bytes.len(),
        UserBufferAccess::Write,
    )? {
        let next = copied + buffer.len();
        buffer.copy_from_slice(&bytes[copied..next]);
        copied = next;
    }
    Ok(())
}

pub fn sys_pipe2(pipefd: *mut i32, flags: u32) -> SysResult {
    let pipe_flags = pipe2_open_flags(flags)?;
    let token = current_user_token();
    validate_pipefd(token, pipefd)?;
    let pipe_capacity = default_pipe_capacity_for_current_process();

    let process = current_process();
    let (pipe_read, pipe_write) = make_pipe(pipe_capacity);
    let mut cleanup_entry = None;
    let fds = {
        let mut inner = process.inner_exclusive_access();
        let read_fd = inner.alloc_fd_from(0).ok_or(SysError::EMFILE)?;
        let _ = inner.set_fd_entry(
            read_fd,
            FdTableEntry::from_file(pipe_read, OpenFlags::RDONLY | pipe_flags),
        );
        if let Some(write_fd) = inner.alloc_fd_from(0) {
            let _ = inner.set_fd_entry(
                write_fd,
                FdTableEntry::from_file(pipe_write, OpenFlags::WRONLY | pipe_flags),
            );
            Ok([read_fd, write_fd])
        } else {
            cleanup_entry = inner.take_fd_entry(read_fd);
            Err(SysError::EMFILE)
        }
    };
    if let Some(entry) = cleanup_entry {
        close_detached_fd_entry(entry);
    }
    let fds = fds?;

    if let Err(err) = write_pipefd_pair(token, pipefd, [fds[0] as i32, fds[1] as i32]) {
        let entries = {
            let mut inner = process.inner_exclusive_access();
            [inner.take_fd_entry(fds[0]), inner.take_fd_entry(fds[1])]
        };
        for entry in entries.into_iter().flatten() {
            close_detached_fd_entry(entry);
        }
        return Err(err);
    }
    Ok(0)
}

pub fn sys_dup(fd: usize) -> SysResult {
    let process = current_process();
    let mut inner = process.inner_exclusive_access();
    let entry = inner.fd_entry(fd).ok_or(SysError::EBADF)?;
    let new_fd = inner.alloc_fd_from(0).ok_or(SysError::EMFILE)?;
    let _ = inner.set_fd_entry(new_fd, entry.duplicate(FdFlags::empty()));
    Ok(new_fd as isize)
}

pub fn sys_dup3(old_fd: usize, new_fd: usize, flags: u32) -> SysResult {
    if flags & !VALID_DUP3_FLAGS != 0 {
        return Err(SysError::EINVAL);
    }
    if old_fd == new_fd {
        return Err(SysError::EINVAL);
    }

    let fd_flags = if flags & OpenFlags::CLOEXEC.bits() != 0 {
        FdFlags::CLOEXEC
    } else {
        FdFlags::empty()
    };

    let process = current_process();
    let replaced = {
        let mut inner = process.inner_exclusive_access();
        let entry = inner.fd_entry(old_fd).ok_or(SysError::EBADF)?;
        if new_fd >= inner.nofile_limit() {
            return Err(SysError::EBADF);
        }
        inner.set_fd_entry(new_fd, entry.duplicate(fd_flags))
    };
    if let Some(entry) = replaced {
        // CONTEXT: Linux dup3 atomically closes an already-open newfd before
        // reusing it; close-time errors are not reported by dup3.
        close_detached_fd_entry(entry);
    }
    Ok(new_fd as isize)
}

fn fcntl_dup(fd: usize, lower_bound: usize, fd_flags: FdFlags) -> SysResult {
    let process = current_process();
    let mut inner = process.inner_exclusive_access();
    if lower_bound >= inner.nofile_limit() {
        return Err(SysError::EINVAL);
    }
    let entry = inner.fd_entry(fd).ok_or(SysError::EBADF)?;
    let new_fd = inner.alloc_fd_from(lower_bound).ok_or(SysError::EMFILE)?;
    let _ = inner.set_fd_entry(new_fd, entry.duplicate(fd_flags));
    Ok(new_fd as isize)
}

fn fcntl_get_pipe_size(fd: usize) -> SysResult {
    let file = get_file_by_fd(fd)?;
    file.pipe_capacity()
        .map(|capacity| capacity as isize)
        .ok_or(SysError::EINVAL)
}

fn fcntl_set_pipe_size(fd: usize, requested: usize) -> SysResult {
    let file = get_file_by_fd(fd)?;
    let capacity = file.pipe_capacity().ok_or(SysError::EINVAL)?;
    let occupied = file.pipe_occupied().unwrap_or(0);

    if requested > MAX_PIPE_SIZE_ARG {
        return Err(SysError::EINVAL);
    }
    if requested < occupied {
        return Err(SysError::EBUSY);
    }
    let requested = requested.max(PAGE_SIZE);
    if requested > pipe_max_size() {
        return Err(SysError::EPERM);
    }
    if requested == capacity {
        return Ok(capacity as isize);
    }
    Ok(file.set_pipe_capacity(requested)? as isize)
}

fn fcntl_get_seals(fd: usize) -> SysResult {
    Ok(get_file_by_fd(fd)?.seals()? as isize)
}

fn fcntl_add_seals(fd: usize, seals: u32) -> SysResult {
    get_file_by_fd(fd)?.add_seals(seals)?;
    Ok(0)
}

fn fcntl_set_lease(fd: usize, arg: usize) -> SysResult {
    let entry = get_fd_entry_by_fd(fd)?;
    match arg as i16 {
        // CONTEXT: This is a minimal lease compatibility surface for LTP.
        // Full Linux lease breaking, SIGIO notification, and open/truncate
        // blocking are still not implemented.
        0 if entry.file().writable() => Err(SysError::EAGAIN),
        0 | 1 | 2 => Ok(0),
        _ => Err(SysError::EINVAL),
    }
}

fn fcntl_get_lease(fd: usize) -> SysResult {
    get_fd_entry_by_fd(fd)?;
    Ok(2)
}

fn read_memfd_name(name: *const u8) -> SysResult {
    match read_user_c_string(current_user_token(), name, MEMFD_NAME_MAX + 1) {
        Ok(_) => Ok(0),
        Err(SysError::ENAMETOOLONG) => Err(SysError::EINVAL),
        Err(err) => Err(err),
    }
}

pub fn sys_memfd_create(name: *const u8, flags: u32) -> SysResult {
    read_memfd_name(name)?;
    if flags & !MFD_VALID_FLAGS != 0 {
        return Err(SysError::EINVAL);
    }
    let open_flags = OpenFlags::RDWR
        | OpenFlags::LARGEFILE
        | if flags & MFD_CLOEXEC != 0 {
            OpenFlags::CLOEXEC
        } else {
            OpenFlags::empty()
        };
    let file = make_memfd(flags & MFD_ALLOW_SEALING != 0);

    let process = current_process();
    let mut inner = process.inner_exclusive_access();
    let fd = inner.alloc_fd_from(0).ok_or(SysError::EMFILE)?;
    let _ = inner.set_fd_entry(fd, FdTableEntry::from_file(file, open_flags));
    Ok(fd as isize)
}

pub fn sys_fcntl(fd: usize, op: usize, arg: usize) -> SysResult {
    match op {
        F_DUPFD => fcntl_dup(fd, arg, FdFlags::empty()),
        F_DUPFD_CLOEXEC => fcntl_dup(fd, arg, FdFlags::CLOEXEC),
        F_GETFD => Ok(get_fd_entry_by_fd(fd)?.fd_flags().bits() as isize),
        F_SETFD => {
            let process = current_process();
            let mut inner = process.inner_exclusive_access();
            let entry = inner
                .fd_table
                .get_mut(fd)
                .and_then(|entry| entry.as_mut())
                .ok_or(SysError::EBADF)?;
            entry.set_fd_flags(FdFlags::from_bits_truncate(
                (arg as u32) & FdFlags::CLOEXEC.bits(),
            ));
            Ok(0)
        }
        F_GETFL => Ok(get_fd_entry_by_fd(fd)?.status_flags().bits() as isize),
        F_SETFL => {
            let entry = get_fd_entry_by_fd(fd)?;
            let status = entry.status_flags();
            // UNFINISHED: O_DIRECT is recorded for fcntl compatibility, but direct-I/O
            // alignment and cache-bypass semantics are not enforced by the filesystem layer.
            entry.set_status_flags(status.with_fcntl_status_flags(arg as u32));
            Ok(0)
        }
        F_GETLK => fcntl_getlk(get_fd_entry_by_fd(fd)?, arg as *mut _),
        F_SETLK => fcntl_setlk(get_fd_entry_by_fd(fd)?, arg as *const _),
        F_SETLKW => fcntl_setlkw(get_fd_entry_by_fd(fd)?, arg as *const _),
        F_OFD_GETLK => fcntl_ofd_getlk(get_fd_entry_by_fd(fd)?, arg as *mut _),
        F_OFD_SETLK => fcntl_ofd_setlk(get_fd_entry_by_fd(fd)?, arg as *const _),
        F_OFD_SETLKW => fcntl_ofd_setlkw(get_fd_entry_by_fd(fd)?, arg as *const _),
        F_SETLEASE => fcntl_set_lease(fd, arg),
        F_GETLEASE => fcntl_get_lease(fd),
        F_GETPIPE_SZ => fcntl_get_pipe_size(fd),
        F_SETPIPE_SZ => fcntl_set_pipe_size(fd, arg),
        F_ADD_SEALS => fcntl_add_seals(fd, arg as u32),
        F_GET_SEALS => fcntl_get_seals(fd),
        _ => Err(SysError::EINVAL),
    }
}

pub fn sys_flock(fd: usize, operation: i32) -> SysResult {
    let entry = get_fd_entry_by_fd(fd)?;
    flock_operation(entry, operation)
}
