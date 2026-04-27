use super::*;

bitflags! {
    pub struct OpenFlags: u32 {
        const RDONLY = 0;
        const WRONLY = 1 << 0;
        const RDWR = 1 << 1;
        const CREATE = 0o100;
        const TRUNC = 0o1000;
        const APPEND = 0o2000;
        const NONBLOCK = 0o4000;
        const DIRECT = 0o40000;
        const DIRECTORY = 0o200000;
        const CLOEXEC = 0o2000000;
    }
}

pub const F_DUPFD: usize = 0;
pub const F_GETFD: usize = 1;
pub const F_SETFD: usize = 2;
pub const F_GETFL: usize = 3;
pub const F_SETFL: usize = 4;
pub const F_GETLK: usize = 5;
pub const F_SETLK: usize = 6;
pub const F_SETLKW: usize = 7;
pub const F_DUPFD_CLOEXEC: usize = 1030;

pub const FD_CLOEXEC: usize = 1;

pub const F_RDLCK: i16 = 0;
pub const F_WRLCK: i16 = 1;
pub const F_UNLCK: i16 = 2;

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct Flock {
    pub l_type: i16,
    pub l_whence: i16,
    pub l_start: i64,
    pub l_len: i64,
    pub l_pid: i32,
}

const AT_FDCWD: isize = -100;

fn compat_ret(ret: isize) -> isize {
    if ret < 0 { -1 } else { ret }
}

pub fn dup(fd: usize) -> isize {
    compat_ret(sys_dup(fd))
}
pub fn dup3(old_fd: usize, new_fd: usize, flags: u32) -> isize {
    compat_ret(sys_dup3(old_fd, new_fd, flags))
}
pub fn fcntl(fd: usize, op: usize, arg: usize) -> isize {
    compat_ret(sys_fcntl(fd, op, arg))
}
pub fn ioctl(fd: usize, request: usize, argp: usize) -> isize {
    compat_ret(sys_ioctl(fd, request, argp))
}
pub fn open(path: &str, flags: OpenFlags) -> isize {
    compat_ret(sys_open(path, flags.bits))
}
pub fn openat(dirfd: isize, path: &str, flags: OpenFlags, mode: u32) -> isize {
    compat_ret(sys_openat(dirfd, path, flags.bits, mode))
}
pub fn getcwd(buf: &mut [u8]) -> isize {
    compat_ret(sys_getcwd(buf.as_mut_ptr(), buf.len()))
}
pub fn chdir(path: &str) -> isize {
    compat_ret(sys_chdir(path))
}
pub fn mkdir(path: &str, mode: u32) -> isize {
    compat_ret(sys_mkdirat(AT_FDCWD, path, mode))
}
pub fn unlink(path: &str) -> isize {
    compat_ret(sys_unlinkat(AT_FDCWD, path, 0))
}
pub fn mount(source: &str, target: &str, fstype: &str, flags: usize, data: *const u8) -> isize {
    compat_ret(sys_mount(source, target, fstype, flags, data))
}
pub fn umount(target: &str) -> isize {
    compat_ret(sys_umount2(target, 0))
}
pub fn umount2(target: &str, flags: i32) -> isize {
    compat_ret(sys_umount2(target, flags))
}
pub fn getdents64(fd: usize, buf: &mut [u8]) -> isize {
    compat_ret(sys_getdents64(fd, buf.as_mut_ptr(), buf.len()))
}
pub fn close(fd: usize) -> isize {
    compat_ret(sys_close(fd))
}
pub fn pipe(pipe_fd: &mut [i32; 2]) -> isize {
    compat_ret(sys_pipe2(pipe_fd, 0))
}
pub fn read(fd: usize, buf: &mut [u8]) -> isize {
    compat_ret(sys_read(fd, buf))
}
pub fn write(fd: usize, buf: &[u8]) -> isize {
    compat_ret(sys_write(fd, buf))
}
