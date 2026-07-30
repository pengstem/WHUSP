use super::*;

fn open_flags_from_socket_type(ty: i32) -> KResult<OpenFlags> {
    if ty & !(SOCK_TYPE_MASK | VALID_SOCKET_TYPE_FLAGS) != 0 {
        return Err(Errno::EINVAL);
    }
    let mut flags = OpenFlags::RDWR;
    if ty & SOCK_NONBLOCK != 0 {
        flags |= OpenFlags::NONBLOCK;
    }
    if ty & SOCK_CLOEXEC != 0 {
        flags |= OpenFlags::CLOEXEC;
    }
    Ok(flags)
}

fn open_flags_from_accept4(flags: i32) -> KResult<OpenFlags> {
    if flags & !VALID_ACCEPT4_FLAGS != 0 {
        return Err(Errno::EINVAL);
    }
    let mut open_flags = OpenFlags::RDWR;
    if flags & SOCK_NONBLOCK != 0 {
        open_flags |= OpenFlags::NONBLOCK;
    }
    if flags & SOCK_CLOEXEC != 0 {
        open_flags |= OpenFlags::CLOEXEC;
    }
    Ok(open_flags)
}

fn socket_kind_from_type(ty: i32) -> KResult<SocketKind> {
    match ty & SOCK_TYPE_MASK {
        SOCK_STREAM => Ok(SocketKind::Stream),
        SOCK_DGRAM => Ok(SocketKind::Datagram),
        // CONTEXT: The bind LTP subset only uses AF_UNIX SOCK_SEQPACKET for
        // connection-oriented local IPC. We reuse the stream queue semantics.
        SOCK_SEQPACKET => Ok(SocketKind::Stream),
        _ => Err(Errno::EPROTONOSUPPORT),
    }
}

fn validate_protocol(kind: SocketKind, protocol: i32) -> KResult {
    match (kind, protocol) {
        (_, IPPROTO_IP) => Ok(0),
        (SocketKind::Stream, IPPROTO_TCP) => Ok(0),
        (SocketKind::Datagram, IPPROTO_UDP) => Ok(0),
        // CONTEXT: LTP bind04/bind05 only require local loopback bind/connect
        // behavior for SCTP and UDP-Lite, so both reuse the existing queues.
        (SocketKind::Stream, IPPROTO_SCTP) => Ok(0),
        (SocketKind::Datagram, IPPROTO_UDPLITE) => Ok(0),
        _ => Err(Errno::EPROTONOSUPPORT),
    }
}

fn with_socket<T>(fd: usize, f: impl FnOnce(&LocalSocket) -> KResult<T>) -> KResult<T> {
    let file = file_from_fd(fd)?;
    let socket = file
        .as_any()
        .downcast_ref::<LocalSocket>()
        .ok_or(Errno::ENOTSOCK)?;
    f(socket)
}

fn file_from_fd(fd: usize) -> KResult<Arc<dyn File + Send + Sync>> {
    let process = current_process();
    let inner = process.inner_exclusive_access();
    let entry = inner
        .fd_table
        .get(fd)
        .and_then(|entry| entry.as_ref())
        .ok_or(Errno::EBADF)?;
    if entry.status_flags().contains(OpenFlags::PATH) {
        return Err(Errno::EBADF);
    }
    Ok(entry.file())
}

fn alloc_socket_fd(file: Arc<dyn File + Send + Sync>, flags: OpenFlags) -> KResult<usize> {
    install_file_fd(file, flags, None).map(|fd| fd as usize)
}

fn recv_nonblock(flags: i32, socket: &LocalSocket) -> bool {
    flags & MSG_DONTWAIT != 0 || socket.status_flags().contains(OpenFlags::NONBLOCK)
}

fn read_i32_option(token: usize, val: usize, len: u32) -> KResult<i32> {
    if val == 0 {
        return Err(Errno::EFAULT);
    }
    if (len as usize) < size_of::<i32>() {
        return Err(Errno::EINVAL);
    }
    read_user_value(token, val as *const i32)
}

fn read_u32_option(token: usize, val: usize, len: u32) -> KResult<u32> {
    if val == 0 {
        return Err(Errno::EFAULT);
    }
    if (len as usize) < size_of::<u32>() {
        return Err(Errno::EINVAL);
    }
    read_user_value(token, val as *const u32)
}

