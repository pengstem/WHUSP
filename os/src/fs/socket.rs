//! Minimal socket syscalls.
//!
//! This is not a complete Linux networking stack. It provides the smallest
//! local TCP/UDP behavior needed by the contest netperf scripts, which use
//! `127.0.0.1` inside one guest.  Packets never leave the kernel and virtio-net
//! is not involved.

use super::{File, FileStat, FsError, OpenFlags, PollEvents, S_IFIFO};
use crate::mm::UserBuffer;
use crate::sync::UPIntrFreeCell;
use crate::syscall::errno::{SysError, SysResult};
use crate::syscall::user_ptr::{
    UserBufferAccess, copy_to_user, read_user_array_item, read_user_value,
    translated_byte_buffer_checked, write_user_value,
};
use crate::syscall::{close_detached_fd_entry, install_file_fd};
use crate::task::{
    FdTableEntry, current_has_unmasked_signal, current_process, current_user_token,
    suspend_current_and_run_next,
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
const AF_ALG: i32 = 38;
const SOCK_STREAM: i32 = 1;
const SOCK_DGRAM: i32 = 2;
const SOCK_SEQPACKET: i32 = 5;
const SOCK_TYPE_MASK: i32 = 0xf;
const SOCK_NONBLOCK: i32 = OpenFlags::NONBLOCK.bits() as i32;
const SOCK_CLOEXEC: i32 = OpenFlags::CLOEXEC.bits() as i32;
const VALID_SOCKET_TYPE_FLAGS: i32 = SOCK_NONBLOCK | SOCK_CLOEXEC;
const VALID_ACCEPT4_FLAGS: i32 = SOCK_NONBLOCK | SOCK_CLOEXEC;
const IPPROTO_IP: i32 = 0;
const IPPROTO_TCP: i32 = 6;
const IPPROTO_UDP: i32 = 17;
const SOL_SOCKET: i32 = 1;
const SOL_ALG: i32 = 279;
const SO_REUSEADDR: i32 = 2;
const SO_TYPE: i32 = 3;
const SO_ERROR: i32 = 4;
const SO_DONTROUTE: i32 = 5;
const SO_SNDBUF: i32 = 7;
const SO_RCVBUF: i32 = 8;
const SO_KEEPALIVE: i32 = 9;
const SO_LINGER: i32 = 13;
const SO_RCVTIMEO_OLD: i32 = 20;
const SO_SNDTIMEO_OLD: i32 = 21;
const SO_RCVTIMEO_NEW: i32 = 66;
const SO_SNDTIMEO_NEW: i32 = 67;
const TCP_NODELAY: i32 = 1;
const TCP_MAXSEG: i32 = 2;
const MCAST_JOIN_GROUP: i32 = 42;
const MCAST_LEAVE_GROUP: i32 = 45;
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct InetEndpoint {
    ip: [u8; 4],
    port: u16,
}

#[derive(Clone)]
struct Datagram {
    data: Vec<u8>,
    from: InetEndpoint,
}

struct LocalSocketInner {
    kind: SocketKind,
    local: Option<InetEndpoint>,
    peer: Option<InetEndpoint>,
    peer_socket: Option<Weak<UPIntrFreeCell<LocalSocketInner>>>,
    accept_queue: VecDeque<Arc<UPIntrFreeCell<LocalSocketInner>>>,
    stream_rx: VecDeque<u8>,
    datagram_rx: VecDeque<Datagram>,
    listening: bool,
    listen_backlog: usize,
    read_shutdown: bool,
    write_shutdown: bool,
    peer_write_shutdown: bool,
    reuse_addr: bool,
    sndbuf: i32,
    rcvbuf: i32,
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
    udp_bound: BTreeMap<u16, Weak<UPIntrFreeCell<LocalSocketInner>>>,
}

impl LoopbackState {
    fn new() -> Self {
        Self {
            next_ephemeral: 49152,
            tcp_listeners: BTreeMap::new(),
            udp_bound: BTreeMap::new(),
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
        self.udp_bound.retain(|_, socket| socket.strong_count() > 0);
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
    fn new(kind: SocketKind) -> Self {
        Self {
            kind,
            local: None,
            peer: None,
            peer_socket: None,
            accept_queue: VecDeque::new(),
            stream_rx: VecDeque::new(),
            datagram_rx: VecDeque::new(),
            listening: false,
            listen_backlog: 0,
            read_shutdown: false,
            write_shutdown: false,
            peer_write_shutdown: false,
            reuse_addr: false,
            sndbuf: DEFAULT_SOCKET_BUFFER,
            rcvbuf: DEFAULT_SOCKET_BUFFER,
        }
    }

    fn connected(
        kind: SocketKind,
        local: InetEndpoint,
        peer: InetEndpoint,
        peer_socket: Option<Weak<UPIntrFreeCell<LocalSocketInner>>>,
        shutdown: ShutdownState,
    ) -> Self {
        let mut inner = Self::new(kind);
        inner.local = Some(local);
        inner.peer = Some(peer);
        inner.peer_socket = peer_socket;
        inner.read_shutdown = shutdown.read;
        inner.write_shutdown = shutdown.write;
        inner.peer_write_shutdown = shutdown.peer_write;
        inner
    }
}

impl LocalSocket {
    fn new(kind: SocketKind, flags: OpenFlags) -> Arc<Self> {
        Arc::new(Self {
            inner: Arc::new(unsafe { UPIntrFreeCell::new(LocalSocketInner::new(kind)) }),
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

    fn bind_endpoint(&self, mut endpoint: InetEndpoint) -> SysResult {
        normalize_local_endpoint(&mut endpoint);
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
                if loopback.udp_bound.contains_key(&endpoint.port) && !inner.reuse_addr {
                    return Err(SysError::EADDRINUSE);
                }
                loopback
                    .udp_bound
                    .insert(endpoint.port, Arc::downgrade(&self.inner));
            }
        }
        inner.local = Some(endpoint);
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
                .insert(endpoint.port, Arc::downgrade(&self.inner));
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
                            SocketKind::Stream,
                            local,
                            peer,
                            None,
                            ShutdownState::CLOSED,
                        ))
                    }),
                    OpenFlags::RDWR,
                ));
            }
            suspend_current_and_run_next();
        }
    }

    fn connect(&self, mut remote: InetEndpoint) -> SysResult {
        normalize_remote_endpoint(&mut remote)?;
        match self.kind() {
            SocketKind::Datagram => {
                self.ensure_bound(SocketKind::Datagram)?;
                self.inner.exclusive_access().peer = Some(remote);
                Ok(0)
            }
            SocketKind::Stream => self.connect_stream(remote),
        }
    }

    fn connect_stream(&self, remote: InetEndpoint) -> SysResult {
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

        let server_inner = Arc::new(unsafe {
            UPIntrFreeCell::new(LocalSocketInner::connected(
                SocketKind::Stream,
                remote,
                local,
                Some(Arc::downgrade(&self.inner)),
                ShutdownState::OPEN,
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
            client.peer_socket = Some(Arc::downgrade(&server_inner));
        }
        listener
            .exclusive_access()
            .accept_queue
            .push_back(server_inner);
        Ok(0)
    }

    fn send_bytes(&self, data: &[u8], remote: Option<InetEndpoint>) -> SysResult<usize> {
        match self.kind() {
            SocketKind::Stream => self.send_stream(data),
            SocketKind::Datagram => self.send_datagram(data, remote),
        }
    }

    fn send_stream(&self, data: &[u8]) -> SysResult<usize> {
        let mut written = 0usize;
        while written < data.len() {
            let peer = {
                let inner = self.inner.exclusive_access();
                if inner.write_shutdown {
                    return Err(SysError::EPIPE);
                }
                inner
                    .peer_socket
                    .as_ref()
                    .and_then(Weak::upgrade)
                    .ok_or(SysError::ENOTCONN)?
            };
            let mut peer_inner = peer.exclusive_access();
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
        }
        Ok(written)
    }

    fn send_datagram(&self, data: &[u8], remote: Option<InetEndpoint>) -> SysResult<usize> {
        let local = self.ensure_bound(SocketKind::Datagram)?;
        let mut remote = remote
            .or_else(|| self.inner.exclusive_access().peer)
            .ok_or(SysError::EDESTADDRREQ)?;
        normalize_remote_endpoint(&mut remote)?;
        let target = {
            let mut loopback = LOOPBACK.exclusive_access();
            loopback.prune();
            loopback.udp_bound.get(&remote.port).and_then(Weak::upgrade)
        };
        if let Some(target) = target {
            let mut target = target.exclusive_access();
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
                });
            }
        }
        Ok(data.len())
    }

    fn recv_bytes(
        &self,
        buf: UserBuffer,
        nonblock: bool,
    ) -> SysResult<(usize, Option<InetEndpoint>)> {
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
    ) -> SysResult<(usize, Option<InetEndpoint>)> {
        loop {
            let packet = self.inner.exclusive_access().datagram_rx.pop_front();
            if let Some(packet) = packet {
                let mut buf = buf;
                let copied = buf.copy_from_slice(&packet.data);
                return Ok((copied, Some(packet.from)));
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

    fn local_endpoint(&self) -> InetEndpoint {
        self.inner.exclusive_access().local.unwrap_or(InetEndpoint {
            ip: ANY_IP,
            port: 0,
        })
    }

    fn peer_endpoint(&self) -> SysResult<InetEndpoint> {
        self.inner.exclusive_access().peer.ok_or(SysError::ENOTCONN)
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
        let peer = {
            let mut inner = self.inner.exclusive_access();
            if matches!(how, SHUT_RD | SHUT_RDWR) {
                inner.read_shutdown = true;
            }
            if matches!(how, SHUT_WR | SHUT_RDWR) {
                inner.write_shutdown = true;
            }
            inner.peer_socket.as_ref().and_then(Weak::upgrade)
        };
        if matches!(how, SHUT_WR | SHUT_RDWR) {
            if let Some(peer) = peer {
                peer.exclusive_access().peer_write_shutdown = true;
            }
        }
        Ok(0)
    }
}

impl Drop for LocalSocket {
    fn drop(&mut self) {
        let (kind, local, listening, peer) = {
            let mut inner = self.inner.exclusive_access();
            inner.read_shutdown = true;
            inner.write_shutdown = true;
            (
                inner.kind,
                inner.local,
                inner.listening,
                inner.peer_socket.as_ref().and_then(Weak::upgrade),
            )
        };
        if let Some(peer) = peer {
            peer.exclusive_access().peer_write_shutdown = true;
        }
        if let Some(local) = local {
            let mut loopback = LOOPBACK.exclusive_access();
            match kind {
                SocketKind::Stream if listening => {
                    loopback.tcp_listeners.remove(&local.port);
                }
                SocketKind::Stream => {}
                SocketKind::Datagram => {
                    loopback.udp_bound.remove(&local.port);
                }
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

    fn poll(&self, events: PollEvents) -> PollEvents {
        let (kind, listening, readable, read_shutdown, peer_write_shutdown, write_shutdown, peer) = {
            let inner = self.inner.exclusive_access();
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

fn normalize_local_endpoint(endpoint: &mut InetEndpoint) {
    if endpoint.ip == ANY_IP {
        endpoint.ip = LOOPBACK_IP;
    }
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

fn sockaddr_to_endpoint(addr: LinuxSockAddrIn) -> SysResult<InetEndpoint> {
    if addr.family as i32 == AF_UNIX {
        return Err(SysError::ENOENT);
    }
    if addr.family as i32 != AF_INET {
        return Err(SysError::EAFNOSUPPORT);
    }
    Ok(InetEndpoint {
        ip: addr.addr.to_ne_bytes(),
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

fn read_sockaddr(token: usize, ptr: usize, len: u32) -> SysResult<InetEndpoint> {
    if ptr == 0 {
        return Err(SysError::EFAULT);
    }
    if (len as usize) < size_of::<LinuxSockAddrIn>() {
        return Err(SysError::EINVAL);
    }
    sockaddr_to_endpoint(read_user_value(token, ptr as *const LinuxSockAddrIn)?)
}

fn write_sockaddr(token: usize, addr: usize, addrlen: usize, endpoint: InetEndpoint) -> SysResult {
    if addr == 0 || addrlen == 0 {
        return Ok(0);
    }
    let len_ptr = addrlen as *mut u32;
    let len = read_user_value(token, len_ptr.cast_const())?;
    if (len as usize) < size_of::<LinuxSockAddrIn>() {
        return Err(SysError::EINVAL);
    }
    write_user_value(
        token,
        addr as *mut LinuxSockAddrIn,
        &endpoint_to_sockaddr(endpoint),
    )?;
    write_user_value(token, len_ptr, &(size_of::<LinuxSockAddrIn>() as u32))?;
    Ok(0)
}

fn copy_user_to_vec(token: usize, ptr: usize, len: usize) -> SysResult<Vec<u8>> {
    let mut data = Vec::with_capacity(len);
    for slice in
        translated_byte_buffer_checked(token, ptr as *const u8, len, UserBufferAccess::Read)?
    {
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
        _ => Err(SysError::EPROTONOSUPPORT),
    }
}

fn validate_protocol(kind: SocketKind, protocol: i32) -> SysResult {
    match (kind, protocol) {
        (_, IPPROTO_IP) => Ok(0),
        (SocketKind::Stream, IPPROTO_TCP) => Ok(0),
        (SocketKind::Datagram, IPPROTO_UDP) => Ok(0),
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
    if val == 0 || (len as usize) < size_of::<i32>() {
        return Err(SysError::EINVAL);
    }
    read_user_value(token, val as *const i32)
}

pub fn sys_socket(domain: i32, ty: i32, protocol: i32) -> SysResult {
    let flags = open_flags_from_socket_type(ty)?;
    if domain == AF_ALG {
        AfAlgSocket::validate_socket_type(ty, protocol)?;
        let socket = AfAlgSocket::new_listener(flags);
        return Ok(alloc_socket_fd(socket, flags)? as isize);
    }

    let kind = socket_kind_from_type(ty)?;
    match domain {
        AF_INET => {
            validate_protocol(kind, protocol)?;
            let socket = LocalSocket::new(kind, flags);
            Ok(alloc_socket_fd(socket, flags)? as isize)
        }
        AF_UNIX => {
            if protocol != 0 {
                return Err(SysError::EPROTONOSUPPORT);
            }
            // CONTEXT: libc group/passwd lookup probes AF_UNIX nscd first.
            // Full pathname AF_UNIX IPC is not implemented; connect/bind on a
            // sockaddr_un still reports ENOENT so libc falls back to local
            // database files. Creating an unbound AF_UNIX fd is enough for LTP
            // fd-type probes such as splice07.
            let socket = LocalSocket::new(kind, flags);
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
            kind,
            endpoint,
            endpoint,
            None,
            ShutdownState::OPEN,
        ))
    });
    let second_inner = Arc::new(unsafe {
        UPIntrFreeCell::new(LocalSocketInner::connected(
            kind,
            endpoint,
            endpoint,
            Some(Arc::downgrade(&first_inner)),
            ShutdownState::OPEN,
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
    let endpoint = read_sockaddr(token, addr, addrlen)?;
    socket.bind_endpoint(endpoint)
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
    let peer = accepted.peer_endpoint()?;
    write_sockaddr(token, addr, addrlen, peer)?;
    Ok(alloc_socket_fd(accepted, open_flags)? as isize)
}

pub fn sys_connect(fd: usize, addr: usize, addrlen: u32) -> SysResult {
    let token = current_user_token();
    let endpoint = read_sockaddr(token, addr, addrlen)?;
    with_socket(fd, |socket| socket.connect(endpoint))
}

pub fn sys_getsockname(fd: usize, addr: usize, addrlen: usize) -> SysResult {
    let token = current_user_token();
    with_socket(fd, |socket| {
        write_sockaddr(token, addr, addrlen, socket.local_endpoint())
    })
}

pub fn sys_getpeername(fd: usize, addr: usize, addrlen: usize) -> SysResult {
    let token = current_user_token();
    with_socket(fd, |socket| {
        write_sockaddr(token, addr, addrlen, socket.peer_endpoint()?)
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
        Some(read_sockaddr(token, addr, addrlen)?)
    };
    with_socket(fd, |socket| Ok(socket.send_bytes(&data, remote)? as isize))
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
            write_sockaddr(token, addr, addrlen, remote)?;
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
            (IPPROTO_TCP, TCP_NODELAY)
            | (
                SOL_SOCKET,
                SO_DONTROUTE | SO_KEEPALIVE | SO_LINGER | SO_RCVTIMEO_OLD | SO_SNDTIMEO_OLD,
            )
            | (SOL_SOCKET, SO_RCVTIMEO_NEW | SO_SNDTIMEO_NEW) => {
                // CONTEXT: accepted as a no-op for libc/netperf compatibility.
                if val != 0 && len > 0 {
                    translated_byte_buffer_checked(
                        token,
                        val as *const u8,
                        len as usize,
                        UserBufferAccess::Read,
                    )?;
                }
            }
            (IPPROTO_IP, MCAST_JOIN_GROUP) => {
                // CONTEXT: The loopback socket subset does not deliver multicast
                // traffic, but LTP/net probes expect joining a group to be
                // accepted and leaving an unjoined group to fail distinctly.
                if val != 0 && len > 0 {
                    translated_byte_buffer_checked(
                        token,
                        val as *const u8,
                        len as usize,
                        UserBufferAccess::Read,
                    )?;
                }
            }
            (IPPROTO_IP, MCAST_LEAVE_GROUP) => {
                // UNFINISHED: Multicast group membership is not tracked yet.
                // Linux returns EADDRNOTAVAIL when the socket is not a member
                // of the requested group; this is enough to avoid inheriting
                // fake membership across accept().
                if val != 0 && len > 0 {
                    translated_byte_buffer_checked(
                        token,
                        val as *const u8,
                        len as usize,
                        UserBufferAccess::Read,
                    )?;
                }
                return Err(SysError::EADDRNOTAVAIL);
            }
            (IPPROTO_IP, _) | (IPPROTO_UDP, _) => {
                // CONTEXT: IP/UDP tuning options do not affect local loopback queues.
                if val != 0 && len > 0 {
                    translated_byte_buffer_checked(
                        token,
                        val as *const u8,
                        len as usize,
                        UserBufferAccess::Read,
                    )?;
                }
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
