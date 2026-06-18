use super::errno::SysResult;

pub fn sys_socket(domain: i32, ty: i32, protocol: i32) -> SysResult {
    crate::fs::socket::sys_socket(domain, ty, protocol)
}

pub fn sys_socketpair(domain: i32, ty: i32, protocol: i32, sv: usize) -> SysResult {
    crate::fs::socket::sys_socketpair(domain, ty, protocol, sv)
}

pub fn sys_bind(fd: usize, addr: usize, addrlen: u32) -> SysResult {
    crate::fs::socket::sys_bind(fd, addr, addrlen)
}

pub fn sys_listen(fd: usize, backlog: i32) -> SysResult {
    crate::fs::socket::sys_listen(fd, backlog)
}

pub fn sys_accept(fd: usize, addr: usize, addrlen: usize) -> SysResult {
    crate::fs::socket::sys_accept(fd, addr, addrlen)
}

pub fn sys_accept4(fd: usize, addr: usize, addrlen: usize, flags: i32) -> SysResult {
    crate::fs::socket::sys_accept4(fd, addr, addrlen, flags)
}

pub fn sys_connect(fd: usize, addr: usize, addrlen: u32) -> SysResult {
    crate::fs::socket::sys_connect(fd, addr, addrlen)
}

pub fn sys_getsockname(fd: usize, addr: usize, addrlen: usize) -> SysResult {
    crate::fs::socket::sys_getsockname(fd, addr, addrlen)
}

pub fn sys_getpeername(fd: usize, addr: usize, addrlen: usize) -> SysResult {
    crate::fs::socket::sys_getpeername(fd, addr, addrlen)
}

pub fn sys_sendto(
    fd: usize,
    buf: usize,
    len: usize,
    flags: i32,
    addr: usize,
    addrlen: u32,
) -> SysResult {
    crate::fs::socket::sys_sendto(fd, buf, len, flags, addr, addrlen)
}

pub fn sys_recvfrom(
    fd: usize,
    buf: usize,
    len: usize,
    flags: i32,
    addr: usize,
    addrlen: usize,
) -> SysResult {
    crate::fs::socket::sys_recvfrom(fd, buf, len, flags, addr, addrlen)
}

pub fn sys_setsockopt(fd: usize, level: i32, name: i32, val: usize, len: u32) -> SysResult {
    crate::fs::socket::sys_setsockopt(fd, level, name, val, len)
}

pub fn sys_getsockopt(fd: usize, level: i32, name: i32, val: usize, len: usize) -> SysResult {
    crate::fs::socket::sys_getsockopt(fd, level, name, val, len)
}

pub fn sys_shutdown(fd: usize, how: i32) -> SysResult {
    crate::fs::socket::sys_shutdown(fd, how)
}

pub fn sys_sendmsg(fd: usize, msg: usize, flags: i32) -> SysResult {
    crate::fs::socket::sys_sendmsg(fd, msg, flags)
}

pub fn sys_sendmmsg(fd: usize, msgvec: usize, vlen: usize, flags: i32) -> SysResult {
    crate::fs::socket::sys_sendmmsg(fd, msgvec, vlen, flags)
}

pub fn sys_recvmsg(fd: usize, msg: usize, flags: i32) -> SysResult {
    crate::fs::socket::sys_recvmsg(fd, msg, flags)
}

pub fn sys_recvmmsg(
    fd: usize,
    msgvec: usize,
    vlen: usize,
    flags: i32,
    timeout: usize,
) -> SysResult {
    crate::fs::socket::sys_recvmmsg(fd, msgvec, vlen, flags, timeout)
}