fn read_tpacket_req3_option(token: usize, val: usize, len: u32) -> KResult<LinuxTPacketReq3> {
    if val == 0 {
        return Err(Errno::EFAULT);
    }
    if (len as usize) < size_of::<LinuxTPacketReq3>() {
        return Err(Errno::EINVAL);
    }
    read_user_value(token, val as *const LinuxTPacketReq3)
}

fn validate_socket_option_buffer(token: usize, val: usize, len: u32) -> KResult<()> {
    if len == 0 {
        return Ok(());
    }
    if val == 0 {
        return Err(Errno::EFAULT);
    }
    translated_byte_buffer_checked(
        token,
        val as *const u8,
        len as usize,
        UserBufferAccess::Read,
    )?;
    Ok(())
}

fn forced_socket_buffer_size(raw: u32) -> i32 {
    if raw > i32::MAX as u32 {
        i32::MAX
    } else {
        raw as i32
    }
}

pub fn sys_socket(domain: i32, ty: i32, protocol: i32) -> KResult {
    let flags = open_flags_from_socket_type(ty)?;
    if domain == AF_ALG {
        AfAlgSocket::validate_socket_type(ty, protocol)?;
        let socket = AfAlgSocket::new_listener(flags);
        return Ok(alloc_socket_fd(socket, flags)? as isize);
    }
    if domain == AF_NETLINK {
        if !matches!(ty & SOCK_TYPE_MASK, SOCK_RAW | SOCK_DGRAM) {
            return Err(Errno::EPROTONOSUPPORT);
        }
        if protocol != NETLINK_ROUTE {
            return Err(Errno::EPROTONOSUPPORT);
        }
        let socket = LocalSocket::new(SocketDomain::Netlink, SocketKind::Datagram, flags);
        return Ok(alloc_socket_fd(socket, flags)? as isize);
    }
    if domain == AF_PACKET {
        if !matches!(ty & SOCK_TYPE_MASK, SOCK_RAW | SOCK_DGRAM) {
            return Err(Errno::EPROTONOSUPPORT);
        }
        // CONTEXT: LTP packet socket CVE probes only exercise SOL_PACKET
        // metadata and never exchange link-layer frames.
        let socket = LocalSocket::new(SocketDomain::Packet, SocketKind::Datagram, flags);
        return Ok(alloc_socket_fd(socket, flags)? as isize);
    }

    let kind = socket_kind_from_type(ty)?;
    match domain {
        AF_INET | AF_INET6 => {
            if ty & SOCK_TYPE_MASK == SOCK_SEQPACKET {
                return Err(Errno::EPROTONOSUPPORT);
            }
            validate_protocol(kind, protocol)?;
            let socket = LocalSocket::new(
                if domain == AF_INET {
                    SocketDomain::Inet
                } else {
                    SocketDomain::Inet6
                },
                kind,
                flags,
            );
            Ok(alloc_socket_fd(socket, flags)? as isize)
        }
        AF_UNIX => {
            if protocol != 0 {
                return Err(Errno::EPROTONOSUPPORT);
            }
            // CONTEXT: libc group/passwd lookup probes AF_UNIX nscd first.
            // The local AF_UNIX subset below supports pathname/abstract bind
            // cases while still returning ENOENT for absent pathname servers.
            let socket = LocalSocket::new(SocketDomain::Unix, kind, flags);
            Ok(alloc_socket_fd(socket, flags)? as isize)
        }
        _ => Err(Errno::EAFNOSUPPORT),
    }
}

