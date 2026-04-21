use crate::fs::{
    OpenFlags, WorkingDir, lookup_dir_at, make_pipe, mkdir_at, normalize_path, open_file_at,
    unlink_file_at,
};
use crate::mm::{UserBuffer, translated_byte_buffer, translated_refmut, translated_str};
use crate::task::{current_process, current_user_token};
use alloc::sync::Arc;

const AT_FDCWD: isize = -100;

fn dirfd_base(dirfd: isize) -> Option<WorkingDir> {
    let process = current_process();
    if dirfd == AT_FDCWD {
        return Some(process.working_dir());
    }
    if dirfd < 0 {
        return None;
    }
    let inner = process.inner_exclusive_access();
    let file = inner.fd_table.get(dirfd as usize)?.as_ref()?.clone();
    drop(inner);
    file.working_dir()
}

fn path_base(dirfd: isize, path: &str) -> Option<WorkingDir> {
    if path.starts_with('/') {
        Some(WorkingDir::root())
    } else {
        dirfd_base(dirfd)
    }
}

fn copy_c_string_to_user(ptr: *mut u8, buf_len: usize, string: &str) -> isize {
    let total_len = string.len() + 1;
    if buf_len < total_len {
        return 0;
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
    ptr as isize
}

pub fn sys_write(fd: usize, buf: *const u8, len: usize) -> isize {
    let token = current_user_token();
    let process = current_process();
    let inner = process.inner_exclusive_access();
    if fd >= inner.fd_table.len() {
        return -1;
    }
    if let Some(file) = &inner.fd_table[fd] {
        if !file.writable() {
            return -1;
        }
        let file = file.clone();
        // release current task TCB manually to avoid multi-borrow
        drop(inner);
        file.write(UserBuffer::new(translated_byte_buffer(token, buf, len))) as isize
    } else {
        -1
    }
}

pub fn sys_read(fd: usize, buf: *const u8, len: usize) -> isize {
    let token = current_user_token();
    let process = current_process();
    let inner = process.inner_exclusive_access();
    if fd >= inner.fd_table.len() {
        return -1;
    }
    if let Some(file) = &inner.fd_table[fd] {
        let file = file.clone();
        if !file.readable() {
            return -1;
        }
        // release current task TCB manually to avoid multi-borrow
        drop(inner);
        file.read(UserBuffer::new(translated_byte_buffer(token, buf, len))) as isize
    } else {
        -1
    }
}

pub fn sys_openat(dirfd: isize, path: *const u8, flags: u32, _mode: u32) -> isize {
    let token = current_user_token();
    let path = translated_str(token, path);
    let Some(flags) = OpenFlags::from_bits(flags) else {
        return -1;
    };
    if flags.bits() & 0b11 == 0b11 {
        return -1;
    }
    let Some(base) = path_base(dirfd, path.as_str()) else {
        return -1;
    };
    let process = current_process();
    let Some(inode) = open_file_at(base, path.as_str(), flags) else {
        return -1;
    };
    let mut inner = process.inner_exclusive_access();
    let fd = inner.alloc_fd();
    inner.fd_table[fd] = Some(inode);
    fd as isize
}

pub fn sys_chdir(path: *const u8) -> isize {
    let process = current_process();
    let token = current_user_token();
    let path = translated_str(token, path);
    let cwd = process.working_dir();
    let Some(next_cwd) = lookup_dir_at(cwd, path.as_str()) else {
        return -1;
    };
    let Some(next_path) = normalize_path(&process.working_dir_path(), path.as_str()) else {
        return -1;
    };
    process.set_working_dir(next_cwd, next_path);
    0
}

pub fn sys_getcwd(buf: *mut u8, size: usize) -> isize {
    let process = current_process();
    let cwd_path = process.working_dir_path();
    copy_c_string_to_user(buf, size, cwd_path.as_str())
}

pub fn sys_mkdirat(dirfd: isize, path: *const u8, mode: u32) -> isize {
    let token = current_user_token();
    let path = translated_str(token, path);
    let Some(base) = path_base(dirfd, path.as_str()) else {
        return -1;
    };
    if mkdir_at(base, path.as_str(), mode).is_some() {
        0
    } else {
        -1
    }
}

pub fn sys_unlinkat(dirfd: isize, path: *const u8, flags: u32) -> isize {
    if flags != 0 {
        return -1;
    }
    let token = current_user_token();
    let path = translated_str(token, path);
    let Some(base) = path_base(dirfd, path.as_str()) else {
        return -1;
    };
    if unlink_file_at(base, path.as_str()).is_some() {
        0
    } else {
        -1
    }
}

pub fn sys_close(fd: usize) -> isize {
    let process = current_process();
    let mut inner = process.inner_exclusive_access();
    if fd >= inner.fd_table.len() {
        return -1;
    }
    if inner.fd_table[fd].is_none() {
        return -1;
    }
    inner.fd_table[fd].take();
    0
}

pub fn sys_pipe(pipe: *mut usize) -> isize {
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
    0
}

pub fn sys_dup(fd: usize) -> isize {
    let process = current_process();
    let mut inner = process.inner_exclusive_access();
    if fd >= inner.fd_table.len() {
        return -1;
    }
    if inner.fd_table[fd].is_none() {
        return -1;
    }
    let new_fd = inner.alloc_fd();
    inner.fd_table[new_fd] = Some(Arc::clone(inner.fd_table[fd].as_ref().unwrap()));
    new_fd as isize
}
