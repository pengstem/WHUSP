use crate::fs::{File, OpenFlags, make_pipe};
use crate::task::{FdFlags, FdTableEntry, current_process, current_user_token};
use alloc::sync::Arc;
use core::mem::size_of;

use super::super::errno::{SysError, SysResult};
use super::user_ptr::{
    UserBufferAccess, read_user_value, translated_byte_buffer_checked, write_user_value,
};

const F_DUPFD: usize = 0;
const F_GETFD: usize = 1;
const F_SETFD: usize = 2;
const F_GETFL: usize = 3;
const F_SETFL: usize = 4;
const F_GETLK: usize = 5;
const F_SETLK: usize = 6;
const F_SETLKW: usize = 7;
const F_DUPFD_CLOEXEC: usize = 1030;
const VALID_PIPE2_FLAGS: u32 = OpenFlags::NONBLOCK.bits() | OpenFlags::CLOEXEC.bits();
const VALID_DUP3_FLAGS: u32 = OpenFlags::CLOEXEC.bits();

const F_RDLCK: i16 = 0;
const F_WRLCK: i16 = 1;
const F_UNLCK: i16 = 2;

#[repr(C)]
#[derive(Clone, Copy)]
struct LinuxFlock {
    l_type: i16,
    l_whence: i16,
    l_start: i64,
    l_len: i64,
    l_pid: i32,
}

pub(super) fn get_fd_entry_by_fd(fd: usize) -> SysResult<FdTableEntry> {
    let process = current_process();
    let inner = process.inner_exclusive_access();
    inner
        .fd_table
        .get(fd)
        .and_then(|entry| entry.as_ref())
        .cloned()
        .ok_or(SysError::EBADF)
}

pub(super) fn get_file_by_fd(fd: usize) -> SysResult<Arc<dyn File + Send + Sync>> {
    Ok(get_fd_entry_by_fd(fd)?.file())
}

pub fn sys_close(fd: usize) -> SysResult {
    let process = current_process();
    let entry = {
        let mut inner = process.inner_exclusive_access();
        if fd >= inner.fd_table.len() {
            return Err(SysError::EBADF);
        }
        let Some(entry) = inner.fd_table[fd].take() else {
            return Err(SysError::EBADF);
        };
        entry
    };
    drop(entry);
    Ok(0)
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

    let process = current_process();
    let mut inner = process.inner_exclusive_access();
    let (pipe_read, pipe_write) = make_pipe();
    let read_fd = inner.alloc_fd_from(0).ok_or(SysError::EMFILE)?;
    inner.fd_table[read_fd] = Some(FdTableEntry::from_file(
        pipe_read,
        OpenFlags::RDONLY | pipe_flags,
    ));
    let write_fd = match inner.alloc_fd_from(0) {
        Some(fd) => fd,
        None => {
            inner.fd_table[read_fd] = None;
            return Err(SysError::EMFILE);
        }
    };
    inner.fd_table[write_fd] = Some(FdTableEntry::from_file(
        pipe_write,
        OpenFlags::WRONLY | pipe_flags,
    ));

    if let Err(err) = write_pipefd_pair(token, pipefd, [read_fd as i32, write_fd as i32]) {
        inner.fd_table[read_fd] = None;
        inner.fd_table[write_fd] = None;
        return Err(err);
    }
    Ok(0)
}

pub fn sys_dup(fd: usize) -> SysResult {
    let process = current_process();
    let mut inner = process.inner_exclusive_access();
    let entry = inner
        .fd_table
        .get(fd)
        .and_then(|entry| entry.as_ref())
        .cloned()
        .ok_or(SysError::EBADF)?;
    let new_fd = inner.alloc_fd_from(0).ok_or(SysError::EMFILE)?;
    inner.fd_table[new_fd] = Some(entry.duplicate(FdFlags::empty()));
    Ok(new_fd as isize)
}

pub fn sys_dup3(old_fd: usize, new_fd: usize, flags: u32) -> SysResult {
    if flags & !VALID_DUP3_FLAGS != 0 {
        return Err(SysError::EINVAL);
    }

    let fd_flags = if flags & OpenFlags::CLOEXEC.bits() != 0 {
        FdFlags::CLOEXEC
    } else {
        FdFlags::empty()
    };

    let process = current_process();
    let mut inner = process.inner_exclusive_access();
    let entry = inner
        .fd_table
        .get(old_fd)
        .and_then(|entry| entry.as_ref())
        .cloned()
        .ok_or(SysError::EBADF)?;
    if old_fd == new_fd {
        return Err(SysError::EINVAL);
    }
    if new_fd >= inner.nofile_limit() {
        return Err(SysError::EBADF);
    }
    while inner.fd_table.len() <= new_fd {
        inner.fd_table.push(None);
    }
    inner.fd_table[new_fd] = Some(entry.duplicate(fd_flags));
    Ok(new_fd as isize)
}

fn fcntl_dup(fd: usize, lower_bound: usize, fd_flags: FdFlags) -> SysResult {
    let process = current_process();
    let mut inner = process.inner_exclusive_access();
    if lower_bound >= inner.nofile_limit() {
        return Err(SysError::EINVAL);
    }
    let entry = inner
        .fd_table
        .get(fd)
        .and_then(|entry| entry.as_ref())
        .cloned()
        .ok_or(SysError::EBADF)?;
    let new_fd = inner.alloc_fd_from(lower_bound).ok_or(SysError::EMFILE)?;
    inner.fd_table[new_fd] = Some(entry.duplicate(fd_flags));
    Ok(new_fd as isize)
}

fn valid_flock_type(l_type: i16) -> bool {
    matches!(l_type, F_RDLCK | F_WRLCK | F_UNLCK)
}

fn fcntl_getlk(fd: usize, lock: *mut LinuxFlock) -> SysResult {
    let _ = get_fd_entry_by_fd(fd)?;
    let token = current_user_token();
    let mut flock = read_user_value(token, lock.cast_const())?;
    if !valid_flock_type(flock.l_type) {
        return Err(SysError::EINVAL);
    }
    // UNFINISHED: byte-range lock conflict tracking is not implemented; report no conflict.
    flock.l_type = F_UNLCK;
    write_user_value(token, lock, &flock)?;
    Ok(0)
}

fn fcntl_setlk(fd: usize, lock: *const LinuxFlock) -> SysResult {
    let _ = get_fd_entry_by_fd(fd)?;
    let token = current_user_token();
    let flock = read_user_value(token, lock)?;
    if !valid_flock_type(flock.l_type) {
        return Err(SysError::EINVAL);
    }
    // UNFINISHED: advisory byte-range lock ownership, conflicts, and F_SETLKW waits are ignored.
    Ok(0)
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
        F_GETLK => fcntl_getlk(fd, arg as *mut LinuxFlock),
        F_SETLK | F_SETLKW => fcntl_setlk(fd, arg as *const LinuxFlock),
        _ => Err(SysError::EINVAL),
    }
}