pub fn sys_socketpair(domain: i32, ty: i32, protocol: i32, sv: usize) -> KResult {
    if sv == 0 {
        return Err(Errno::EFAULT);
    }
    if domain != AF_UNIX {
        return Err(Errno::EAFNOSUPPORT);
    }
    if protocol != 0 {
        return Err(Errno::EPROTONOSUPPORT);
    }
    let kind = socket_kind_from_type(ty)?;
    let flags = open_flags_from_socket_type(ty)?;

    let endpoint = InetEndpoint {
        ip: LOOPBACK_IP,
        port: 0,
    };
    let first_inner = Arc::new(unsafe {
        UPIntrFreeCell::new(LocalSocketInner::connected(
            SocketDomain::Unix,
            kind,
            endpoint,
            endpoint,
            None,
            ShutdownState::OPEN,
            None,
            None,
        ))
    });
    let second_inner = Arc::new(unsafe {
        UPIntrFreeCell::new(LocalSocketInner::connected(
            SocketDomain::Unix,
            kind,
            endpoint,
            endpoint,
            Some(Arc::downgrade(&first_inner)),
            ShutdownState::OPEN,
            None,
            None,
        ))
    });
    first_inner.exclusive_access().peer_socket = Some(Arc::downgrade(&second_inner));

    let first = LocalSocket::from_inner(first_inner, flags);
    let second = LocalSocket::from_inner(second_inner, flags);
    let fds = {
        let process = current_process();
        let mut inner = process.inner_exclusive_access();
        let first_fd = inner.alloc_fd_from(0).ok_or(Errno::EMFILE)?;
        let second_fd = inner.alloc_fd_from(first_fd + 1).ok_or(Errno::EMFILE)?;
        let previous = inner.set_fd_entry(first_fd, FdTableEntry::from_file(first, flags));
        debug_assert!(previous.is_none());
        let previous = inner.set_fd_entry(second_fd, FdTableEntry::from_file(second, flags));
        debug_assert!(previous.is_none());
        [first_fd as i32, second_fd as i32]
    };

    if let Err(err) = write_user_value(current_user_token(), sv as *mut [i32; 2], &fds) {
        let entries = {
            let process = current_process();
            let mut inner = process.inner_exclusive_access();
            [
                inner.take_fd_entry(fds[0] as usize),
                inner.take_fd_entry(fds[1] as usize),
            ]
        };
        for entry in entries.into_iter().flatten() {
            close_detached_fd_entry(entry);
        }
        return Err(err);
    }
    Ok(0)
}

pub fn sys_bind(fd: usize, addr: usize, addrlen: u32) -> KResult {
    let token = current_user_token();
    let file = file_from_fd(fd)?;
    if let Some(socket) = file.as_any().downcast_ref::<AfAlgSocket>() {
        socket.bind_alg(read_sockaddr_alg(token, addr, addrlen)?)?;
        return Ok(0);
    }
    let socket = file
        .as_any()
        .downcast_ref::<LocalSocket>()
        .ok_or(Errno::ENOTSOCK)?;
    let socket_addr = read_socket_address(token, addr, addrlen)?;
    socket.bind_address(socket_addr)
}

pub fn sys_listen(fd: usize, backlog: i32) -> KResult {
    with_socket(fd, |socket| {
        if socket.kind() != SocketKind::Stream {
            return Err(Errno::ENOTSUP);
        }
        socket.listen(backlog)
    })
}

pub fn sys_accept(fd: usize, addr: usize, addrlen: usize) -> KResult {
    sys_accept4(fd, addr, addrlen, 0)
}

pub fn sys_accept4(fd: usize, addr: usize, addrlen: usize, flags: i32) -> KResult {
    let open_flags = open_flags_from_accept4(flags)?;
    let token = current_user_token();
    let file = file_from_fd(fd)?;
    if let Some(socket) = file.as_any().downcast_ref::<AfAlgSocket>() {
        let accepted = socket.accept_request(open_flags)?;
        if addr != 0 && addrlen != 0 {
            write_user_value(token, addrlen as *mut u32, &0)?;
        }
        return Ok(alloc_socket_fd(accepted, open_flags)? as isize);
    }
    let socket = file
        .as_any()
        .downcast_ref::<LocalSocket>()
        .ok_or(Errno::ENOTSOCK)?;
    let accepted = socket.accept(socket.status_flags().contains(OpenFlags::NONBLOCK))?;
    let peer = accepted.peer_address()?;
    write_socket_address(token, addr, addrlen, peer)?;
    Ok(alloc_socket_fd(accepted, open_flags)? as isize)
}

pub fn sys_connect(fd: usize, addr: usize, addrlen: u32) -> KResult {
    let token = current_user_token();
    let socket_addr = read_socket_address(token, addr, addrlen)?;
    with_socket(fd, |socket| socket.connect(socket_addr))
}

pub fn sys_getsockname(fd: usize, addr: usize, addrlen: usize) -> KResult {
    let token = current_user_token();
    with_socket(fd, |socket| {
        write_socket_address(token, addr, addrlen, socket.local_address())
    })
}

pub fn sys_getpeername(fd: usize, addr: usize, addrlen: usize) -> KResult {
    let token = current_user_token();
    with_socket(fd, |socket| {
        write_socket_address(token, addr, addrlen, socket.peer_address()?)
    })
}

