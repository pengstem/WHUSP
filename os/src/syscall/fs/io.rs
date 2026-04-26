use crate::fs::S_IFDIR;
use crate::mm::{UserBuffer, translated_byte_buffer};
use crate::task::{current_process, current_user_token};
use alloc::vec::Vec;
use core::mem::size_of;

use super::super::errno::{SysError, SysResult};
use super::fd::get_file_by_fd;
use super::uapi::{IOV_MAX, LinuxIovec};
use super::user_ptr::{UserBufferAccess, read_user_usize, translated_byte_buffer_checked};

fn read_user_iovecs(
    token: usize,
    iov: *const LinuxIovec,
    iovcnt: usize,
) -> SysResult<Vec<LinuxIovec>> {
    let entry_size = size_of::<LinuxIovec>();
    let mut iovecs = Vec::new();
    let mut total_len = 0usize;
    for index in 0..iovcnt {
        let entry_addr = (iov as usize)
            .checked_add(index.checked_mul(entry_size).ok_or(SysError::EFAULT)?)
            .ok_or(SysError::EFAULT)?;
        let base = read_user_usize(token, entry_addr)?;
        let len = read_user_usize(token, entry_addr + size_of::<usize>())?;
        total_len = total_len.checked_add(len).ok_or(SysError::EINVAL)?;
        if total_len > isize::MAX as usize {
            return Err(SysError::EINVAL);
        }
        iovecs.push(LinuxIovec { base, len });
    }
    Ok(iovecs)
}

pub fn sys_write(fd: usize, buf: *const u8, len: usize) -> SysResult {
    let token = current_user_token();
    let process = current_process();
    let inner = process.inner_exclusive_access();
    if fd >= inner.fd_table.len() {
        return Err(SysError::EBADF);
    }
    if let Some(file) = &inner.fd_table[fd] {
        if !file.writable() {
            return Err(SysError::EBADF);
        }
        let file = file.clone();
        // release current task TCB manually to avoid multi-borrow
        drop(inner);
        Ok(file.write(UserBuffer::new(translated_byte_buffer(token, buf, len))) as isize)
    } else {
        Err(SysError::EBADF)
    }
}

pub fn sys_writev(fd: usize, iov: *const LinuxIovec, iovcnt: usize) -> SysResult {
    if iovcnt == 0 {
        return Ok(0);
    }
    if iovcnt > IOV_MAX {
        return Err(SysError::EINVAL);
    }
    if iov.is_null() {
        return Err(SysError::EFAULT);
    }

    let token = current_user_token();
    let iovecs = read_user_iovecs(token, iov, iovcnt)?;
    let process = current_process();
    let inner = process.inner_exclusive_access();
    if fd >= inner.fd_table.len() {
        return Err(SysError::EBADF);
    }
    let Some(file) = inner.fd_table[fd].as_ref().cloned() else {
        return Err(SysError::EBADF);
    };
    if !file.writable() {
        return Err(SysError::EBADF);
    }
    drop(inner);

    let mut total_written = 0usize;
    for iovec in iovecs {
        if iovec.len == 0 {
            continue;
        }
        let buffers = match translated_byte_buffer_checked(
            token,
            iovec.base as *const u8,
            iovec.len,
            UserBufferAccess::Read,
        ) {
            Ok(buffers) => buffers,
            Err(_) if total_written > 0 => return Ok(total_written as isize),
            Err(err) => return Err(err),
        };
        let written = file.write(UserBuffer::new(buffers));
        total_written += written;
        if written < iovec.len {
            break;
        }
    }
    Ok(total_written as isize)
}

pub fn sys_readv(fd: usize, iov: *const LinuxIovec, iovcnt: usize) -> SysResult {
    if iovcnt == 0 {
        return Ok(0);
    }
    if iovcnt > IOV_MAX {
        return Err(SysError::EINVAL);
    }
    if iov.is_null() {
        return Err(SysError::EFAULT);
    }

    let token = current_user_token();
    let iovecs = read_user_iovecs(token, iov, iovcnt)?;
    let file = get_file_by_fd(fd)?;
    if !file.readable() {
        return Err(SysError::EBADF);
    }
    if file.stat().mode & S_IFDIR == S_IFDIR {
        return Err(SysError::EISDIR);
    }

    for iovec in iovecs.iter() {
        if iovec.len == 0 {
            continue;
        }
        translated_byte_buffer_checked(
            token,
            iovec.base as *const u8,
            iovec.len,
            UserBufferAccess::Write,
        )?;
    }

    let mut total_read = 0usize;
    for iovec in iovecs {
        if iovec.len == 0 {
            continue;
        }
        let buffers = translated_byte_buffer_checked(
            token,
            iovec.base as *const u8,
            iovec.len,
            UserBufferAccess::Write,
        )?;
        let read = file.read(UserBuffer::new(buffers));
        total_read += read;
        if read < iovec.len {
            break;
        }
    }
    Ok(total_read as isize)
}

pub fn sys_read(fd: usize, buf: *const u8, len: usize) -> SysResult {
    let token = current_user_token();
    let process = current_process();
    let inner = process.inner_exclusive_access();
    if fd >= inner.fd_table.len() {
        return Err(SysError::EBADF);
    }
    if let Some(file) = &inner.fd_table[fd] {
        let file = file.clone();
        if !file.readable() {
            return Err(SysError::EBADF);
        }
        // release current task TCB manually to avoid multi-borrow
        drop(inner);
        Ok(file.read(UserBuffer::new(translated_byte_buffer(token, buf, len))) as isize)
    } else {
        Err(SysError::EBADF)
    }
}
