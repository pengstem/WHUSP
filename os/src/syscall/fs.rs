use crate::fs::{
    OpenFlags, WorkingDir, lookup_dir_at, make_pipe, mkdir_at, normalize_path, open_file_at,
    unlink_file_at,
};
use crate::mm::{
    PageTable, StepByOne, UserBuffer, VirtAddr, translated_byte_buffer, translated_refmut,
    translated_str,
};
use crate::task::{current_process, current_user_token};
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::mem::size_of;

use super::errno::{SysError, SysResult};

const AT_FDCWD: isize = -100;
const IOV_MAX: usize = 1024;

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct LinuxIovec {
    base: usize,
    len: usize,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct LinuxKstat {
    st_dev: u64,
    st_ino: u64,
    st_mode: u32,
    st_nlink: u32,
    st_uid: u32,
    st_gid: u32,
    st_rdev: u64,
    __pad: u64,
    st_size: i64,
    st_blksize: u32,
    __pad2: i32,
    st_blocks: u64,
    st_atime_sec: i64,
    st_atime_nsec: i64,
    st_mtime_sec: i64,
    st_mtime_nsec: i64,
    st_ctime_sec: i64,
    st_ctime_nsec: i64,
    __unused: [u32; 2],
}

impl From<crate::fs::FileStat> for LinuxKstat {
    fn from(stat: crate::fs::FileStat) -> Self {
        Self {
            st_dev: stat.dev,
            st_ino: stat.ino,
            st_mode: stat.mode,
            st_nlink: stat.nlink,
            st_uid: stat.uid,
            st_gid: stat.gid,
            st_rdev: stat.rdev,
            __pad: 0,
            st_size: stat.size as i64,
            st_blksize: stat.blksize,
            __pad2: 0,
            st_blocks: stat.blocks,
            st_atime_sec: stat.atime_sec as i64,
            st_atime_nsec: stat.atime_nsec as i64,
            st_mtime_sec: stat.mtime_sec as i64,
            st_mtime_nsec: stat.mtime_nsec as i64,
            st_ctime_sec: stat.ctime_sec as i64,
            st_ctime_nsec: stat.ctime_nsec as i64,
            __unused: [0; 2],
        }
    }
}

// TODO: i think these functions are taking the responsibility of the mm module
fn translated_byte_buffer_checked(
    token: usize,
    ptr: *const u8,
    len: usize,
) -> SysResult<Vec<&'static mut [u8]>> {
    if len == 0 {
        return Ok(Vec::new());
    }
    let mut start = ptr as usize;
    let end = start.checked_add(len).ok_or(SysError::EFAULT)?;
    let page_table = PageTable::from_token(token);
    let mut buffers = Vec::new();
    while start < end {
        let start_va = VirtAddr::from(start);
        let mut vpn = start_va.floor();
        let pte = page_table.translate(vpn).ok_or(SysError::EFAULT)?;
        if !pte.is_valid() || !pte.readable() {
            return Err(SysError::EFAULT);
        }
        let ppn = pte.ppn();
        vpn.step();
        let mut end_va: VirtAddr = vpn.into();
        end_va = end_va.min(VirtAddr::from(end));
        if end_va.page_offset() == 0 {
            buffers.push(&mut ppn.get_bytes_array()[start_va.page_offset()..]);
        } else {
            buffers.push(&mut ppn.get_bytes_array()[start_va.page_offset()..end_va.page_offset()]);
        }
        start = end_va.into();
    }
    Ok(buffers)
}

fn read_user_usize(token: usize, addr: usize) -> SysResult<usize> {
    let mut bytes = [0u8; size_of::<usize>()];
    let buffers = translated_byte_buffer_checked(token, addr as *const u8, bytes.len())?;
    let mut copied = 0usize;
    for buffer in buffers.iter() {
        let next = copied + buffer.len();
        bytes[copied..next].copy_from_slice(buffer);
        copied = next;
    }
    Ok(usize::from_ne_bytes(bytes))
}

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

fn dirfd_base(dirfd: isize) -> SysResult<WorkingDir> {
    let process = current_process();
    if dirfd == AT_FDCWD {
        return Ok(process.working_dir());
    }
    if dirfd < 0 {
        return Err(SysError::EBADF);
    }
    let inner = process.inner_exclusive_access();
    let file = inner
        .fd_table
        .get(dirfd as usize)
        .and_then(|file| file.as_ref())
        .ok_or(SysError::EBADF)?
        .clone();
    drop(inner);
    file.working_dir().ok_or(SysError::ENOTDIR)
}

