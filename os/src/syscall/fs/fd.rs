use crate::fs::{File, make_pipe};
use crate::mm::translated_refmut;
use crate::task::{current_process, current_user_token};
use alloc::sync::Arc;

use super::super::errno::{SysError, SysResult};

pub(super) fn get_file_by_fd(fd: usize) -> SysResult<Arc<dyn File + Send + Sync>> {
    let process = current_process();
    let inner = process.inner_exclusive_access();
    inner
        .fd_table
        .get(fd)
        .and_then(|file| file.as_ref())
        .cloned()
        .ok_or(SysError::EBADF)
}

pub fn sys_close(fd: usize) -> SysResult {
    let process = current_process();
    let mut inner = process.inner_exclusive_access();
    if fd >= inner.fd_table.len() {
        return Err(SysError::EBADF);
    }
    if inner.fd_table[fd].is_none() {
        return Err(SysError::EBADF);
    }
    inner.fd_table[fd].take();
    Ok(0)
}

pub fn sys_pipe(pipe: *mut usize) -> SysResult {
    let process = current_process();
    let token = current_user_token();
    let mut inner = process.inner_exclusive_access();
    let (pipe_read, pipe_write) = make_pipe();
    let read_fd = inner.alloc_fd();
    inner.fd_table[read_fd] = Some(pipe_read);
    let write_fd = inner.alloc_fd();
    inner.fd_table[write_fd] = Some(pipe_write);
    *translated_refmut(token, pipe) = read_fd;
    *translated_refmut(token, unsafe { pipe.add(1) }) = write_fd;
    Ok(0)
}

pub fn sys_dup(fd: usize) -> SysResult {
    let process = current_process();
    let mut inner = process.inner_exclusive_access();
    if fd >= inner.fd_table.len() {
        return Err(SysError::EBADF);
    }
    if inner.fd_table[fd].is_none() {
        return Err(SysError::EBADF);
    }
    let new_fd = inner.alloc_fd();
    inner.fd_table[new_fd] = Some(Arc::clone(inner.fd_table[fd].as_ref().unwrap()));
    Ok(new_fd as isize)
}
