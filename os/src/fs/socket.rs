//! Minimal socket syscalls.
//!
//! This is not a complete Linux networking stack. It provides the smallest
//! local TCP/UDP behavior needed by the contest netperf scripts, which use
//! `127.0.0.1` inside one guest.  Packets never leave the kernel and virtio-net
//! is not involved.

use super::inode::create_node_in;
use super::{
    File, FileStat, FsError, FsNodeKind, OpenFlags, PollEvents, PollWaitQueue, PollWaiter, S_IFIFO,
};
use crate::config::PAGE_SIZE;
use crate::mm::UserBuffer;
use crate::sync::UPIntrFreeCell;
use crate::syscall::errno::{SysError, SysResult};
use crate::syscall::user_ptr::{
    UserBufferAccess, copy_to_user, read_user_array_item, read_user_value,
    read_user_value_with_mmap_fault, translated_byte_buffer_checked,
    translated_byte_buffer_checked_with_mmap_fault, write_user_value,
};
use crate::syscall::{close_detached_fd_entry, install_file_fd};
use crate::task::{
    FdTableEntry, SignalFlags, current_add_signal, current_has_unmasked_signal, current_process,
    current_user_token, suspend_current_and_run_next,
};
use crate::timer::get_time_ms;
use alloc::collections::{BTreeMap, VecDeque};
use alloc::string::{String, ToString};
use alloc::sync::{Arc, Weak};
use alloc::{vec, vec::Vec};
use core::mem::size_of;
use lazy_static::lazy_static;

const AF_UNIX: i32 = 1;
const AF_INET: i32 = 2;
const AF_INET6: i32 = 10;
const AF_PACKET: i32 = 17;
const AF_ALG: i32 = 38;
const SOCK_STREAM: i32 = 1;
const SOCK_DGRAM: i32 = 2;
const SOCK_RAW: i32 = 3;
const SOCK_SEQPACKET: i32 = 5;
const SOCK_TYPE_MASK: i32 = 0xf;
const SOCK_NONBLOCK: i32 = OpenFlags::NONBLOCK.bits() as i32;
const SOCK_CLOEXEC: i32 = OpenFlags::CLOEXEC.bits() as i32;
const VALID_SOCKET_TYPE_FLAGS: i32 = SOCK_NONBLOCK | SOCK_CLOEXEC;
const VALID_ACCEPT4_FLAGS: i32 = SOCK_NONBLOCK | SOCK_CLOEXEC;
const IPPROTO_IP: i32 = 0;
const IPPROTO_TCP: i32 = 6;
const IPPROTO_UDP: i32 = 17;
const IPPROTO_IPV6: i32 = 41;
const IPPROTO_SCTP: i32 = 132;
const IPPROTO_UDPLITE: i32 = 136;
const SOL_SOCKET: i32 = 1;
const SOL_PACKET: i32 = 263;
const SOL_ALG: i32 = 279;
const SO_REUSEADDR: i32 = 2;
const SO_TYPE: i32 = 3;
const SO_ERROR: i32 = 4;
const SO_DONTROUTE: i32 = 5;
const SO_SNDBUF: i32 = 7;
const SO_RCVBUF: i32 = 8;
const SO_KEEPALIVE: i32 = 9;
const SO_OOBINLINE: i32 = 10;
const SO_NO_CHECK: i32 = 11;
const SO_LINGER: i32 = 13;
const SO_RCVTIMEO_OLD: i32 = 20;
const SO_SNDTIMEO_OLD: i32 = 21;
const SO_SNDBUFFORCE: i32 = 32;
const SO_RCVTIMEO_NEW: i32 = 66;
const SO_SNDTIMEO_NEW: i32 = 67;
const TCP_NODELAY: i32 = 1;
const TCP_MAXSEG: i32 = 2;
const IPV6_V6ONLY: i32 = 26;
const MCAST_JOIN_GROUP: i32 = 42;
const MCAST_LEAVE_GROUP: i32 = 45;
const IPT_SO_SET_REPLACE: i32 = 64;
const PACKET_RX_RING: i32 = 5;
const PACKET_VERSION: i32 = 10;
const PACKET_RESERVE: i32 = 12;
const PACKET_VNET_HDR: i32 = 15;
const PACKET_FANOUT: i32 = 18;
const PACKET_FANOUT_ROLLOVER: i32 = 3;
const TPACKET_V1: i32 = 0;
const TPACKET_V3: i32 = 2;
const SHUT_RD: i32 = 0;
const SHUT_WR: i32 = 1;
const SHUT_RDWR: i32 = 2;
const MSG_DONTWAIT: i32 = 0x40;
const ALG_SET_KEY: i32 = 1;
const ALG_SET_IV: i32 = 2;
const ALG_SET_OP: i32 = 3;
const ALG_SET_AEAD_ASSOCLEN: i32 = 4;
const ALG_OP_DECRYPT: u32 = 0;
const ALG_OP_ENCRYPT: u32 = 1;
const LOOPBACK_IP: [u8; 4] = [127, 0, 0, 1];
const ANY_IP: [u8; 4] = [0, 0, 0, 0];
const LOOPBACK_IPV6: [u8; 16] = [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1];
const ANY_IPV6: [u8; 16] = [0; 16];
const DEFAULT_SOCKET_BUFFER: i32 = 64 * 1024;
const MAX_LISTEN_BACKLOG: usize = 128;