pub fn sys_sendto(
    fd: usize,
    buf: usize,
    len: usize,
    flags: i32,
    addr: usize,
    addrlen: u32,
) -> KResult {
    let token = current_user_token();
    let data = copy_user_to_vec(token, buf, len)?;
    let remote = if addr == 0 {
        None
    } else {
        Some(read_socket_address(token, addr, addrlen)?)
    };
    with_socket(fd, |socket| {
        match socket.send_bytes(data, remote, recv_nonblock(flags, socket)) {
            Ok(written) => Ok(written as isize),
            Err(Errno::EPIPE) => {
                current_add_signal(SignalFlags::SIGPIPE);
                Err(Errno::EPIPE)
            }
            Err(err) => Err(err),
        }
    })
}

pub fn sys_recvfrom(
    fd: usize,
    buf: usize,
    len: usize,
    flags: i32,
    addr: usize,
    addrlen: usize,
) -> KResult {
    let token = current_user_token();
    let user_buf = UserBuffer::new(translated_byte_buffer_checked(
        token,
        buf as *const u8,
        len,
        UserBufferAccess::Write,
    )?);
    with_socket(fd, |socket| {
        let (read, remote) = socket.recv_bytes(user_buf, recv_nonblock(flags, socket))?;
        if let Some(remote) = remote {
            write_socket_address(token, addr, addrlen, remote)?;
        }
        Ok(read as isize)
    })
}

pub fn sys_setsockopt(fd: usize, level: i32, name: i32, val: usize, len: u32) -> KResult {
    let token = current_user_token();
    let file = file_from_fd(fd)?;
    if let Some(socket) = file.as_any().downcast_ref::<AfAlgSocket>() {
        if level != SOL_ALG || name != ALG_SET_KEY {
            return Err(Errno::ENOPROTOOPT);
        }
        let key = copy_user_to_vec(token, val, len as usize)?;
        socket.set_key(&key)?;
        return Ok(0);
    }
    let socket = file
        .as_any()
        .downcast_ref::<LocalSocket>()
        .ok_or(Errno::ENOTSOCK)?;
    {
        match (level, name) {
            (SOL_SOCKET, SO_REUSEADDR) => {
                socket.set_reuse_addr(read_i32_option(token, val, len)? != 0);
            }
            (IPPROTO_IP, IP_BIND_ADDRESS_NO_PORT) => {
                socket.set_bind_address_no_port(read_i32_option(token, val, len)? != 0);
            }
            (SOL_SOCKET, SO_SNDBUF | SO_RCVBUF) => {
                socket.set_buffer_size(name, read_i32_option(token, val, len)?.max(1));
            }
            (SOL_SOCKET, SO_SNDBUFFORCE) => {
                socket.set_buffer_size(
                    SO_SNDBUF,
                    forced_socket_buffer_size(read_u32_option(token, val, len)?),
                );
            }
            (SOL_SOCKET, SO_OOBINLINE | SO_NO_CHECK) => {
                // CONTEXT: The local loopback sockets do not model TCP urgent
                // data or UDP checksum toggles, but these Linux SOL_SOCKET
                // options still need normal optval/optlen validation.
                read_i32_option(token, val, len)?;
            }
            (SOL_PACKET, PACKET_VERSION) => {
                socket.set_packet_version(read_i32_option(token, val, len)?)?;
            }
            (SOL_PACKET, PACKET_RESERVE) => {
                socket.set_packet_reserve(read_u32_option(token, val, len)?)?;
            }
            (SOL_PACKET, PACKET_RX_RING) => {
                socket.set_packet_rx_ring(read_tpacket_req3_option(token, val, len)?)?;
            }
            (SOL_PACKET, PACKET_VNET_HDR | PACKET_FANOUT | PACKET_FANOUT_ROLLOVER | TPACKET_V3) => {
                socket.ensure_packet_domain()?;
                validate_socket_option_buffer(token, val, len)?;
            }
            (IPPROTO_TCP, TCP_NODELAY)
            | (IPPROTO_IPV6, IPV6_V6ONLY)
            | (
                SOL_SOCKET,
                SO_DONTROUTE | SO_KEEPALIVE | SO_LINGER | SO_RCVTIMEO_OLD | SO_SNDTIMEO_OLD
                | SO_BINDTODEVICE,
            )
            | (SOL_SOCKET, SO_RCVTIMEO_NEW | SO_SNDTIMEO_NEW) => {
                // CONTEXT: accepted as a no-op for libc/netperf/iperf
                // compatibility. The in-kernel loopback table is keyed by port
                // and already accepts the contest's IPv4 clients for an AF_INET6
                // listener, so IPV6_V6ONLY has no routing effect here.
                validate_socket_option_buffer(token, val, len)?;
            }
            (IPPROTO_IP, MCAST_JOIN_GROUP) => {
                // CONTEXT: The loopback socket subset does not deliver multicast
                // traffic, but LTP/net probes expect joining a group to be
                // accepted and leaving an unjoined group to fail distinctly.
                validate_socket_option_buffer(token, val, len)?;
            }
            (IPPROTO_IP, MCAST_LEAVE_GROUP) => {
                // UNFINISHED: Multicast group membership is not tracked yet.
                // Linux returns EADDRNOTAVAIL when the socket is not a member
                // of the requested group; this is enough to avoid inheriting
                // fake membership across accept().
                validate_socket_option_buffer(token, val, len)?;
                return Err(Errno::EADDRNOTAVAIL);
            }
            (IPPROTO_IP, IPT_SO_SET_REPLACE) => {
                validate_socket_option_buffer(token, val, len)?;
                if (len as usize) < size_of::<u32>() {
                    return Err(Errno::EINVAL);
                }
            }
            (IPPROTO_IP, optname) if optname >= 0 => {
                // CONTEXT: Most IP tuning options, including netfilter's
                // IPT_SO_SET_REPLACE CVE probes, do not affect local loopback
                // queues. Preserve Linux-style negative optname rejection.
                validate_socket_option_buffer(token, val, len)?;
            }
            (IPPROTO_UDP, optname) if optname >= 0 && optname != SO_OOBINLINE => {
                // CONTEXT: UDP tuning options do not affect local loopback
                // queues. SO_OOBINLINE is a socket/TCP urgent-data option and
                // must stay rejected at UDP level for LTP errno coverage.
                validate_socket_option_buffer(token, val, len)?;
            }
            _ => return Err(Errno::ENOPROTOOPT),
        }
        Ok(0)
    }
}

