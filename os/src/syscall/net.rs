//! Socket syscall stubs.
//!
//! The kernel does not implement a network stack (see CLAUDE.md — networking
//! modules were removed). To keep userspace cleanup paths well-behaved, these
//! handlers return concrete socket errnos instead of falling through to the
//! generic `ENOSYS`. In particular, glibc's `socket()` failure path on
//! `EAFNOSUPPORT` exits without dragging the process through pthread/futex
//! teardown that we cannot honor, which is what `netperf-glibc` exercises when
//! it tries to spawn `netserver`.

use super::errno::{SysError, SysResult};

pub fn sys_socket(_domain: i32, _ty: i32, _protocol: i32) -> SysResult {
    Err(SysError::EAFNOSUPPORT)
}

pub fn sys_socketpair(_domain: i32, _ty: i32, _protocol: i32, _sv: usize) -> SysResult {
    Err(SysError::EAFNOSUPPORT)
}

pub fn sys_bind(_fd: usize, _addr: usize, _addrlen: u32) -> SysResult {
    Err(SysError::ENOTSOCK)
}

pub fn sys_listen(_fd: usize, _backlog: i32) -> SysResult {
    Err(SysError::ENOTSOCK)
}

pub fn sys_accept(_fd: usize, _addr: usize, _addrlen: usize) -> SysResult {
    Err(SysError::ENOTSOCK)
}

pub fn sys_accept4(_fd: usize, _addr: usize, _addrlen: usize, _flags: i32) -> SysResult {
    Err(SysError::ENOTSOCK)
}

pub fn sys_connect(_fd: usize, _addr: usize, _addrlen: u32) -> SysResult {
    Err(SysError::ENOTSOCK)
}

pub fn sys_getsockname(_fd: usize, _addr: usize, _addrlen: usize) -> SysResult {
    Err(SysError::ENOTSOCK)
}

pub fn sys_getpeername(_fd: usize, _addr: usize, _addrlen: usize) -> SysResult {
    Err(SysError::ENOTSOCK)
}

pub fn sys_sendto(
    _fd: usize,
    _buf: usize,
    _len: usize,
    _flags: i32,
    _addr: usize,
    _addrlen: u32,
) -> SysResult {
    Err(SysError::ENOTSOCK)
}

pub fn sys_recvfrom(
    _fd: usize,
    _buf: usize,
    _len: usize,
    _flags: i32,
    _addr: usize,
    _addrlen: usize,
) -> SysResult {
    Err(SysError::ENOTSOCK)
}

pub fn sys_setsockopt(_fd: usize, _level: i32, _name: i32, _val: usize, _len: u32) -> SysResult {
    Err(SysError::ENOTSOCK)
}

pub fn sys_getsockopt(_fd: usize, _level: i32, _name: i32, _val: usize, _len: usize) -> SysResult {
    Err(SysError::ENOTSOCK)
}

pub fn sys_shutdown(_fd: usize, _how: i32) -> SysResult {
    Err(SysError::ENOTSOCK)
}

pub fn sys_sendmsg(_fd: usize, _msg: usize, _flags: i32) -> SysResult {
    Err(SysError::ENOTSOCK)
}

pub fn sys_recvmsg(_fd: usize, _msg: usize, _flags: i32) -> SysResult {
    Err(SysError::ENOTSOCK)
}