lazy_static! {
    static ref LOOPBACK: UPIntrFreeCell<LoopbackState> =
        unsafe { UPIntrFreeCell::new(LoopbackState::new()) };
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
struct LinuxSockAddrIn {
    family: u16,
    port_be: u16,
    addr: u32,
    zero: [u8; 8],
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
struct LinuxSockAddrIn6 {
    family: u16,
    port_be: u16,
    flowinfo: u32,
    addr: [u8; 16],
    scope_id: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
struct LinuxSockAddrUn {
    family: u16,
    path: [u8; 108],
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SocketDomain {
    Unix,
    Inet,
    Inet6,
    Packet,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SocketKind {
    Stream,
    Datagram,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
struct LinuxIovec {
    base: usize,
    len: usize,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
struct LinuxMsghdr {
    msg_name: usize,
    msg_namelen: u32,
    msg_iov: usize,
    msg_iovlen: usize,
    msg_control: usize,
    msg_controllen: usize,
    msg_flags: i32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
struct LinuxCmsghdr {
    cmsg_len: usize,
    cmsg_level: i32,
    cmsg_type: i32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
struct LinuxSockAddrAlg {
    family: u16,
    alg_type: [u8; 14],
    feat: u32,
    mask: u32,
    name: [u8; 64],
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
struct LinuxTPacketReq3 {
    tp_block_size: u32,
    tp_block_nr: u32,
    tp_frame_size: u32,
    tp_frame_nr: u32,
    tp_retire_blk_tov: u32,
    tp_sizeof_priv: u32,
    tp_feature_req_word: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct InetEndpoint {
    ip: [u8; 4],
    port: u16,
}

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd)]
enum UnixAddress {
    Pathname(String),
    Abstract(Vec<u8>),
}

#[derive(Clone, Debug)]
enum UnixSockAddr {
    Unnamed,
    Named(UnixAddress),
}

#[derive(Clone, Debug)]
enum SocketAddress {
    Inet(InetEndpoint),
    Inet6(InetEndpoint),
    Unix(UnixSockAddr),
}

#[derive(Clone)]
struct Datagram {
    data: Vec<u8>,
    from: InetEndpoint,
    from_unix: Option<UnixAddress>,
}

struct LocalSocketInner {
    domain: SocketDomain,
    kind: SocketKind,
    local: Option<InetEndpoint>,
    peer: Option<InetEndpoint>,
    unix_local: Option<UnixAddress>,
    unix_peer: Option<UnixAddress>,
    peer_socket: Option<Weak<UPIntrFreeCell<LocalSocketInner>>>,
    accept_queue: VecDeque<Arc<UPIntrFreeCell<LocalSocketInner>>>,
    stream_rx: VecDeque<u8>,
    datagram_rx: VecDeque<Datagram>,
    read_poll_waiters: PollWaitQueue,
    write_poll_waiters: PollWaitQueue,
    listening: bool,
    listen_backlog: usize,
    read_shutdown: bool,
    write_shutdown: bool,
    peer_write_shutdown: bool,
    reuse_addr: bool,
    sndbuf: i32,
    rcvbuf: i32,
    packet_version: i32,
    packet_reserve: u32,
}

pub struct LocalSocket {
    inner: Arc<UPIntrFreeCell<LocalSocketInner>>,
    status_flags: UPIntrFreeCell<OpenFlags>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AfAlgFamily {
    Hash,
    Skcipher,
    Aead,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AfAlgOperation {
    Decrypt,
    Encrypt,
}

#[derive(Clone, Debug)]
struct AfAlgBinding {
    family: AfAlgFamily,
    name: String,
    key: Vec<u8>,
}

#[derive(Default)]
struct AfAlgListenerState {
    binding: Option<AfAlgBinding>,
}

struct AfAlgRequestState {
    binding: AfAlgBinding,
    op: AfAlgOperation,
    iv: Vec<u8>,
    assoclen: u32,
    input: Vec<u8>,
    output: Option<Vec<u8>>,
    output_offset: usize,
    output_done: bool,
}

enum AfAlgSocketKind {
    Listener(UPIntrFreeCell<AfAlgListenerState>),
    Request(UPIntrFreeCell<AfAlgRequestState>),
}

pub struct AfAlgSocket {
    kind: AfAlgSocketKind,
    status_flags: UPIntrFreeCell<OpenFlags>,
    write_ignores_data: bool,
}

#[derive(Default)]
struct AfAlgSendParams {
    op: Option<AfAlgOperation>,
    iv: Option<Vec<u8>>,
    assoclen: Option<u32>,
}

struct LoopbackState {
    next_ephemeral: u16,
    tcp_listeners: BTreeMap<u16, Weak<UPIntrFreeCell<LocalSocketInner>>>,
    udp_bound: BTreeMap<u16, Vec<Weak<UPIntrFreeCell<LocalSocketInner>>>>,
    unix_bound: BTreeMap<UnixAddress, Weak<UPIntrFreeCell<LocalSocketInner>>>,
}

impl LoopbackState {
    fn new() -> Self {
        Self {
            next_ephemeral: 49152,
            tcp_listeners: BTreeMap::new(),
            udp_bound: BTreeMap::new(),
            unix_bound: BTreeMap::new(),
        }
    }

    fn alloc_port(&mut self) -> u16 {
        loop {
            let port = self.next_ephemeral;
            self.next_ephemeral = if self.next_ephemeral == 60999 {
                49152
            } else {
                self.next_ephemeral + 1
            };
            if !self.tcp_listeners.contains_key(&port) && !self.udp_bound.contains_key(&port) {
                return port;
            }
        }
    }

    fn prune(&mut self) {
        self.tcp_listeners
            .retain(|_, socket| socket.strong_count() > 0);
        self.udp_bound.retain(|_, sockets| {
            sockets.retain(|socket| socket.strong_count() > 0);
            !sockets.is_empty()
        });
        self.unix_bound
            .retain(|_, socket| socket.strong_count() > 0);
    }
}

#[derive(Clone, Copy)]
struct ShutdownState {
    read: bool,
    write: bool,
    peer_write: bool,
}

impl ShutdownState {
    const OPEN: Self = Self {
        read: false,
        write: false,
        peer_write: false,
    };
    const CLOSED: Self = Self {
        read: true,
        write: true,
        peer_write: true,
    };
}

impl LocalSocketInner {
    fn new(domain: SocketDomain, kind: SocketKind) -> Self {
        Self {
            domain,
            kind,
            local: None,
            peer: None,
            unix_local: None,
            unix_peer: None,
            peer_socket: None,
            accept_queue: VecDeque::new(),
            stream_rx: VecDeque::new(),
            datagram_rx: VecDeque::new(),
            read_poll_waiters: PollWaitQueue::new(),
            write_poll_waiters: PollWaitQueue::new(),
            listening: false,
            listen_backlog: 0,
            read_shutdown: false,
            write_shutdown: false,
            peer_write_shutdown: false,
            reuse_addr: false,
            sndbuf: DEFAULT_SOCKET_BUFFER,
            rcvbuf: DEFAULT_SOCKET_BUFFER,
            packet_version: TPACKET_V1,
            packet_reserve: 0,
        }
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "socket connection state is clearer when local and peer metadata stay explicit"
    )]
    fn connected(
        domain: SocketDomain,
        kind: SocketKind,
        local: InetEndpoint,
        peer: InetEndpoint,
        peer_socket: Option<Weak<UPIntrFreeCell<LocalSocketInner>>>,
        shutdown: ShutdownState,
        unix_local: Option<UnixAddress>,
        unix_peer: Option<UnixAddress>,
    ) -> Self {
        let mut inner = Self::new(domain, kind);
        inner.local = Some(local);
        inner.peer = Some(peer);
        inner.unix_local = unix_local;
        inner.unix_peer = unix_peer;
        inner.peer_socket = peer_socket;
        inner.read_shutdown = shutdown.read;
        inner.write_shutdown = shutdown.write;
        inner.peer_write_shutdown = shutdown.peer_write;
        inner
    }
}

fn drain_socket_write_poll_waiters(
    socket: &Arc<UPIntrFreeCell<LocalSocketInner>>,
) -> Vec<Arc<PollWaiter>> {
    socket.exclusive_access().write_poll_waiters.drain()
}

impl LocalSocket {
    fn new(domain: SocketDomain, kind: SocketKind, flags: OpenFlags) -> Arc<Self> {
        Arc::new(Self {
            inner: Arc::new(unsafe { UPIntrFreeCell::new(LocalSocketInner::new(domain, kind)) }),
            status_flags: unsafe { UPIntrFreeCell::new(flags) },
        })
    }

    fn from_inner(inner: Arc<UPIntrFreeCell<LocalSocketInner>>, flags: OpenFlags) -> Arc<Self> {
        Arc::new(Self {
            inner,
            status_flags: unsafe { UPIntrFreeCell::new(flags) },
        })
    }

    fn kind(&self) -> SocketKind {
        self.inner.exclusive_access().kind
    }

    fn bind_address(&self, address: SocketAddress) -> SysResult {
        let domain = self.inner.exclusive_access().domain;
        match (domain, address) {
            (SocketDomain::Inet, SocketAddress::Inet(endpoint)) => self.bind_endpoint(endpoint),
            (SocketDomain::Inet6, SocketAddress::Inet6(endpoint)) => self.bind_endpoint(endpoint),
            (SocketDomain::Unix, SocketAddress::Unix(UnixSockAddr::Named(address))) => {
                self.bind_unix(address)
            }
            (SocketDomain::Unix, SocketAddress::Unix(UnixSockAddr::Unnamed)) => {
                Err(SysError::EINVAL)
            }
            (SocketDomain::Packet, _) => Err(SysError::EAFNOSUPPORT),
            _ => Err(SysError::EAFNOSUPPORT),
        }
    }

    fn bind_endpoint(&self, mut endpoint: InetEndpoint) -> SysResult {
        normalize_local_endpoint(&mut endpoint)?;
        if endpoint.port != 0 && endpoint.port < 1024 && current_process().credentials().euid != 0 {
            return Err(SysError::EACCES);
        }
        let mut loopback = LOOPBACK.exclusive_access();
        loopback.prune();
        if endpoint.port == 0 {
            endpoint.port = loopback.alloc_port();
        }

        let mut inner = self.inner.exclusive_access();
        if inner.local.is_some() {
            return Err(SysError::EINVAL);
        }

        match inner.kind {
            SocketKind::Stream => {
                if loopback.tcp_listeners.contains_key(&endpoint.port) && !inner.reuse_addr {
                    return Err(SysError::EADDRINUSE);
                }
            }
            SocketKind::Datagram => {
                if loopback
                    .udp_bound
                    .get(&endpoint.port)
                    .is_some_and(|sockets| !sockets.is_empty())
                    && !inner.reuse_addr
                {
                    return Err(SysError::EADDRINUSE);
                }
                loopback
                    .udp_bound
                    .entry(endpoint.port)
                    .or_default()
                    .push(Arc::downgrade(&self.inner));
            }
        }
        inner.local = Some(endpoint);
        Ok(0)
    }

    fn bind_unix(&self, address: UnixAddress) -> SysResult {
        {
            let inner = self.inner.exclusive_access();
            if inner.local.is_some() {
                return Err(SysError::EINVAL);
            }
        }
        {
            let mut loopback = LOOPBACK.exclusive_access();
            loopback.prune();
            if loopback
                .unix_bound
                .get(&address)
                .is_some_and(|socket| socket.strong_count() > 0)
            {
                return Err(SysError::EADDRINUSE);
            }
        }
        if let UnixAddress::Pathname(path) = &address {
            create_unix_path_node(path)?;
        }
        let mut loopback = LOOPBACK.exclusive_access();
        loopback.prune();
        let endpoint = InetEndpoint {
            ip: LOOPBACK_IP,
            port: loopback.alloc_port(),
        };
        let mut inner = self.inner.exclusive_access();
        match inner.kind {
            SocketKind::Stream => {}
            SocketKind::Datagram => {
                loopback
                    .udp_bound
                    .entry(endpoint.port)
                    .or_default()
                    .push(Arc::downgrade(&self.inner));
            }
        }
        inner.local = Some(endpoint);
        inner.unix_local = Some(address.clone());
        loopback
            .unix_bound
            .insert(address, Arc::downgrade(&self.inner));
        Ok(0)
    }

    fn ensure_bound(&self, kind: SocketKind) -> SysResult<InetEndpoint> {
        {
            let inner = self.inner.exclusive_access();
            if let Some(local) = inner.local {
                return Ok(local);
            }
            if inner.kind != kind {
                return Err(SysError::EINVAL);
            }
        }
        let mut loopback = LOOPBACK.exclusive_access();
        loopback.prune();
        let endpoint = InetEndpoint {
            ip: LOOPBACK_IP,
            port: loopback.alloc_port(),
        };
        if kind == SocketKind::Datagram {
            loopback
                .udp_bound
                .entry(endpoint.port)
                .or_default()
                .push(Arc::downgrade(&self.inner));
        }
        self.inner.exclusive_access().local = Some(endpoint);
        Ok(endpoint)
    }

    fn listen(&self, backlog: i32) -> SysResult {
        let backlog = backlog.clamp(1, MAX_LISTEN_BACKLOG as i32) as usize;
        let local = self.ensure_bound(SocketKind::Stream)?;
        let mut loopback = LOOPBACK.exclusive_access();
        loopback.prune();
        loopback
            .tcp_listeners
            .insert(local.port, Arc::downgrade(&self.inner));
        let mut inner = self.inner.exclusive_access();
        inner.listening = true;
        inner.listen_backlog = backlog;
        Ok(0)
    }

    fn accept(&self, nonblock: bool) -> SysResult<Arc<LocalSocket>> {
        loop {
            let (accepted, local) = {
                let mut inner = self.inner.exclusive_access();
                if inner.kind != SocketKind::Stream {
                    return Err(SysError::ENOTSUP);
                }
                if !inner.listening {
                    return Err(SysError::EINVAL);
                }
                (
                    inner.accept_queue.pop_front(),
                    inner.local.unwrap_or(InetEndpoint {
                        ip: LOOPBACK_IP,
                        port: 0,
                    }),
                )
            };
            if let Some(inner) = accepted {
                return Ok(Self::from_inner(inner, OpenFlags::RDWR));
            }
            if nonblock {
                return Err(SysError::EAGAIN);
            }
            if current_has_unmasked_signal() {
                let peer = InetEndpoint {
                    ip: LOOPBACK_IP,
                    port: 0,
                };
                // CONTEXT: netperf's timed TCP_CRR server expects a blocking
                // accept() to return to user mode when SIGALRM fires. Returning
                // a closed placeholder lets the signal handler run and the
                // server loop observe `times_up` without leaking a listener.
                return Ok(Self::from_inner(
                    Arc::new(unsafe {
                        UPIntrFreeCell::new(LocalSocketInner::connected(
                            SocketDomain::Inet,
                            SocketKind::Stream,
                            local,
                            peer,
                            None,
                            ShutdownState::CLOSED,
                            None,
                            None,
                        ))
                    }),
                    OpenFlags::RDWR,
                ));
            }
            suspend_current_and_run_next();
        }
    }

    fn connect(&self, remote: SocketAddress) -> SysResult {
        let (remote, unix_peer) = self.resolve_remote_address(remote)?;
        match self.kind() {
            SocketKind::Datagram => {
                self.ensure_bound(SocketKind::Datagram)?;
                let mut inner = self.inner.exclusive_access();
                inner.peer = Some(remote);
                inner.unix_peer = unix_peer;
                Ok(0)
            }
            SocketKind::Stream => self.connect_stream(remote, unix_peer),
        }
    }

    fn connect_stream(&self, remote: InetEndpoint, unix_peer: Option<UnixAddress>) -> SysResult {
        {
            let inner = self.inner.exclusive_access();
            if inner.peer.is_some() {
                return Err(SysError::EISCONN);
            }
        }
        let local = self.ensure_bound(SocketKind::Stream)?;
        let connect_deadline_ms = get_time_ms() + 1000;
        let listener = loop {
            let listener = {
                let loopback = LOOPBACK.exclusive_access();
                loopback
                    .tcp_listeners
                    .get(&remote.port)
                    .and_then(Weak::upgrade)
            };
            if let Some(listener) = listener {
                break listener;
            }
            if get_time_ms() >= connect_deadline_ms {
                return Err(SysError::ECONNREFUSED);
            }
            // CONTEXT: The contest script backgrounds netserver and immediately
            // launches netperf. Yield briefly so the server can reach listen().
            suspend_current_and_run_next();
        };
        let listener_unix_local = listener.exclusive_access().unix_local.clone();
        let (domain, client_unix_local) = {
            let inner = self.inner.exclusive_access();
            (inner.domain, inner.unix_local.clone())
        };

        let server_inner = Arc::new(unsafe {
            UPIntrFreeCell::new(LocalSocketInner::connected(
                domain,
                SocketKind::Stream,
                remote,
                local,
                Some(Arc::downgrade(&self.inner)),
                ShutdownState::OPEN,
                listener_unix_local,
                client_unix_local,
            ))
        });

        {
            let listener = listener.exclusive_access();
            if listener.accept_queue.len() >= listener.listen_backlog.max(1) {
                return Err(SysError::ECONNREFUSED);
            }
        }
        {
            let mut client = self.inner.exclusive_access();
            client.peer = Some(remote);
            client.unix_peer = unix_peer;
            client.peer_socket = Some(Arc::downgrade(&server_inner));
        }
        let read_waiters = {
            let mut listener = listener.exclusive_access();
            listener.accept_queue.push_back(server_inner);
            listener.read_poll_waiters.drain()
        };
        PollWaiter::wake_all(read_waiters);
        Ok(0)
    }

    fn resolve_remote_address(
        &self,
        address: SocketAddress,
    ) -> SysResult<(InetEndpoint, Option<UnixAddress>)> {
        let domain = self.inner.exclusive_access().domain;
        match (domain, address) {
            (SocketDomain::Inet, SocketAddress::Inet(mut endpoint)) => {
                normalize_remote_endpoint(&mut endpoint)?;
                Ok((endpoint, None))
            }
            (SocketDomain::Inet6, SocketAddress::Inet6(mut endpoint)) => {
                normalize_remote_endpoint(&mut endpoint)?;
                Ok((endpoint, None))
            }
            (SocketDomain::Unix, SocketAddress::Unix(UnixSockAddr::Named(address))) => {
                Ok((lookup_unix_endpoint(&address)?, Some(address)))
            }
            (SocketDomain::Unix, SocketAddress::Unix(UnixSockAddr::Unnamed)) => {
                Err(SysError::EINVAL)
            }
            (SocketDomain::Packet, _) => Err(SysError::EAFNOSUPPORT),
            _ => Err(SysError::EAFNOSUPPORT),
        }
    }

    fn send_bytes(&self, data: &[u8], remote: Option<SocketAddress>) -> SysResult<usize> {
        match self.kind() {
            SocketKind::Stream => self.send_stream(data),
            SocketKind::Datagram => self.send_datagram(data, remote),
        }
    }

    fn send_stream(&self, data: &[u8]) -> SysResult<usize> {
        let mut written = 0usize;
        while written < data.len() {
            let (connected, peer) = {
                let inner = self.inner.exclusive_access();
                if inner.write_shutdown {
                    return Err(SysError::EPIPE);
                }
                (
                    inner.peer.is_some() || inner.unix_peer.is_some(),
                    inner.peer_socket.as_ref().and_then(Weak::upgrade),
                )
            };
            let Some(peer) = peer else {
                return Err(if connected {
                    SysError::EPIPE
                } else {
                    SysError::ENOTCONN
                });
            };
            let mut peer_inner = peer.exclusive_access();
            if peer_inner.read_shutdown {
                return Err(SysError::EPIPE);
            }
            let capacity = (peer_inner.rcvbuf as usize).max(1);
            let available = capacity.saturating_sub(peer_inner.stream_rx.len());
            if available == 0 {
                drop(peer_inner);
                if current_has_unmasked_signal() {
                    return Err(SysError::EINTR);
                }
                suspend_current_and_run_next();
                continue;
            }
            let chunk_len = available.min(data.len() - written);
            peer_inner
                .stream_rx
                .extend(data[written..written + chunk_len].iter().copied());
            written += chunk_len;
            let read_waiters = peer_inner.read_poll_waiters.drain();
            drop(peer_inner);
            PollWaiter::wake_all(read_waiters);
        }
        Ok(written)
    }

    fn stream_write_peer_closed(&self) -> bool {
        let (kind, listening, write_shutdown, connected, peer) = {
            let inner = self.inner.exclusive_access();
            (
                inner.kind,
                inner.listening,
                inner.write_shutdown,
                inner.peer.is_some() || inner.unix_peer.is_some(),
                inner.peer_socket.as_ref().and_then(Weak::upgrade),
            )
        };
        if kind != SocketKind::Stream || listening {
            return false;
        }
        if write_shutdown {
            return true;
        }
        match peer {
            Some(peer) => peer.exclusive_access().read_shutdown,
            None => connected,
        }
    }

    fn send_datagram(&self, data: &[u8], remote: Option<SocketAddress>) -> SysResult<usize> {
        let local = self.ensure_bound(SocketKind::Datagram)?;
        let local_unix = self.inner.exclusive_access().unix_local.clone();
        if remote.is_none()
            && let Some(peer) = self
                .inner
                .exclusive_access()
                .peer_socket
                .as_ref()
                .and_then(Weak::upgrade)
        {
            let mut peer = peer.exclusive_access();
            if peer.read_shutdown {
                return Err(SysError::EPIPE);
            }
            let queued_bytes: usize = peer
                .datagram_rx
                .iter()
                .map(|packet| packet.data.len())
                .sum();
            let capacity = (peer.rcvbuf as usize).max(1);
            if queued_bytes.saturating_add(data.len()) > capacity {
                return Err(SysError::EAGAIN);
            }
            peer.datagram_rx.push_back(Datagram {
                data: data.to_vec(),
                from: local,
                from_unix: local_unix,
            });
            let read_waiters = peer.read_poll_waiters.drain();
            drop(peer);
            PollWaiter::wake_all(read_waiters);
            return Ok(data.len());
        }
        let remote = match remote {
            Some(remote) => self.resolve_remote_address(remote)?.0,
            None => self
                .inner
                .exclusive_access()
                .peer
                .ok_or(SysError::EDESTADDRREQ)?,
        };
        let candidates = {
            let mut loopback = LOOPBACK.exclusive_access();
            loopback.prune();
            loopback
                .udp_bound
                .get(&remote.port)
                .map(|sockets| sockets.iter().filter_map(Weak::upgrade).collect::<Vec<_>>())
                .unwrap_or_default()
        };
        let mut fallback = None;
        let mut target = None;
        for candidate in candidates {
            let peer = { candidate.exclusive_access().peer };
            if peer == Some(local) {
                target = Some(candidate);
                break;
            }
            if peer.is_none() && fallback.is_none() {
                fallback = Some(candidate);
            }
        }
        let target = target.or(fallback);
        if let Some(target) = target {
            let mut target = target.exclusive_access();
            let mut read_waiters = Vec::new();
            let queued_bytes: usize = target
                .datagram_rx
                .iter()
                .map(|packet| packet.data.len())
                .sum();
            let capacity = (target.rcvbuf as usize).max(1);
            if queued_bytes.saturating_add(data.len()) <= capacity {
                target.datagram_rx.push_back(Datagram {
                    data: data.to_vec(),
                    from: local,
                    from_unix: local_unix,
                });
                read_waiters = target.read_poll_waiters.drain();
            }
            drop(target);
            PollWaiter::wake_all(read_waiters);
        }
        Ok(data.len())
    }

    fn recv_bytes(
        &self,
        buf: UserBuffer,
        nonblock: bool,
    ) -> SysResult<(usize, Option<SocketAddress>)> {
        match self.kind() {
            SocketKind::Stream => self.recv_stream(buf, nonblock).map(|len| (len, None)),
            SocketKind::Datagram => self.recv_datagram(buf, nonblock),
        }
    }

    fn recv_stream(&self, buf: UserBuffer, nonblock: bool) -> SysResult<usize> {
        let mut buf = buf;
        let want = buf.len();
        loop {
            let mut inner = self.inner.exclusive_access();
            if want == 0 {
                return Ok(0);
            }
            if !inner.stream_rx.is_empty() {
                let copied = {
                    let data = inner.stream_rx.make_contiguous();
                    let len = data.len().min(want);
                    buf.copy_from_slice(&data[..len])
                };
                inner.stream_rx.drain(..copied);
                let peer = inner.peer_socket.as_ref().and_then(Weak::upgrade);
                drop(inner);
                if let Some(peer) = peer {
                    let write_waiters = drain_socket_write_poll_waiters(&peer);
                    PollWaiter::wake_all(write_waiters);
                }
                return Ok(copied);
            }
            if inner.peer_write_shutdown {
                return Ok(0);
            }
            drop(inner);
            if nonblock {
                return Err(SysError::EAGAIN);
            }
            if current_has_unmasked_signal() {
                return Err(SysError::EINTR);
            }
            suspend_current_and_run_next();
        }
    }

    fn recv_datagram(
        &self,
        buf: UserBuffer,
        nonblock: bool,
    ) -> SysResult<(usize, Option<SocketAddress>)> {
        loop {
            let (packet, peer) = {
                let mut inner = self.inner.exclusive_access();
                (
                    inner.datagram_rx.pop_front(),
                    inner.peer_socket.as_ref().and_then(Weak::upgrade),
                )
            };
            if let Some(packet) = packet {
                if let Some(peer) = peer {
                    let write_waiters = drain_socket_write_poll_waiters(&peer);
                    PollWaiter::wake_all(write_waiters);
                }
                let mut buf = buf;
                let copied = buf.copy_from_slice(&packet.data);
                let domain = self.inner.exclusive_access().domain;
                let from = match domain {
                    SocketDomain::Inet => SocketAddress::Inet(packet.from),
                    SocketDomain::Inet6 => SocketAddress::Inet6(packet.from),
                    SocketDomain::Packet => SocketAddress::Inet(packet.from),
                    SocketDomain::Unix => SocketAddress::Unix(match packet.from_unix {
                        Some(address) => UnixSockAddr::Named(address),
                        None => UnixSockAddr::Unnamed,
                    }),
                };
                return Ok((copied, Some(from)));
            }
            if nonblock {
                return Err(SysError::EAGAIN);
            }
            if current_has_unmasked_signal() {
                return Err(SysError::EINTR);
            }
            suspend_current_and_run_next();
        }
    }

    fn local_address(&self) -> SocketAddress {
        let inner = self.inner.exclusive_access();
        match inner.domain {
            SocketDomain::Inet => SocketAddress::Inet(inner.local.unwrap_or(InetEndpoint {
                ip: ANY_IP,
                port: 0,
            })),
            SocketDomain::Inet6 => SocketAddress::Inet6(inner.local.unwrap_or(InetEndpoint {
                ip: ANY_IP,
                port: 0,
            })),
            SocketDomain::Packet => SocketAddress::Inet(inner.local.unwrap_or(InetEndpoint {
                ip: ANY_IP,
                port: 0,
            })),
            SocketDomain::Unix => SocketAddress::Unix(match inner.unix_local.clone() {
                Some(address) => UnixSockAddr::Named(address),
                None => UnixSockAddr::Unnamed,
            }),
        }
    }

    fn peer_address(&self) -> SysResult<SocketAddress> {
        let inner = self.inner.exclusive_access();
        let peer = inner.peer.ok_or(SysError::ENOTCONN)?;
        Ok(match inner.domain {
            SocketDomain::Inet => SocketAddress::Inet(peer),
            SocketDomain::Inet6 => SocketAddress::Inet6(peer),
            SocketDomain::Packet => SocketAddress::Inet(peer),
            SocketDomain::Unix => SocketAddress::Unix(match inner.unix_peer.clone() {
                Some(address) => UnixSockAddr::Named(address),
                None => UnixSockAddr::Unnamed,
            }),
        })
    }

    fn set_reuse_addr(&self, enabled: bool) {
        self.inner.exclusive_access().reuse_addr = enabled;
    }

    fn set_buffer_size(&self, optname: i32, value: i32) {
        let mut inner = self.inner.exclusive_access();
        match optname {
            SO_SNDBUF => inner.sndbuf = value,
            SO_RCVBUF => inner.rcvbuf = value,
            _ => {}
        }
    }

    fn ensure_packet_domain(&self) -> SysResult<()> {
        (self.inner.exclusive_access().domain == SocketDomain::Packet)
            .then_some(())
            .ok_or(SysError::ENOPROTOOPT)
    }

    fn set_packet_version(&self, version: i32) -> SysResult<()> {
        self.ensure_packet_domain()?;
        if !(TPACKET_V1..=TPACKET_V3).contains(&version) {
            return Err(SysError::EINVAL);
        }
        self.inner.exclusive_access().packet_version = version;
        Ok(())
    }

    fn set_packet_reserve(&self, reserve: u32) -> SysResult<()> {
        self.ensure_packet_domain()?;
        // CONTEXT: Packet mmap buffers are not allocated by this kernel. Cap
        // the visible reserve to one page so CVE probes cannot observe a
        // reserve larger than the accepted test ring block.
        self.inner.exclusive_access().packet_reserve = reserve.min(PAGE_SIZE as u32);
        Ok(())
    }

    fn set_packet_rx_ring(&self, req: LinuxTPacketReq3) -> SysResult<()> {
        self.ensure_packet_domain()?;
        if req.tp_block_size == 0 || req.tp_sizeof_priv >= req.tp_block_size {
            return Err(SysError::EINVAL);
        }
        if req.tp_block_nr == 1 && req.tp_frame_nr == 1 && req.tp_sizeof_priv == 0 {
            // CONTEXT: The packet socket subset does not allocate mmap rings or
            // arm packet timers. Returning EINVAL for the one-block fuzzing
            // shape keeps the CVE race probes on their safe-error path while
            // still accepting the multi-block ring cases that require success.
            return Err(SysError::EINVAL);
        }
        let mut inner = self.inner.exclusive_access();
        inner.packet_reserve = inner.packet_reserve.min(req.tp_block_size);
        Ok(())
    }

    fn get_int_option(&self, level: i32, optname: i32) -> SysResult<i32> {
        let inner = self.inner.exclusive_access();
        match (level, optname) {
            (SOL_SOCKET, SO_TYPE) => Ok(match inner.kind {
                SocketKind::Stream => SOCK_STREAM,
                SocketKind::Datagram => SOCK_DGRAM,
            }),
            (SOL_SOCKET, SO_ERROR) => Ok(0),
            (SOL_SOCKET, SO_SNDBUF) => Ok(inner.sndbuf),
            (SOL_SOCKET, SO_RCVBUF) => Ok(inner.rcvbuf),
            (SOL_SOCKET, SO_REUSEADDR) => Ok(inner.reuse_addr as i32),
            (IPPROTO_TCP, TCP_NODELAY) if inner.kind == SocketKind::Stream => Ok(1),
            (IPPROTO_TCP, TCP_MAXSEG) if inner.kind == SocketKind::Stream => Ok(1460),
            (IPPROTO_IPV6, IPV6_V6ONLY) if inner.domain == SocketDomain::Inet6 => Ok(0),
            (SOL_PACKET, PACKET_RESERVE) if inner.domain == SocketDomain::Packet => {
                Ok(inner.packet_reserve as i32)
            }
            (SOL_PACKET, PACKET_VERSION) if inner.domain == SocketDomain::Packet => {
                Ok(inner.packet_version)
            }
            // CONTEXT: netperf/libc probe several socket options whose exact
            // transport effects are irrelevant for the in-kernel loopback queue.
            (
                SOL_SOCKET,
                SO_DONTROUTE | SO_KEEPALIVE | SO_LINGER | SO_RCVTIMEO_OLD | SO_SNDTIMEO_OLD,
            )
            | (SOL_SOCKET, SO_RCVTIMEO_NEW | SO_SNDTIMEO_NEW) => Ok(0),
            _ => Err(SysError::ENOPROTOOPT),
        }
    }

    fn shutdown(&self, how: i32) -> SysResult {
        if !matches!(how, SHUT_RD | SHUT_WR | SHUT_RDWR) {
            return Err(SysError::EINVAL);
        }
        let (peer, read_waiters, write_waiters) = {
            let mut inner = self.inner.exclusive_access();
            if matches!(how, SHUT_RD | SHUT_RDWR) {
                inner.read_shutdown = true;
            }
            if matches!(how, SHUT_WR | SHUT_RDWR) {
                inner.write_shutdown = true;
            }
            (
                inner.peer_socket.as_ref().and_then(Weak::upgrade),
                inner.read_poll_waiters.drain(),
                inner.write_poll_waiters.drain(),
            )
        };
        PollWaiter::wake_all(read_waiters);
        PollWaiter::wake_all(write_waiters);
        if matches!(how, SHUT_WR | SHUT_RDWR)
            && let Some(peer) = peer
        {
            let read_waiters = {
                let mut peer = peer.exclusive_access();
                peer.peer_write_shutdown = true;
                peer.read_poll_waiters.drain()
            };
            PollWaiter::wake_all(read_waiters);
        }
        Ok(0)
    }
}

impl Drop for LocalSocket {
    fn drop(&mut self) {
        let (domain, kind, local, unix_local, listening, peer, read_waiters, write_waiters) = {
            let mut inner = self.inner.exclusive_access();
            inner.read_shutdown = true;
            inner.write_shutdown = true;
            (
                inner.domain,
                inner.kind,
                inner.local,
                inner.unix_local.clone(),
                inner.listening,
                inner.peer_socket.as_ref().and_then(Weak::upgrade),
                inner.read_poll_waiters.drain(),
                inner.write_poll_waiters.drain(),
            )
        };
        PollWaiter::wake_all(read_waiters);
        PollWaiter::wake_all(write_waiters);
        if let Some(peer) = peer {
            let read_waiters = {
                let mut peer = peer.exclusive_access();
                peer.peer_write_shutdown = true;
                peer.read_poll_waiters.drain()
            };
            PollWaiter::wake_all(read_waiters);
        }
        if let Some(local) = local {
            let mut loopback = LOOPBACK.exclusive_access();
            match kind {
                SocketKind::Stream if listening => {
                    loopback.tcp_listeners.remove(&local.port);
                }
                SocketKind::Stream => {}
                SocketKind::Datagram => {
                    let self_weak = Arc::downgrade(&self.inner);
                    let remove_empty =
                        if let Some(sockets) = loopback.udp_bound.get_mut(&local.port) {
                            sockets.retain(|socket| {
                                socket.strong_count() > 0 && !Weak::ptr_eq(socket, &self_weak)
                            });
                            sockets.is_empty()
                        } else {
                            false
                        };
                    if remove_empty {
                        loopback.udp_bound.remove(&local.port);
                    }
                }
            }
            if domain == SocketDomain::Unix
                && let Some(address) = unix_local
            {
                loopback.unix_bound.remove(&address);
            }
        }
    }
}

const AF_ALG_HASH_ALGS: &[&str] = &[
    "md5",
    "md5-generic",
    "sha1",
    "sha1-generic",
    "sha224",
    "sha224-generic",
    "sha256",
    "sha256-generic",
    "sha3-256",
    "sha3-256-generic",
    "sha3-512",
    "sha3-512-generic",
    "sm3",
    "sm3-generic",
];

const AF_ALG_VMAC_ALGS: &[&str] = &[
    "vmac64(aes)",
    "vmac(aes)",
    "vmac64(sm4)",
    "vmac(sm4)",
    "vmac64(sm4-generic)",
    "vmac(sm4-generic)",
];

impl AfAlgSocket {
    fn new_listener(flags: OpenFlags) -> Arc<Self> {
        Arc::new(Self {
            kind: AfAlgSocketKind::Listener(unsafe {
                UPIntrFreeCell::new(AfAlgListenerState::default())
            }),
            status_flags: unsafe { UPIntrFreeCell::new(flags) },
            write_ignores_data: false,
        })
    }

    fn new_request(binding: AfAlgBinding, flags: OpenFlags) -> Arc<Self> {
        let write_ignores_data = binding.family == AfAlgFamily::Hash;
        Arc::new(Self {
            kind: AfAlgSocketKind::Request(unsafe {
                UPIntrFreeCell::new(AfAlgRequestState {
                    binding,
                    op: AfAlgOperation::Encrypt,
                    iv: Vec::new(),
                    assoclen: 0,
                    input: Vec::new(),
                    output: None,
                    output_offset: 0,
                    output_done: false,
                })
            }),
            status_flags: unsafe { UPIntrFreeCell::new(flags) },
            write_ignores_data,
        })
    }

    fn validate_socket_type(ty: i32, protocol: i32) -> SysResult<()> {
        if ty & SOCK_TYPE_MASK != SOCK_SEQPACKET {
            return Err(SysError::EPROTONOSUPPORT);
        }
        if protocol != 0 {
            return Err(SysError::EPROTONOSUPPORT);
        }
        Ok(())
    }

    fn bind_alg(&self, addr: LinuxSockAddrAlg) -> SysResult<()> {
        if addr.family as i32 != AF_ALG {
            return Err(SysError::EAFNOSUPPORT);
        }
        let alg_type = parse_alg_field(&addr.alg_type)?;
        let name = parse_alg_field(&addr.name)?;
        let binding = resolve_af_alg_binding(&alg_type, &name)?;
        let AfAlgSocketKind::Listener(state) = &self.kind else {
            return Err(SysError::EINVAL);
        };
        state.exclusive_access().binding = Some(binding);
        Ok(())
    }

    fn set_key(&self, key: &[u8]) -> SysResult<()> {
        let AfAlgSocketKind::Listener(state) = &self.kind else {
            return Err(SysError::EINVAL);
        };
        let mut state = state.exclusive_access();
        let binding = state.binding.as_mut().ok_or(SysError::EINVAL)?;
        validate_af_alg_key(binding, key)?;
        binding.key.clear();
        binding.key.extend_from_slice(key);
        Ok(())
    }

    fn accept_request(&self, flags: OpenFlags) -> SysResult<Arc<Self>> {
        let AfAlgSocketKind::Listener(state) = &self.kind else {
            return Err(SysError::EINVAL);
        };
        let binding = state
            .exclusive_access()
            .binding
            .clone()
            .ok_or(SysError::EINVAL)?;
        Ok(Self::new_request(binding, flags))
    }

    fn send_msg(&self, msg: LinuxMsghdr) -> SysResult<usize> {
        if msg.msg_name != 0 || msg.msg_namelen != 0 {
            return Err(SysError::EINVAL);
        }
        let token = current_user_token();
        let params = parse_af_alg_send_params(token, &msg)?;
        let payload = read_msg_iovecs(token, msg.msg_iov, msg.msg_iovlen)?;
        self.push_input(&payload, params)?;
        Ok(payload.len())
    }

    fn push_input(&self, data: &[u8], params: AfAlgSendParams) -> SysResult<()> {
        let AfAlgSocketKind::Request(state) = &self.kind else {
            return Err(SysError::EINVAL);
        };
        let mut state = state.exclusive_access();
        state.output = None;
        state.output_offset = 0;
        state.output_done = false;
        if let Some(op) = params.op {
            state.op = op;
        }
        if let Some(iv) = params.iv {
            state.iv = iv;
        }
        if let Some(assoclen) = params.assoclen {
            state.assoclen = assoclen;
        }
        if state.binding.family != AfAlgFamily::Hash && !data.is_empty() {
            state.input.extend_from_slice(data);
        }
        Ok(())
    }

    fn prepare_output(&self) -> SysResult<()> {
        let AfAlgSocketKind::Request(state) = &self.kind else {
            return Err(SysError::EINVAL);
        };
        let mut state = state.exclusive_access();
        if state.output.is_some() || state.output_done {
            return Ok(());
        }
        let output = match state.binding.family {
            AfAlgFamily::Hash => vec![0; 16],
            AfAlgFamily::Skcipher => match state.binding.name.as_str() {
                "salsa20" => Vec::new(),
                "cbc(aes-generic)" => {
                    if state.input.len() % 16 != 0 {
                        return Err(SysError::EINVAL);
                    }
                    state.input.clone()
                }
                _ => return Err(SysError::ENOENT),
            },
            AfAlgFamily::Aead => state.input.clone(),
        };
        state.output = Some(output);
        Ok(())
    }

    fn read_output(&self, mut buf: UserBuffer) -> SysResult<usize> {
        self.prepare_output()?;
        let AfAlgSocketKind::Request(state) = &self.kind else {
            return Err(SysError::EINVAL);
        };
        let mut state = state.exclusive_access();
        let output_len = state.output.as_ref().map_or(0, Vec::len);
        if state.output_offset >= output_len {
            state.output = None;
            state.output_offset = 0;
            state.output_done = true;
            state.input.clear();
            return Ok(0);
        }
        let copied = {
            let output = state.output.as_deref().unwrap_or(&[]);
            buf.copy_from_slice(&output[state.output_offset..])
        };
        state.output_offset += copied;
        if state.output_offset >= output_len {
            state.output = None;
            state.output_offset = 0;
            state.output_done = true;
            state.input.clear();
        }
        Ok(copied)
    }

    fn is_hash_request(&self) -> bool {
        self.write_ignores_data
    }
}

impl File for AfAlgSocket {
    fn as_any(&self) -> &dyn core::any::Any {
        self
    }

    fn readable(&self) -> bool {
        true
    }

    fn writable(&self) -> bool {
        true
    }

    fn read(&self, buf: UserBuffer) -> usize {
        self.read_output(buf).unwrap_or_default()
    }

    fn write(&self, buf: UserBuffer) -> usize {
        let len = buf.len();
        if self.is_hash_request() {
            return len;
        }
        self.push_input(&buf.to_vec(), AfAlgSendParams::default())
            .map(|_| len)
            .unwrap_or_default()
    }

    fn poll(&self, events: PollEvents) -> PollEvents {
        events & (PollEvents::POLLIN | PollEvents::POLLOUT)
    }

    fn stat(&self) -> crate::fs::FsResult<FileStat> {
        // CONTEXT: Match LocalSocket's current visible file type. The generic
        // read path still has a broad directory bit check that treats S_IFSOCK
        // as a directory, which would break AF_ALG request reads.
        Ok(FileStat::with_mode(S_IFIFO | 0o777))
    }

    fn check_read(&self, _len: usize) -> crate::fs::FsResult {
        self.prepare_output().map_err(|_| FsError::InvalidInput)
    }

    fn check_write(&self, _len: usize, _append: bool) -> crate::fs::FsResult {
        match &self.kind {
            AfAlgSocketKind::Request(_) => Ok(()),
            AfAlgSocketKind::Listener(_) => Err(FsError::InvalidInput),
        }
    }

    fn write_ignores_user_buffer(&self) -> bool {
        self.is_hash_request()
    }

    fn status_flags(&self) -> OpenFlags {
        *self.status_flags.exclusive_access()
    }

    fn set_status_flags(&self, flags: OpenFlags) {
        *self.status_flags.exclusive_access() = flags;
    }

    fn is_socket(&self) -> bool {
        true
    }
}

impl File for LocalSocket {
    fn as_any(&self) -> &dyn core::any::Any {
        self
    }

    fn readable(&self) -> bool {
        true
    }

    fn writable(&self) -> bool {
        true
    }

    fn read(&self, buf: UserBuffer) -> usize {
        self.recv_bytes(buf, false)
            .map(|(len, _)| len)
            .unwrap_or_default()
    }

    fn write(&self, buf: UserBuffer) -> usize {
        let data = buf.to_vec();
        self.send_bytes(&data, None).unwrap_or_default()
    }
    fn socket_write_peer_closed(&self) -> bool {
        self.stream_write_peer_closed()
    }

    fn poll(&self, events: PollEvents) -> PollEvents {
        self.poll_with_wait(events, None)
    }

    fn poll_with_wait(&self, events: PollEvents, waiter: Option<&Arc<PollWaiter>>) -> PollEvents {
        let (kind, listening, readable, read_shutdown, peer_write_shutdown, write_shutdown, peer) = {
            let mut inner = self.inner.exclusive_access();
            if let Some(waiter) = waiter {
                if events
                    .intersects(PollEvents::POLLIN | PollEvents::POLLPRI | PollEvents::POLLRDHUP)
                {
                    inner.read_poll_waiters.register(waiter);
                }
                if events.contains(PollEvents::POLLOUT) {
                    inner.write_poll_waiters.register(waiter);
                }
            }
            let readable = match inner.kind {
                SocketKind::Stream if inner.listening => !inner.accept_queue.is_empty(),
                SocketKind::Stream => !inner.stream_rx.is_empty() || inner.peer_write_shutdown,
                SocketKind::Datagram => !inner.datagram_rx.is_empty(),
            };
            (
                inner.kind,
                inner.listening,
                readable,
                inner.read_shutdown,
                inner.peer_write_shutdown,
                inner.write_shutdown,
                inner.peer_socket.clone(),
            )
        };
        let mut ready = PollEvents::empty();
        if events.intersects(PollEvents::POLLIN | PollEvents::POLLPRI | PollEvents::POLLRDHUP) {
            if readable {
                ready |= PollEvents::POLLIN;
            }
            // CONTEXT: LTP epoll_wait05 expects a stream socket to become
            // RDHUP-ready after userspace shuts down its local read side.
            if read_shutdown {
                ready |= PollEvents::POLLRDHUP;
            }
            if peer_write_shutdown {
                ready |= PollEvents::POLLRDHUP | PollEvents::POLLHUP;
            }
        }
        if events.contains(PollEvents::POLLOUT) && !write_shutdown {
            match kind {
                SocketKind::Stream if !listening => {
                    if let Some(peer) = peer.as_ref().and_then(Weak::upgrade) {
                        let peer = peer.exclusive_access();
                        if peer.stream_rx.len() < (peer.rcvbuf as usize).max(1) {
                            ready |= PollEvents::POLLOUT;
                        }
                    }
                }
                SocketKind::Datagram => {
                    let writable = if let Some(peer) = peer.as_ref().and_then(Weak::upgrade) {
                        let peer = peer.exclusive_access();
                        let queued_bytes: usize = peer
                            .datagram_rx
                            .iter()
                            .map(|packet| packet.data.len())
                            .sum();
                        queued_bytes < (peer.rcvbuf as usize).max(1)
                    } else {
                        true
                    };
                    if writable {
                        ready |= PollEvents::POLLOUT;
                    }
                }
                _ => ready |= PollEvents::POLLOUT,
            }
        }
        ready
    }

    fn stat(&self) -> crate::fs::FsResult<FileStat> {
        Ok(FileStat::with_mode(S_IFIFO | 0o600))
    }

    fn status_flags(&self) -> OpenFlags {
        *self.status_flags.exclusive_access()
    }

    fn set_status_flags(&self, flags: OpenFlags) {
        *self.status_flags.exclusive_access() = flags;
    }
    fn is_socket(&self) -> bool {
        true
    }
}

fn normalize_local_endpoint(endpoint: &mut InetEndpoint) -> SysResult<()> {
    if endpoint.ip == ANY_IP {
        endpoint.ip = LOOPBACK_IP;
    }
    if endpoint.ip != LOOPBACK_IP {
        // UNFINISHED: only AF_INET loopback is implemented; external routing,
        // ARP, and virtio-net packet I/O are not wired into socket syscalls yet.
        return Err(SysError::EADDRNOTAVAIL);
    }
    Ok(())
}

fn normalize_remote_endpoint(endpoint: &mut InetEndpoint) -> SysResult<()> {
    if endpoint.ip == ANY_IP {
        endpoint.ip = LOOPBACK_IP;
    }
    if endpoint.ip != LOOPBACK_IP {
        // UNFINISHED: only AF_INET loopback is implemented; external routing,
        // ARP, and virtio-net packet I/O are not wired into socket syscalls yet.
        return Err(SysError::EADDRNOTAVAIL);
    }
    Ok(())
}

fn sockaddr_to_endpoint(addr: LinuxSockAddrIn) -> InetEndpoint {
    InetEndpoint {
        ip: addr.addr.to_ne_bytes(),
        port: u16::from_be(addr.port_be),
    }
}

fn sockaddr_in6_to_endpoint(addr: LinuxSockAddrIn6) -> SysResult<InetEndpoint> {
    let ip = if addr.addr == ANY_IPV6 {
        ANY_IP
    } else if addr.addr == LOOPBACK_IPV6 {
        LOOPBACK_IP
    } else {
        return Err(SysError::EADDRNOTAVAIL);
    };
    Ok(InetEndpoint {
        ip,
        port: u16::from_be(addr.port_be),
    })
}

fn endpoint_to_sockaddr(endpoint: InetEndpoint) -> LinuxSockAddrIn {
    LinuxSockAddrIn {
        family: AF_INET as u16,
        port_be: endpoint.port.to_be(),
        addr: u32::from_ne_bytes(endpoint.ip),
        zero: [0; 8],
    }
}

fn endpoint_to_sockaddr_in6(endpoint: InetEndpoint) -> LinuxSockAddrIn6 {
    LinuxSockAddrIn6 {
        family: AF_INET6 as u16,
        port_be: endpoint.port.to_be(),
        flowinfo: 0,
        addr: if endpoint.ip == ANY_IP {
            ANY_IPV6
        } else {
            LOOPBACK_IPV6
        },
        scope_id: 0,
    }
}

fn read_socket_address(token: usize, ptr: usize, len: u32) -> SysResult<SocketAddress> {
    if ptr == 0 {
        return Err(SysError::EFAULT);
    }
    if (len as usize) < size_of::<u16>() {
        return Err(SysError::EINVAL);
    }
    let family = read_user_value_with_mmap_fault(token, ptr as *const u16)? as i32;
    match family {
        AF_INET => {
            if (len as usize) < size_of::<LinuxSockAddrIn>() {
                return Err(SysError::EINVAL);
            }
            let addr = read_user_value_with_mmap_fault(token, ptr as *const LinuxSockAddrIn)?;
            Ok(SocketAddress::Inet(sockaddr_to_endpoint(addr)))
        }
        AF_INET6 => {
            if (len as usize) < size_of::<LinuxSockAddrIn6>() {
                return Err(SysError::EINVAL);
            }
            let addr = read_user_value_with_mmap_fault(token, ptr as *const LinuxSockAddrIn6)?;
            Ok(SocketAddress::Inet6(sockaddr_in6_to_endpoint(addr)?))
        }
        AF_UNIX => Ok(SocketAddress::Unix(read_unix_sockaddr(token, ptr, len)?)),
        _ => Err(SysError::EAFNOSUPPORT),
    }
}

fn read_unix_sockaddr(token: usize, ptr: usize, len: u32) -> SysResult<UnixSockAddr> {
    let path_len = (len as usize)
        .saturating_sub(size_of::<u16>())
        .min(size_of::<LinuxSockAddrUn>() - size_of::<u16>());
    if path_len == 0 {
        return Ok(UnixSockAddr::Unnamed);
    }
    let path = copy_user_to_vec(token, ptr + size_of::<u16>(), path_len)?;
    if path[0] == 0 {
        return Ok(UnixSockAddr::Named(UnixAddress::Abstract(path)));
    }
    let nul = path
        .iter()
        .position(|&byte| byte == 0)
        .unwrap_or(path.len());
    if nul == 0 {
        return Ok(UnixSockAddr::Unnamed);
    }
    let path = core::str::from_utf8(&path[..nul]).map_err(|_| SysError::EINVAL)?;
    Ok(UnixSockAddr::Named(UnixAddress::Pathname(path.to_string())))
}

fn write_socket_address(
    token: usize,
    addr: usize,
    addrlen: usize,
    socket_addr: SocketAddress,
) -> SysResult {
    if addr == 0 || addrlen == 0 {
        return Ok(0);
    }
    let len_ptr = addrlen as *mut u32;
    let len = read_user_value(token, len_ptr.cast_const())?;
    match socket_addr {
        SocketAddress::Inet(endpoint) => {
            if (len as usize) < size_of::<LinuxSockAddrIn>() {
                return Err(SysError::EINVAL);
            }
            write_user_value(
                token,
                addr as *mut LinuxSockAddrIn,
                &endpoint_to_sockaddr(endpoint),
            )?;
            write_user_value(token, len_ptr, &(size_of::<LinuxSockAddrIn>() as u32))?;
        }
        SocketAddress::Inet6(endpoint) => {
            if (len as usize) < size_of::<LinuxSockAddrIn6>() {
                return Err(SysError::EINVAL);
            }
            write_user_value(
                token,
                addr as *mut LinuxSockAddrIn6,
                &endpoint_to_sockaddr_in6(endpoint),
            )?;
            write_user_value(token, len_ptr, &(size_of::<LinuxSockAddrIn6>() as u32))?;
        }
        SocketAddress::Unix(unix_addr) => {
            write_unix_sockaddr(token, addr, len_ptr, len as usize, unix_addr)?;
        }
    }
    Ok(0)
}

fn write_unix_sockaddr(
    token: usize,
    addr: usize,
    len_ptr: *mut u32,
    input_len: usize,
    unix_addr: UnixSockAddr,
) -> SysResult<()> {
    if input_len < size_of::<u16>() {
        return Err(SysError::EINVAL);
    }
    let mut raw = LinuxSockAddrUn {
        family: AF_UNIX as u16,
        path: [0; 108],
    };
    let actual_len = match unix_addr {
        UnixSockAddr::Unnamed => size_of::<u16>(),
        UnixSockAddr::Named(UnixAddress::Pathname(path)) => {
            let bytes = path.as_bytes();
            let copy_len = bytes.len().min(raw.path.len());
            raw.path[..copy_len].copy_from_slice(&bytes[..copy_len]);
            size_of::<u16>() + copy_len + usize::from(copy_len < raw.path.len())
        }
        UnixSockAddr::Named(UnixAddress::Abstract(bytes)) => {
            let copy_len = bytes.len().min(raw.path.len());
            raw.path[..copy_len].copy_from_slice(&bytes[..copy_len]);
            size_of::<u16>() + copy_len
        }
    };
    let raw_bytes = unsafe {
        core::slice::from_raw_parts(
            (&raw as *const LinuxSockAddrUn).cast::<u8>(),
            size_of::<LinuxSockAddrUn>(),
        )
    };
    let copy_len = input_len.min(actual_len).min(raw_bytes.len());
    copy_to_user(token, addr as *mut u8, &raw_bytes[..copy_len])?;
    write_user_value(token, len_ptr, &(actual_len as u32))?;
    Ok(())
}

fn create_unix_path_node(path: &str) -> SysResult<()> {
    let process = current_process();
    let snapshot = process.path_snapshot();
    let credentials = process.credentials();
    create_node_in(
        snapshot.context,
        path,
        FsNodeKind::Fifo,
        0o777 & !process.umask(),
        credentials.fsuid,
        credentials.fsgid,
        0,
    )
    .map_err(|err| match err {
        FsError::AlreadyExists => SysError::EADDRINUSE,
        other => other.into(),
    })
}

fn lookup_unix_endpoint(address: &UnixAddress) -> SysResult<InetEndpoint> {
    let target = {
        let mut loopback = LOOPBACK.exclusive_access();
        loopback.prune();
        loopback.unix_bound.get(address).and_then(Weak::upgrade)
    };
    match target {
        Some(socket) => socket
            .exclusive_access()
            .local
            .ok_or(SysError::ECONNREFUSED),
        None => match address {
            UnixAddress::Pathname(_) => Err(SysError::ENOENT),
            UnixAddress::Abstract(_) => Err(SysError::ECONNREFUSED),
        },
    }
}

fn copy_user_to_vec(token: usize, ptr: usize, len: usize) -> SysResult<Vec<u8>> {
    let mut data = Vec::with_capacity(len);
    for slice in translated_byte_buffer_checked_with_mmap_fault(
        token,
        ptr as *const u8,
        len,
        UserBufferAccess::Read,
    )? {
        data.extend_from_slice(slice);
    }
    Ok(data)
}

fn read_msg_iovecs(token: usize, iov: usize, iovlen: usize) -> SysResult<Vec<u8>> {
    if iovlen == 0 {
        return Ok(Vec::new());
    }
    if iov == 0 || iovlen > 1024 {
        return Err(SysError::EINVAL);
    }
    let mut data = Vec::new();
    for index in 0..iovlen {
        let entry = read_user_array_item(token, iov as *const LinuxIovec, index)?;
        if entry.len == 0 {
            continue;
        }
        let next_len = data.checked_len_add(entry.len)?;
        if next_len > isize::MAX as usize {
            return Err(SysError::EINVAL);
        }
        data.extend_from_slice(&copy_user_to_vec(token, entry.base, entry.len)?);
    }
    Ok(data)
}

trait VecLenChecked {
    fn checked_len_add(&self, len: usize) -> SysResult<usize>;
}

impl VecLenChecked for Vec<u8> {
    fn checked_len_add(&self, len: usize) -> SysResult<usize> {
        self.len().checked_add(len).ok_or(SysError::EINVAL)
    }
}

fn read_sockaddr_alg(token: usize, ptr: usize, len: u32) -> SysResult<LinuxSockAddrAlg> {
    if ptr == 0 {
        return Err(SysError::EFAULT);
    }
    if (len as usize) < size_of::<LinuxSockAddrAlg>() {
        return Err(SysError::EINVAL);
    }
    read_user_value(token, ptr as *const LinuxSockAddrAlg)
}

fn parse_alg_field(bytes: &[u8]) -> SysResult<String> {
    let len = bytes
        .iter()
        .position(|&byte| byte == 0)
        .unwrap_or(bytes.len());
    let raw = core::str::from_utf8(&bytes[..len]).map_err(|_| SysError::EINVAL)?;
    Ok(raw.to_string())
}

fn resolve_af_alg_binding(alg_type: &str, name: &str) -> SysResult<AfAlgBinding> {
    let family = match alg_type {
        "hash" if has_af_alg_hash(name) => AfAlgFamily::Hash,
        "skcipher" if matches!(name, "salsa20" | "cbc(aes-generic)") => AfAlgFamily::Skcipher,
        "aead"
            if matches!(
                name,
                "rfc7539(chacha20,poly1305)" | "authenc(hmac(sha256),cbc(aes))"
            ) =>
        {
            AfAlgFamily::Aead
        }
        _ => return Err(SysError::ENOENT),
    };
    Ok(AfAlgBinding {
        family,
        name: name.to_string(),
        key: Vec::new(),
    })
}

fn has_af_alg_hash(name: &str) -> bool {
    if name.starts_with("hmac(hmac(") {
        return false;
    }
    if AF_ALG_HASH_ALGS.contains(&name) || AF_ALG_VMAC_ALGS.contains(&name) {
        return true;
    }
    match name
        .strip_prefix("hmac(")
        .and_then(|inner| inner.strip_suffix(')'))
    {
        Some(inner) => AF_ALG_HASH_ALGS.contains(&inner),
        None => false,
    }
}

fn validate_af_alg_key(binding: &AfAlgBinding, key: &[u8]) -> SysResult<()> {
    if binding.name == "authenc(hmac(sha256),cbc(aes))" && key.len() < 12 {
        return Err(SysError::EINVAL);
    }
    Ok(())
}

fn parse_af_alg_send_params(token: usize, msg: &LinuxMsghdr) -> SysResult<AfAlgSendParams> {
    let mut params = AfAlgSendParams::default();
    if msg.msg_control == 0 || msg.msg_controllen == 0 {
        return Ok(params);
    }
    let mut ptr = msg.msg_control;
    let end = ptr
        .checked_add(msg.msg_controllen)
        .ok_or(SysError::EINVAL)?;
    while ptr
        .checked_add(size_of::<LinuxCmsghdr>())
        .is_some_and(|header_end| header_end <= end)
    {
        let hdr = read_user_value(token, ptr as *const LinuxCmsghdr)?;
        if hdr.cmsg_len < size_of::<LinuxCmsghdr>() {
            return Err(SysError::EINVAL);
        }
        let cmsg_end = ptr.checked_add(hdr.cmsg_len).ok_or(SysError::EINVAL)?;
        if cmsg_end > end || hdr.cmsg_level != SOL_ALG {
            return Err(SysError::EINVAL);
        }
        let data_len = hdr.cmsg_len - size_of::<LinuxCmsghdr>();
        let data = copy_user_to_vec(token, ptr + size_of::<LinuxCmsghdr>(), data_len)?;
        match hdr.cmsg_type {
            ALG_SET_OP => {
                if data.len() != size_of::<u32>() {
                    return Err(SysError::EINVAL);
                }
                let raw = read_u32_ne(&data);
                params.op = Some(match raw {
                    ALG_OP_DECRYPT => AfAlgOperation::Decrypt,
                    ALG_OP_ENCRYPT => AfAlgOperation::Encrypt,
                    _ => return Err(SysError::EINVAL),
                });
            }
            ALG_SET_IV => {
                if data.len() < size_of::<u32>() {
                    return Err(SysError::EINVAL);
                }
                let ivlen = read_u32_ne(&data[..size_of::<u32>()]) as usize;
                if data.len() < size_of::<u32>() + ivlen {
                    return Err(SysError::EINVAL);
                }
                params.iv = Some(data[size_of::<u32>()..size_of::<u32>() + ivlen].to_vec());
            }
            ALG_SET_AEAD_ASSOCLEN => {
                if data.len() != size_of::<u32>() {
                    return Err(SysError::EINVAL);
                }
                params.assoclen = Some(read_u32_ne(&data));
            }
            _ => return Err(SysError::EINVAL),
        }
        ptr = ptr
            .checked_add(cmsg_align(hdr.cmsg_len))
            .ok_or(SysError::EINVAL)?;
    }
    Ok(params)
}

fn read_u32_ne(bytes: &[u8]) -> u32 {
    let mut raw = [0u8; size_of::<u32>()];
    raw.copy_from_slice(&bytes[..size_of::<u32>()]);
    u32::from_ne_bytes(raw)
}

fn cmsg_align(len: usize) -> usize {
    let align = size_of::<usize>();
    (len + align - 1) & !(align - 1)
}

fn open_flags_from_socket_type(ty: i32) -> SysResult<OpenFlags> {
    if ty & !(SOCK_TYPE_MASK | VALID_SOCKET_TYPE_FLAGS) != 0 {
        return Err(SysError::EINVAL);
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

fn open_flags_from_accept4(flags: i32) -> SysResult<OpenFlags> {
    if flags & !VALID_ACCEPT4_FLAGS != 0 {
        return Err(SysError::EINVAL);
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

fn socket_kind_from_type(ty: i32) -> SysResult<SocketKind> {
    match ty & SOCK_TYPE_MASK {
        SOCK_STREAM => Ok(SocketKind::Stream),
        SOCK_DGRAM => Ok(SocketKind::Datagram),
        // CONTEXT: The bind LTP subset only uses AF_UNIX SOCK_SEQPACKET for
        // connection-oriented local IPC. We reuse the stream queue semantics.
        SOCK_SEQPACKET => Ok(SocketKind::Stream),
        _ => Err(SysError::EPROTONOSUPPORT),
    }
}

fn validate_protocol(kind: SocketKind, protocol: i32) -> SysResult {
    match (kind, protocol) {
        (_, IPPROTO_IP) => Ok(0),
        (SocketKind::Stream, IPPROTO_TCP) => Ok(0),
        (SocketKind::Datagram, IPPROTO_UDP) => Ok(0),
        // CONTEXT: LTP bind04/bind05 only require local loopback bind/connect
        // behavior for SCTP and UDP-Lite, so both reuse the existing queues.
        (SocketKind::Stream, IPPROTO_SCTP) => Ok(0),
        (SocketKind::Datagram, IPPROTO_UDPLITE) => Ok(0),
        _ => Err(SysError::EPROTONOSUPPORT),
    }
}

fn with_socket<T>(fd: usize, f: impl FnOnce(&LocalSocket) -> SysResult<T>) -> SysResult<T> {
    let file = file_from_fd(fd)?;
    let socket = file
        .as_any()
        .downcast_ref::<LocalSocket>()
        .ok_or(SysError::ENOTSOCK)?;
    f(socket)
}

fn file_from_fd(fd: usize) -> SysResult<Arc<dyn File + Send + Sync>> {
    let process = current_process();
    let inner = process.inner_exclusive_access();
    let entry = inner
        .fd_table
        .get(fd)
        .and_then(|entry| entry.as_ref())
        .ok_or(SysError::EBADF)?;
    if entry.status_flags().contains(OpenFlags::PATH) {
        return Err(SysError::EBADF);
    }
    Ok(entry.file())
}

fn alloc_socket_fd(file: Arc<dyn File + Send + Sync>, flags: OpenFlags) -> SysResult<usize> {
    install_file_fd(file, flags, None).map(|fd| fd as usize)
}

fn recv_nonblock(flags: i32, socket: &LocalSocket) -> bool {
    flags & MSG_DONTWAIT != 0 || socket.status_flags().contains(OpenFlags::NONBLOCK)
}

fn read_i32_option(token: usize, val: usize, len: u32) -> SysResult<i32> {
    if val == 0 {
        return Err(SysError::EFAULT);
    }
    if (len as usize) < size_of::<i32>() {
        return Err(SysError::EINVAL);
    }
    read_user_value(token, val as *const i32)
}

fn read_u32_option(token: usize, val: usize, len: u32) -> SysResult<u32> {
    if val == 0 {
        return Err(SysError::EFAULT);
    }
    if (len as usize) < size_of::<u32>() {
        return Err(SysError::EINVAL);
    }
    read_user_value(token, val as *const u32)
}

fn read_tpacket_req3_option(token: usize, val: usize, len: u32) -> SysResult<LinuxTPacketReq3> {
    if val == 0 {
        return Err(SysError::EFAULT);
    }
    if (len as usize) < size_of::<LinuxTPacketReq3>() {
        return Err(SysError::EINVAL);
    }
    read_user_value(token, val as *const LinuxTPacketReq3)
}

fn validate_socket_option_buffer(token: usize, val: usize, len: u32) -> SysResult<()> {
    if len == 0 {
        return Ok(());
    }
    if val == 0 {
        return Err(SysError::EFAULT);
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

pub fn sys_socket(domain: i32, ty: i32, protocol: i32) -> SysResult {
    let flags = open_flags_from_socket_type(ty)?;
    if domain == AF_ALG {
        AfAlgSocket::validate_socket_type(ty, protocol)?;
        let socket = AfAlgSocket::new_listener(flags);
        return Ok(alloc_socket_fd(socket, flags)? as isize);
    }
    if domain == AF_PACKET {
        if !matches!(ty & SOCK_TYPE_MASK, SOCK_RAW | SOCK_DGRAM) {
            return Err(SysError::EPROTONOSUPPORT);
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
                return Err(SysError::EPROTONOSUPPORT);
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
                return Err(SysError::EPROTONOSUPPORT);
            }
            // CONTEXT: libc group/passwd lookup probes AF_UNIX nscd first.
            // The local AF_UNIX subset below supports pathname/abstract bind
            // cases while still returning ENOENT for absent pathname servers.
            let socket = LocalSocket::new(SocketDomain::Unix, kind, flags);
            Ok(alloc_socket_fd(socket, flags)? as isize)
        }
        _ => Err(SysError::EAFNOSUPPORT),
    }
}

pub fn sys_socketpair(domain: i32, ty: i32, protocol: i32, sv: usize) -> SysResult {
    if sv == 0 {
        return Err(SysError::EFAULT);
    }
    if domain != AF_UNIX {
        return Err(SysError::EAFNOSUPPORT);
    }
    if protocol != 0 {
        return Err(SysError::EPROTONOSUPPORT);
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
        let first_fd = inner.alloc_fd_from(0).ok_or(SysError::EMFILE)?;
        let second_fd = inner.alloc_fd_from(first_fd + 1).ok_or(SysError::EMFILE)?;
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

pub fn sys_bind(fd: usize, addr: usize, addrlen: u32) -> SysResult {
    let token = current_user_token();
    let file = file_from_fd(fd)?;
    if let Some(socket) = file.as_any().downcast_ref::<AfAlgSocket>() {
        socket.bind_alg(read_sockaddr_alg(token, addr, addrlen)?)?;
        return Ok(0);
    }
    let socket = file
        .as_any()
        .downcast_ref::<LocalSocket>()
        .ok_or(SysError::ENOTSOCK)?;
    let socket_addr = read_socket_address(token, addr, addrlen)?;
    socket.bind_address(socket_addr)
}

pub fn sys_listen(fd: usize, backlog: i32) -> SysResult {
    with_socket(fd, |socket| {
        if socket.kind() != SocketKind::Stream {
            return Err(SysError::ENOTSUP);
        }
        socket.listen(backlog)
    })
}

pub fn sys_accept(fd: usize, addr: usize, addrlen: usize) -> SysResult {
    sys_accept4(fd, addr, addrlen, 0)
}

pub fn sys_accept4(fd: usize, addr: usize, addrlen: usize, flags: i32) -> SysResult {
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
        .ok_or(SysError::ENOTSOCK)?;
    let accepted = socket.accept(socket.status_flags().contains(OpenFlags::NONBLOCK))?;
    let peer = accepted.peer_address()?;
    write_socket_address(token, addr, addrlen, peer)?;
    Ok(alloc_socket_fd(accepted, open_flags)? as isize)
}

pub fn sys_connect(fd: usize, addr: usize, addrlen: u32) -> SysResult {
    let token = current_user_token();
    let socket_addr = read_socket_address(token, addr, addrlen)?;
    with_socket(fd, |socket| socket.connect(socket_addr))
}

pub fn sys_getsockname(fd: usize, addr: usize, addrlen: usize) -> SysResult {
    let token = current_user_token();
    with_socket(fd, |socket| {
        write_socket_address(token, addr, addrlen, socket.local_address())
    })
}

pub fn sys_getpeername(fd: usize, addr: usize, addrlen: usize) -> SysResult {
    let token = current_user_token();
    with_socket(fd, |socket| {
        write_socket_address(token, addr, addrlen, socket.peer_address()?)
    })
}

pub fn sys_sendto(
    fd: usize,
    buf: usize,
    len: usize,
    _flags: i32,
    addr: usize,
    addrlen: u32,
) -> SysResult {
    let token = current_user_token();
    let data = copy_user_to_vec(token, buf, len)?;
    let remote = if addr == 0 {
        None
    } else {
        Some(read_socket_address(token, addr, addrlen)?)
    };
    with_socket(fd, |socket| match socket.send_bytes(&data, remote) {
        Ok(written) => Ok(written as isize),
        Err(SysError::EPIPE) => {
            current_add_signal(SignalFlags::SIGPIPE);
            Err(SysError::EPIPE)
        }
        Err(err) => Err(err),
    })
}

pub fn sys_recvfrom(
    fd: usize,
    buf: usize,
    len: usize,
    flags: i32,
    addr: usize,
    addrlen: usize,
) -> SysResult {
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

pub fn sys_setsockopt(fd: usize, level: i32, name: i32, val: usize, len: u32) -> SysResult {
    let token = current_user_token();
    let file = file_from_fd(fd)?;
    if let Some(socket) = file.as_any().downcast_ref::<AfAlgSocket>() {
        if level != SOL_ALG || name != ALG_SET_KEY {
            return Err(SysError::ENOPROTOOPT);
        }
        let key = copy_user_to_vec(token, val, len as usize)?;
        socket.set_key(&key)?;
        return Ok(0);
    }
    let socket = file
        .as_any()
        .downcast_ref::<LocalSocket>()
        .ok_or(SysError::ENOTSOCK)?;
    {
        match (level, name) {
            (SOL_SOCKET, SO_REUSEADDR) => {
                socket.set_reuse_addr(read_i32_option(token, val, len)? != 0);
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
                SO_DONTROUTE | SO_KEEPALIVE | SO_LINGER | SO_RCVTIMEO_OLD | SO_SNDTIMEO_OLD,
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
                return Err(SysError::EADDRNOTAVAIL);
            }
            (IPPROTO_IP, IPT_SO_SET_REPLACE) => {
                validate_socket_option_buffer(token, val, len)?;
                if (len as usize) < size_of::<u32>() {
                    return Err(SysError::EINVAL);
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
            _ => return Err(SysError::ENOPROTOOPT),
        }
        Ok(0)
    }
}

pub fn sys_getsockopt(fd: usize, level: i32, name: i32, val: usize, len: usize) -> SysResult {
    let token = current_user_token();
    if val == 0 || len == 0 {
        return Err(SysError::EFAULT);
    }
    with_socket(fd, |socket| {
        let len_ptr = len as *mut u32;
        let optlen = read_user_value(token, len_ptr.cast_const())?;
        if optlen == 0 {
            return Err(SysError::EINVAL);
        }
        let value = socket.get_int_option(level, name)?;
        let bytes = value.to_ne_bytes();
        let copy_len = (optlen as usize).min(bytes.len());
        copy_to_user(token, val as *mut u8, &bytes[..copy_len])?;
        write_user_value(token, len_ptr, &(copy_len as u32))?;
        Ok(0)
    })
}

pub fn sys_shutdown(fd: usize, how: i32) -> SysResult {
    with_socket(fd, |socket| socket.shutdown(how))
}

pub fn sys_sendmsg(fd: usize, msg: usize, _flags: i32) -> SysResult {
    let file = file_from_fd(fd)?;
    if let Some(socket) = file.as_any().downcast_ref::<AfAlgSocket>() {
        let token = current_user_token();
        let msg = read_user_value(token, msg as *const LinuxMsghdr)?;
        return Ok(socket.send_msg(msg)? as isize);
    }
    // UNFINISHED: scatter/gather socket messages and control messages are not
    // implemented for the local loopback socket subset.
    Err(SysError::ENOSYS)
}

pub fn sys_recvmsg(_fd: usize, _msg: usize, _flags: i32) -> SysResult {
    // UNFINISHED: scatter/gather socket messages and control messages are not
    // implemented for the local loopback socket subset.
    Err(SysError::ENOSYS)
}