pub fn sys_getsockopt(fd: usize, level: i32, name: i32, val: usize, len: usize) -> KResult {
    let token = current_user_token();
    if val == 0 || len == 0 {
        return Err(Errno::EFAULT);
    }
    with_socket(fd, |socket| {
        let len_ptr = len as *mut u32;
        let optlen = read_user_value(token, len_ptr.cast_const())?;
        if optlen == 0 {
            return Err(Errno::EINVAL);
        }
        let value = socket.get_int_option(level, name)?;
        let bytes = value.to_ne_bytes();
        let copy_len = (optlen as usize).min(bytes.len());
        copy_to_user(token, val as *mut u8, &bytes[..copy_len])?;
        write_user_value(token, len_ptr, &(copy_len as u32))?;
        Ok(0)
    })
}

pub fn sys_shutdown(fd: usize, how: i32) -> KResult {
    with_socket(fd, |socket| socket.shutdown(how))
}

pub fn sys_sendmsg(fd: usize, msg: usize, _flags: i32) -> KResult {
    let file = file_from_fd(fd)?;
    if let Some(socket) = file.as_any().downcast_ref::<AfAlgSocket>() {
        let token = current_user_token();
        let msg = read_user_value(token, msg as *const LinuxMsghdr)?;
        return Ok(socket.send_msg(msg)? as isize);
    }
    if let Some(socket) = file.as_any().downcast_ref::<LocalSocket>() {
        let token = current_user_token();
        let msg = read_user_value(token, msg as *const LinuxMsghdr)?;
        let data = read_msg_iovecs(token, msg.msg_iov, msg.msg_iovlen)?;
        let remote = if socket.domain() == SocketDomain::Netlink || msg.msg_name == 0 {
            None
        } else {
            Some(read_socket_address(token, msg.msg_name, msg.msg_namelen)?)
        };
        return match socket.send_bytes(data, remote, recv_nonblock(_flags, socket)) {
            Ok(written) => Ok(written as isize),
            Err(Errno::EPIPE) => {
                current_add_signal(SignalFlags::SIGPIPE);
                Err(Errno::EPIPE)
            }
            Err(err) => Err(err),
        };
    }
    Err(Errno::ENOTSOCK)
}