fn path_base(dirfd: isize, path: &str) -> SysResult<WorkingDir> {
    if path.starts_with('/') {
        Ok(WorkingDir::root())
    } else {
        dirfd_base(dirfd)
    }
}

fn copy_c_string_to_user(ptr: *mut u8, buf_len: usize, string: &str) -> SysResult {
    let total_len = string.len() + 1;
    if buf_len < total_len {
        return Err(SysError::ERANGE);
    }
    let token = current_user_token();
    let mut written = 0usize;
    for byte_ref in UserBuffer::new(translated_byte_buffer(token, ptr, total_len)) {
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
        let buffers =
            match translated_byte_buffer_checked(token, iovec.base as *const u8, iovec.len) {
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

pub fn sys_openat(dirfd: isize, path: *const u8, flags: u32, _mode: u32) -> SysResult {
    let token = current_user_token();
    let path = translated_str(token, path);
    let Some(flags) = OpenFlags::from_bits(flags) else {
        return Err(SysError::EINVAL);
    };
    if flags.bits() & 0b11 == 0b11 {
        return Err(SysError::EINVAL);
    }
    let base = path_base(dirfd, path.as_str())?;
    let process = current_process();
    let Some(inode) = open_file_at(base, path.as_str(), flags) else {
        return Err(SysError::ENOENT);
    };
    let mut inner = process.inner_exclusive_access();
    let fd = inner.alloc_fd();
    inner.fd_table[fd] = Some(inode);
    Ok(fd as isize)
}

pub fn sys_chdir(path: *const u8) -> SysResult {
    let process = current_process();
    let token = current_user_token();
    let path = translated_str(token, path);
    let cwd = process.working_dir();
    let Some(next_cwd) = lookup_dir_at(cwd, path.as_str()) else {
        return Err(SysError::ENOENT);
    };
    let Some(next_path) = normalize_path(&process.working_dir_path(), path.as_str()) else {
        return Err(SysError::ENOENT);
    };
    process.set_working_dir(next_cwd, next_path);
    Ok(0)
}

pub fn sys_getcwd(buf: *mut u8, size: usize) -> SysResult {
    let process = current_process();
    let cwd_path = process.working_dir_path();
    copy_c_string_to_user(buf, size, cwd_path.as_str())
}

pub fn sys_fstat(fd: usize, statbuf: *mut LinuxKstat) -> SysResult {
    if statbuf.is_null() {
        return Err(SysError::EFAULT);
    }
    let token = current_user_token();
    let process = current_process();
    let inner = process.inner_exclusive_access();
    let Some(file) = inner
        .fd_table
        .get(fd)
        .and_then(|file| file.as_ref())
        .cloned()
    else {
        return Err(SysError::EBADF);
    };
    drop(inner);
    *translated_refmut(token, statbuf) = file.stat().into();
    Ok(0)
}

pub fn sys_mkdirat(dirfd: isize, path: *const u8, mode: u32) -> SysResult {
    let token = current_user_token();
    let path = translated_str(token, path);
    let base = path_base(dirfd, path.as_str())?;
    if mkdir_at(base, path.as_str(), mode).is_some() {
        Ok(0)
    } else {
        Err(SysError::ENOENT)
    }
}

pub fn sys_unlinkat(dirfd: isize, path: *const u8, flags: u32) -> SysResult {
    if flags != 0 {
        return Err(SysError::EINVAL);
    }
    let token = current_user_token();
    let path = translated_str(token, path);
    let base = path_base(dirfd, path.as_str())?;
    if unlink_file_at(base, path.as_str()).is_some() {
        Ok(0)
    } else {
        Err(SysError::ENOENT)
    }
}

pub fn sys_getdents64(fd: usize, buf: *mut u8, len: usize) -> SysResult {
    if len == 0 {
        return Err(SysError::EINVAL);
    }
    let token = current_user_token();
    let process = current_process();
    let inner = process.inner_exclusive_access();
    let Some(file) = inner
        .fd_table
        .get(fd)
        .and_then(|file| file.as_ref())
        .cloned()
    else {
        return Err(SysError::EBADF);
    };
    drop(inner);
    Ok(file.read_dirent64(UserBuffer::new(translated_byte_buffer(token, buf, len))))
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