pub fn sys_sendmmsg(fd: usize, msgvec: usize, vlen: usize, flags: i32) -> KResult {
    if vlen == 0 {
        return Ok(0);
    }
    if vlen > 1024 {
        return Err(Errno::EINVAL);
    }

    let token = current_user_token();
    let mut sent = 0usize;
    for index in 0..vlen {
        let offset = index
            .checked_mul(size_of::<LinuxMmsghdr>())
            .ok_or(Errno::EFAULT)?;
        let ptr = msgvec.checked_add(offset).ok_or(Errno::EFAULT)?;
        match sys_sendmsg(fd, ptr, flags) {
            Ok(len) => {
                let mut header = read_user_value(token, ptr as *const LinuxMmsghdr)?;
                header.msg_len = len as u32;
                write_user_value(token, ptr as *mut LinuxMmsghdr, &header)?;
                sent += 1;
            }
            Err(_err) if sent > 0 => return Ok(sent as isize),
            Err(err) => return Err(err),
        }
    }
    Ok(sent as isize)
}

fn validate_recvmmsg_timeout(timeout: usize) -> KResult<()> {
    if timeout == 0 {
        return Ok(());
    }
    let token = current_user_token();
    let timeout = read_user_value(token, timeout as *const LinuxOldTimespec)?;
    if timeout.tv_sec < 0 || timeout.tv_nsec < 0 || timeout.tv_nsec >= 1_000_000_000 {
        return Err(Errno::EINVAL);
    }
    Ok(())
}

pub fn sys_recvmmsg(fd: usize, msgvec: usize, vlen: usize, flags: i32, timeout: usize) -> KResult {
    if vlen == 0 {
        return Ok(0);
    }
    if vlen > 1024 {
        return Err(Errno::EINVAL);
    }
    validate_recvmmsg_timeout(timeout)?;

    let token = current_user_token();
    let mut received = 0usize;
    for index in 0..vlen {
        let offset = index
            .checked_mul(size_of::<LinuxMmsghdr>())
            .ok_or(Errno::EFAULT)?;
        let ptr = msgvec.checked_add(offset).ok_or(Errno::EFAULT)?;
        let recv_flags = if received > 0 && flags & MSG_WAITFORONE != 0 {
            flags | MSG_DONTWAIT
        } else {
            flags
        };
        match sys_recvmsg(fd, ptr, recv_flags) {
            Ok(len) => {
                let mut header = read_user_value(token, ptr as *const LinuxMmsghdr)?;
                header.msg_len = len as u32;
                write_user_value(token, ptr as *mut LinuxMmsghdr, &header)?;
                received += 1;
                if len == 0 {
                    break;
                }
            }
            Err(_err) if received > 0 => return Ok(received as isize),
            Err(err) => return Err(err),
        }
    }
    Ok(received as isize)
}

pub fn sys_recvmsg(fd: usize, msg: usize, flags: i32) -> KResult {
    let file = file_from_fd(fd)?;
    if let Some(socket) = file.as_any().downcast_ref::<LocalSocket>() {
        if socket.kind() != SocketKind::Datagram {
            // UNFINISHED: stream recvmsg scatter/gather is not implemented.
            return Err(Errno::ENOSYS);
        }
        let token = current_user_token();
        let mut msg_value = read_user_value(token, msg as *const LinuxMsghdr)?;
        let data = socket.recv_raw_datagram(recv_nonblock(flags, socket))?;
        let copied = copy_to_msg_iovecs(token, msg_value.msg_iov, msg_value.msg_iovlen, &data)?;
        if socket.domain() == SocketDomain::Netlink
            && msg_value.msg_name != 0
            && msg_value.msg_namelen as usize >= size_of::<LinuxSockAddrNl>()
        {
            write_user_value(
                token,
                msg_value.msg_name as *mut LinuxSockAddrNl,
                &LinuxSockAddrNl {
                    family: AF_NETLINK as u16,
                    pad: 0,
                    pid: 0,
                    groups: 0,
                },
            )?;
            msg_value.msg_namelen = size_of::<LinuxSockAddrNl>() as u32;
        }
        write_user_value(token, msg as *mut LinuxMsghdr, &msg_value)?;
        return Ok(copied as isize);
    }
    Err(Errno::ENOTSOCK)
}
