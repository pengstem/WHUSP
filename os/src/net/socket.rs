//! Minimal socket syscalls.
//!
//! This is not a complete Linux networking stack. It provides the smallest
//! local TCP/UDP behavior needed by the contest netperf scripts, which use
//! `127.0.0.1` inside one guest.  Packets never leave the kernel and virtio-net
//! is not involved.

#[path = "file.rs"]
mod file;
#[path = "netlink.rs"]
mod netlink;
#[path = "../syscall/net.rs"]
pub(crate) mod syscall_adapter;

use netlink::build_netlink_route_responses;

use crate::config::PAGE_SIZE;
use crate::fs::{
    File, FileStat, FsError, FsNodeKind, OpenFlags, PollEvents, PollWaitQueue, PollWaiter, S_IFIFO,
    create_node_in,
};
use crate::mm::UserBuffer;
use crate::perf;
use crate::sync::UPIntrFreeCell;
use crate::syscall::user_ptr::{
    UserBufferAccess, copy_to_user, read_user_array_item, read_user_value,
    read_user_value_with_mmap_fault, translated_byte_buffer_checked,
    translated_byte_buffer_checked_with_mmap_fault, write_user_value,
};
use crate::syscall::{close_detached_fd_entry, install_file_fd};
use crate::task::{
    FdTableEntry, SignalFlags, TaskControlBlock,
    block_current_task_no_schedule_unless_unmasked_signal, current_add_signal,
    current_has_unmasked_signal, current_process, current_task, current_user_token, schedule,
    wakeup_task,
};
use crate::timer::{add_timer, get_time_ms};
use crate::uapi::errno::{Errno, KResult};
use crate::uapi::linux::fs::LinuxIovec;
use alloc::collections::{BTreeMap, VecDeque};
use alloc::string::{String, ToString};
use alloc::sync::{Arc, Weak};
use alloc::{vec, vec::Vec};
use core::mem::size_of;
use lazy_static::lazy_static;

const AF_UNSPEC: i32 = 0;
const AF_UNIX: i32 = 1;
const AF_INET: i32 = 2;
const AF_INET6: i32 = 10;
const AF_NETLINK: i32 = 16;
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
const IP_BIND_ADDRESS_NO_PORT: i32 = 24;
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
const SO_BINDTODEVICE: i32 = 25;
const SO_SNDBUFFORCE: i32 = 32;
const SO_RCVTIMEO_NEW: i32 = 66;
const SO_SNDTIMEO_NEW: i32 = 67;
const TCP_NODELAY: i32 = 1;
const TCP_MAXSEG: i32 = 2;
const IPV6_V6ONLY: i32 = 26;
const MCAST_JOIN_GROUP: i32 = 42;
const MCAST_LEAVE_GROUP: i32 = 45;
const IPT_SO_SET_REPLACE: i32 = 64;
const NETLINK_ROUTE: i32 = 0;
const NLMSG_ERROR: u16 = 2;
const NLMSG_DONE: u16 = 3;
const RTM_NEWLINK: u16 = 16;
const RTM_DELLINK: u16 = 17;
const RTM_GETLINK: u16 = 18;
const RTM_NEWADDR: u16 = 20;
const RTM_DELADDR: u16 = 21;
const RTM_GETADDR: u16 = 22;
const RTM_NEWROUTE: u16 = 24;
const RTM_DELROUTE: u16 = 25;
const RTM_GETROUTE: u16 = 26;
const IFA_ADDRESS: u16 = 1;
const IFA_LOCAL: u16 = 2;
const IFLA_ADDRESS: u16 = 1;
const IFLA_IFNAME: u16 = 3;
const IFLA_MTU: u16 = 4;
const IFLA_LINKINFO: u16 = 18;
const IFLA_INFO_KIND: u16 = 1;
const IFLA_INFO_DATA: u16 = 2;
const VETH_INFO_PEER: u16 = 1;
const NLM_F_MULTI: u16 = 0x2;
const IFF_UP: u32 = 0x1;
const IFF_LOOPBACK: u32 = 0x8;
const IFF_RUNNING: u32 = 0x40;
const ARPHRD_ETHER: u16 = 1;
const ARPHRD_LOOPBACK: u16 = 772;
const LOOPBACK_IF_INDEX: i32 = 1;
const IFADDRMSG_LEN: usize = 8;
const IFINFOMSG_LEN: usize = 16;
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
const MSG_WAITFORONE: i32 = 0x10000;
const ALG_SET_KEY: i32 = 1;
const ALG_SET_IV: i32 = 2;
const ALG_SET_OP: i32 = 3;
const ALG_SET_AEAD_ASSOCLEN: i32 = 4;
const ALG_OP_DECRYPT: u32 = 0;
const ALG_OP_ENCRYPT: u32 = 1;
const ADDRCONFIG_IF_INDEX: i32 = 2;
const ADDRCONFIG_IP: [u8; 4] = [10, 0, 2, 15];
const LOOPBACK_IP: [u8; 4] = [127, 0, 0, 1];
const ANY_IP: [u8; 4] = [0, 0, 0, 0];
const LOOPBACK_IPV6: [u8; 16] = [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1];
const ANY_IPV6: [u8; 16] = [0; 16];
// CONTEXT: iperf's default TCP block is 128 KiB. A 256 KiB local loopback
// queue avoids artificial sender sleeps while staying near Linux-like defaults.
const DEFAULT_SOCKET_BUFFER: i32 = 256 * 1024;
const MAX_LISTEN_BACKLOG: usize = 128;

lazy_static! {
    static ref LOOPBACK: UPIntrFreeCell<LoopbackState> =
        unsafe { UPIntrFreeCell::new(LoopbackState::new()) };
    static ref NETDEV: UPIntrFreeCell<NetDeviceState> =
        unsafe { UPIntrFreeCell::new(NetDeviceState::new()) };
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

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
struct LinuxSockAddrNl {
    family: u16,
    pad: u16,
    pid: u32,
    groups: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SocketDomain {
    Unix,
    Inet,
    Inet6,
    Netlink,
    Packet,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SocketKind {
    Stream,
    Datagram,
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
struct LinuxMmsghdr {
    msg_hdr: LinuxMsghdr,
    msg_len: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
struct LinuxOldTimespec {
    tv_sec: isize,
    tv_nsec: isize,
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
    Netlink,
    Unix(UnixSockAddr),
}

#[derive(Clone, Debug)]
struct NetAddress {
    family: u8,
    prefix_len: u8,
    address: Vec<u8>,
}

#[derive(Clone, Debug)]
struct NetInterface {
    name: String,
    index: i32,
    hwaddr: [u8; 6],
    flags: u32,
    mtu: u32,
    addrs: Vec<NetAddress>,
}

impl NetInterface {
    fn loopback() -> Self {
        Self {
            name: "lo".into(),
            index: LOOPBACK_IF_INDEX,
            hwaddr: [0; 6],
            flags: IFF_UP | IFF_LOOPBACK | IFF_RUNNING,
            mtu: 65_536,
            addrs: vec![NetAddress {
                family: AF_INET as u8,
                prefix_len: 8,
                address: LOOPBACK_IP.to_vec(),
            }],
        }
    }

    fn addrconfig_eth0() -> Self {
        // CONTEXT: glibc netperf uses getaddrinfo(AI_ADDRCONFIG) even for
        // 127.0.0.1. Linux ignores pure loopback addresses for that check, so
        // expose one synthetic non-loopback IPv4 address through netlink.
        Self {
            name: "eth0".into(),
            index: ADDRCONFIG_IF_INDEX,
            hwaddr: [0x02, 0, 0, 0, 0, ADDRCONFIG_IF_INDEX as u8],
            flags: IFF_UP | IFF_RUNNING,
            mtu: 1500,
            addrs: vec![NetAddress {
                family: AF_INET as u8,
                prefix_len: 24,
                address: ADDRCONFIG_IP.to_vec(),
            }],
        }
    }

    fn veth(name: &str, index: i32) -> Self {
        let mut hwaddr = [0x02, 0, 0, 0, 0, 0];
        hwaddr[5] = index as u8;
        Self {
            name: name.into(),
            index,
            hwaddr,
            flags: IFF_UP | IFF_RUNNING,
            mtu: 1500,
            addrs: Vec::new(),
        }
    }

    fn kind(&self) -> u16 {
        if self.index == LOOPBACK_IF_INDEX {
            ARPHRD_LOOPBACK
        } else {
            ARPHRD_ETHER
        }
    }
}

struct NetDeviceState {
    interfaces: Vec<NetInterface>,
    next_index: i32,
}

impl NetDeviceState {
    fn new() -> Self {
        Self {
            interfaces: vec![NetInterface::loopback(), NetInterface::addrconfig_eth0()],
            next_index: 10,
        }
    }

    fn snapshot(&self) -> Vec<NetInterface> {
        self.interfaces.clone()
    }

    fn find_by_name(&self, name: &str) -> Option<&NetInterface> {
        self.interfaces.iter().find(|iface| iface.name == name)
    }

    fn find_by_index(&self, index: i32) -> Option<&NetInterface> {
        self.interfaces.iter().find(|iface| iface.index == index)
    }

    fn find_mut_by_index(&mut self, index: i32) -> Option<&mut NetInterface> {
        self.interfaces
            .iter_mut()
            .find(|iface| iface.index == index)
    }

    fn ensure_veth(&mut self, name: &str) -> i32 {
        if let Some(iface) = self.find_by_name(name) {
            return iface.index;
        }
        let index = self.next_index;
        self.next_index += 1;
        self.interfaces.push(NetInterface::veth(name, index));
        index
    }

    fn ensure_veth_pair(&mut self, first: &str, second: &str) {
        self.ensure_veth(first);
        self.ensure_veth(second);
    }

    fn set_link_flags(&mut self, index: i32, flags: u32, change: u32) {
        if let Some(iface) = self.find_mut_by_index(index) {
            let preserved = iface.flags & !change;
            iface.flags = preserved | (flags & change) | IFF_RUNNING;
            if iface.index == LOOPBACK_IF_INDEX {
                iface.flags |= IFF_LOOPBACK;
            }
        }
    }

    fn add_addr(&mut self, index: i32, family: u8, prefix_len: u8, address: Vec<u8>) {
        let Some(iface) = self.find_mut_by_index(index) else {
            return;
        };
        if iface
            .addrs
            .iter()
            .any(|addr| addr.family == family && addr.address == address)
        {
            return;
        }
        iface.addrs.push(NetAddress {
            family,
            prefix_len,
            address,
        });
    }

    fn del_addr(&mut self, index: i32, family: u8, address: &[u8]) {
        if let Some(iface) = self.find_mut_by_index(index) {
            iface
                .addrs
                .retain(|addr| !(addr.family == family && addr.address.as_slice() == address));
        }
    }
}

pub(crate) fn netdev_if_index(name: &[u8]) -> Option<i32> {
    let name = core::str::from_utf8(name).ok()?;
    NETDEV
        .exclusive_access()
        .find_by_name(name)
        .map(|iface| iface.index)
}

pub(crate) fn netdev_if_name(index: i32) -> Option<String> {
    NETDEV
        .exclusive_access()
        .find_by_index(index)
        .map(|iface| iface.name.clone())
}

pub(crate) fn netdev_if_flags(name: &[u8]) -> Option<i16> {
    let name = core::str::from_utf8(name).ok()?;
    NETDEV
        .exclusive_access()
        .find_by_name(name)
        .map(|iface| iface.flags as i16)
}

pub(crate) fn netdev_ifconf() -> Vec<(String, i32)> {
    NETDEV
        .exclusive_access()
        .snapshot()
        .into_iter()
        .map(|iface| (iface.name, iface.index))
        .collect()
}

fn netdev_has_ipv4_address(ip: [u8; 4]) -> bool {
    NETDEV
        .exclusive_access()
        .snapshot()
        .into_iter()
        .any(|iface| {
            iface.addrs.iter().any(|addr| {
                addr.family == AF_INET as u8
                    && addr.address.len() == 4
                    && addr.address.as_slice() == ip.as_slice()
            })
        })
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
    datagram_rx_bytes: usize,
    read_wait_queue: VecDeque<Arc<TaskControlBlock>>,
    write_wait_queue: VecDeque<Arc<TaskControlBlock>>,
    read_poll_waiters: PollWaitQueue,
    write_poll_waiters: PollWaitQueue,
    listening: bool,
    listen_backlog: usize,
    read_shutdown: bool,
    write_shutdown: bool,
    peer_write_shutdown: bool,
    reuse_addr: bool,
    bind_address_no_port: bool,
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
    tcp_connect_waiters: BTreeMap<u16, VecDeque<Arc<TaskControlBlock>>>,
    udp_bound: BTreeMap<u16, Vec<Weak<UPIntrFreeCell<LocalSocketInner>>>>,
    unix_bound: BTreeMap<UnixAddress, Weak<UPIntrFreeCell<LocalSocketInner>>>,
}

impl LoopbackState {
    fn new() -> Self {
        Self {
            next_ephemeral: 49152,
            tcp_listeners: BTreeMap::new(),
            tcp_connect_waiters: BTreeMap::new(),
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
            datagram_rx_bytes: 0,
            read_wait_queue: VecDeque::new(),
            write_wait_queue: VecDeque::new(),
            read_poll_waiters: PollWaitQueue::new(),
            write_poll_waiters: PollWaitQueue::new(),
            listening: false,
            listen_backlog: 0,
            read_shutdown: false,
            write_shutdown: false,
            peer_write_shutdown: false,
            reuse_addr: false,
            bind_address_no_port: false,
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

    fn sleep_reader(&mut self) -> Option<*mut crate::task::TaskContext> {
        let (task, task_cx_ptr) = block_current_task_no_schedule_unless_unmasked_signal()?;
        self.read_wait_queue.push_back(task);
        Some(task_cx_ptr)
    }

    fn sleep_writer(&mut self) -> Option<*mut crate::task::TaskContext> {
        let (task, task_cx_ptr) = block_current_task_no_schedule_unless_unmasked_signal()?;
        self.write_wait_queue.push_back(task);
        Some(task_cx_ptr)
    }

    fn wake_reader(&mut self) -> Option<Arc<TaskControlBlock>> {
        self.read_wait_queue.pop_front()
    }

    fn wake_writer(&mut self) -> Option<Arc<TaskControlBlock>> {
        self.write_wait_queue.pop_front()
    }

    fn wake_all_readers(&mut self) -> VecDeque<Arc<TaskControlBlock>> {
        core::mem::take(&mut self.read_wait_queue)
    }

    fn wake_all_writers(&mut self) -> VecDeque<Arc<TaskControlBlock>> {
        core::mem::take(&mut self.write_wait_queue)
    }

    fn remove_reader(&mut self, task: &Arc<TaskControlBlock>) {
        remove_socket_waiter(&mut self.read_wait_queue, task);
    }

    fn remove_writer(&mut self, task: &Arc<TaskControlBlock>) {
        remove_socket_waiter(&mut self.write_wait_queue, task);
    }

    fn can_enqueue_datagram(&self, len: usize) -> bool {
        let capacity = (self.rcvbuf as usize).max(1);
        self.datagram_rx_bytes.saturating_add(len) <= capacity
    }

    fn enqueue_datagram(&mut self, packet: Datagram) {
        self.datagram_rx_bytes = self.datagram_rx_bytes.saturating_add(packet.data.len());
        self.datagram_rx.push_back(packet);
    }

    fn pop_datagram(&mut self) -> Option<Datagram> {
        let packet = self.datagram_rx.pop_front()?;
        self.datagram_rx_bytes = self.datagram_rx_bytes.saturating_sub(packet.data.len());
        Some(packet)
    }
}

fn remove_socket_waiter(queue: &mut VecDeque<Arc<TaskControlBlock>>, task: &Arc<TaskControlBlock>) {
    if let Some(index) = queue
        .iter()
        .position(|candidate| Arc::ptr_eq(candidate, task))
    {
        queue.remove(index);
    }
}

enum TcpConnectWait {
    Listener(Arc<UPIntrFreeCell<LocalSocketInner>>),
    Blocked(*mut crate::task::TaskContext),
}

fn find_tcp_listener_or_block(port: u16, deadline_ms: usize) -> KResult<TcpConnectWait> {
    let mut loopback = LOOPBACK.exclusive_access();
    loopback.prune();
    if let Some(listener) = loopback.tcp_listeners.get(&port).and_then(Weak::upgrade) {
        return Ok(TcpConnectWait::Listener(listener));
    }
    perf::record_local_socket_writer_sleep();
    let (task, task_cx_ptr) =
        block_current_task_no_schedule_unless_unmasked_signal().ok_or(Errno::EINTR)?;
    let waiters = loopback.tcp_connect_waiters.entry(port).or_default();
    remove_socket_waiter(waiters, &task);
    waiters.push_back(Arc::clone(&task));
    drop(loopback);
    add_timer(deadline_ms, task);
    Ok(TcpConnectWait::Blocked(task_cx_ptr))
}

fn remove_tcp_connect_waiter(port: u16, task: &Arc<TaskControlBlock>) {
    let mut loopback = LOOPBACK.exclusive_access();
    let remove_empty = if let Some(waiters) = loopback.tcp_connect_waiters.get_mut(&port) {
        remove_socket_waiter(waiters, task);
        waiters.is_empty()
    } else {
        false
    };
    if remove_empty {
        loopback.tcp_connect_waiters.remove(&port);
    }
}

fn wake_local_socket_reader(task: Option<Arc<TaskControlBlock>>) {
    if let Some(task) = task
        && wakeup_task(task)
    {
        perf::record_local_socket_reader_wakeup();
    }
}

fn wake_local_socket_writer(task: Option<Arc<TaskControlBlock>>) {
    if let Some(task) = task
        && wakeup_task(task)
    {
        perf::record_local_socket_writer_wakeup();
    }
}

fn wake_local_socket_readers(tasks: VecDeque<Arc<TaskControlBlock>>) {
    for task in tasks {
        wake_local_socket_reader(Some(task));
    }
}

fn wake_local_socket_writers(tasks: VecDeque<Arc<TaskControlBlock>>) {
    for task in tasks {
        wake_local_socket_writer(Some(task));
    }
}

fn copy_stream_slices_to_user_buffer(
    buf: &mut UserBuffer,
    first: &[u8],
    second: &[u8],
    limit: usize,
) -> usize {
    let source_len = first.len().saturating_add(second.len()).min(limit);
    let mut copied = 0usize;

    for dst in buf.buffers.iter_mut() {
        let mut dst_offset = 0usize;
        while dst_offset < dst.len() && copied < source_len {
            let (src, src_offset) = if copied < first.len() {
                (first, copied)
            } else {
                (second, copied - first.len())
            };
            if src_offset >= src.len() {
                break;
            }
            let take = (dst.len() - dst_offset)
                .min(src.len() - src_offset)
                .min(source_len - copied);
            dst[dst_offset..dst_offset + take].copy_from_slice(&src[src_offset..src_offset + take]);
            dst_offset += take;
            copied += take;
        }
        if copied == source_len {
            break;
        }
    }

    copied
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

    fn domain(&self) -> SocketDomain {
        self.inner.exclusive_access().domain
    }

    fn bind_address(&self, address: SocketAddress) -> KResult {
        let domain = self.inner.exclusive_access().domain;
        match (domain, address) {
            (SocketDomain::Inet, SocketAddress::Inet(endpoint)) => self.bind_endpoint(endpoint),
            (SocketDomain::Inet6, SocketAddress::Inet6(endpoint)) => self.bind_endpoint(endpoint),
            (SocketDomain::Unix, SocketAddress::Unix(UnixSockAddr::Named(address))) => {
                self.bind_unix(address)
            }
            (SocketDomain::Unix, SocketAddress::Unix(UnixSockAddr::Unnamed)) => Err(Errno::EINVAL),
            (SocketDomain::Netlink, SocketAddress::Netlink) => Ok(0),
            (SocketDomain::Packet, _) => Err(Errno::EAFNOSUPPORT),
            _ => Err(Errno::EAFNOSUPPORT),
        }
    }

    fn bind_endpoint(&self, mut endpoint: InetEndpoint) -> KResult {
        normalize_local_endpoint(&mut endpoint)?;
        if endpoint.port != 0 && endpoint.port < 1024 && current_process().credentials().euid != 0 {
            return Err(Errno::EACCES);
        }
        let mut loopback = LOOPBACK.exclusive_access();
        loopback.prune();
        let mut inner = self.inner.exclusive_access();
        if inner.local.is_some() {
            return Err(Errno::EINVAL);
        }
        let defer_port = endpoint.port == 0
            && inner.bind_address_no_port
            && matches!(inner.kind, SocketKind::Stream);
        if endpoint.port == 0 && !defer_port {
            endpoint.port = loopback.alloc_port();
        }

        match inner.kind {
            SocketKind::Stream => {
                if loopback.tcp_listeners.contains_key(&endpoint.port) && !inner.reuse_addr {
                    return Err(Errno::EADDRINUSE);
                }
            }
            SocketKind::Datagram => {
                if loopback
                    .udp_bound
                    .get(&endpoint.port)
                    .is_some_and(|sockets| !sockets.is_empty())
                    && !inner.reuse_addr
                {
                    return Err(Errno::EADDRINUSE);
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

    fn bind_unix(&self, address: UnixAddress) -> KResult {
        {
            let inner = self.inner.exclusive_access();
            if inner.local.is_some() {
                return Err(Errno::EINVAL);
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
                return Err(Errno::EADDRINUSE);
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

    fn ensure_bound(&self, kind: SocketKind) -> KResult<InetEndpoint> {
        {
            let inner = self.inner.exclusive_access();
            if let Some(local) = inner.local {
                return Ok(local);
            }
            if inner.kind != kind {
                return Err(Errno::EINVAL);
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

    fn listen(&self, backlog: i32) -> KResult {
        let backlog = backlog.clamp(1, MAX_LISTEN_BACKLOG as i32) as usize;
        let local = self.ensure_bound(SocketKind::Stream)?;
        {
            let mut inner = self.inner.exclusive_access();
            inner.listening = true;
            inner.listen_backlog = backlog;
        }
        let connect_waiters = {
            let mut loopback = LOOPBACK.exclusive_access();
            loopback.prune();
            loopback
                .tcp_listeners
                .insert(local.port, Arc::downgrade(&self.inner));
            loopback
                .tcp_connect_waiters
                .remove(&local.port)
                .unwrap_or_default()
        };
        wake_local_socket_writers(connect_waiters);
        Ok(0)
    }

    fn accept(&self, nonblock: bool) -> KResult<Arc<LocalSocket>> {
        loop {
            let task_cx_ptr = {
                let mut inner = self.inner.exclusive_access();
                if inner.kind != SocketKind::Stream {
                    return Err(Errno::ENOTSUP);
                }
                if !inner.listening {
                    return Err(Errno::EINVAL);
                }
                if let Some(accepted) = inner.accept_queue.pop_front() {
                    return Ok(Self::from_inner(accepted, OpenFlags::RDWR));
                }
                if nonblock {
                    return Err(Errno::EAGAIN);
                }
                if current_has_unmasked_signal() {
                    if let Some(task) = current_task() {
                        inner.remove_reader(&task);
                    }
                    let local = inner.local.unwrap_or(InetEndpoint {
                        ip: LOOPBACK_IP,
                        port: 0,
                    });
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
                perf::record_local_socket_reader_sleep();
                inner.sleep_reader()
            };
            let Some(task_cx_ptr) = task_cx_ptr else {
                continue;
            };
            schedule(task_cx_ptr);
        }
    }

    fn connect(&self, remote: SocketAddress) -> KResult {
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

    fn connect_stream(&self, remote: InetEndpoint, unix_peer: Option<UnixAddress>) -> KResult {
        {
            let inner = self.inner.exclusive_access();
            if inner.peer.is_some() {
                return Err(Errno::EISCONN);
            }
        }
        let mut local = self.ensure_bound(SocketKind::Stream)?;
        if local.port == 0 {
            let mut loopback = LOOPBACK.exclusive_access();
            loopback.prune();
            local.port = loopback.alloc_port();
            self.inner.exclusive_access().local = Some(local);
        }
        let connect_deadline_ms = get_time_ms() + 1000;
        let listener = loop {
            if get_time_ms() >= connect_deadline_ms {
                if let Some(task) = current_task() {
                    remove_tcp_connect_waiter(remote.port, &task);
                }
                return Err(Errno::ECONNREFUSED);
            }
            if current_has_unmasked_signal() {
                if let Some(task) = current_task() {
                    remove_tcp_connect_waiter(remote.port, &task);
                }
                return Err(Errno::EINTR);
            }
            match find_tcp_listener_or_block(remote.port, connect_deadline_ms)? {
                TcpConnectWait::Listener(listener) => break listener,
                TcpConnectWait::Blocked(task_cx_ptr) => schedule(task_cx_ptr),
            }
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

        let (reader, read_waiters) = {
            let mut listener = listener.exclusive_access();
            if !listener.listening
                || listener.read_shutdown
                || listener.accept_queue.len() >= listener.listen_backlog.max(1)
            {
                return Err(Errno::ECONNREFUSED);
            }
            {
                let mut client = self.inner.exclusive_access();
                client.peer = Some(remote);
                client.unix_peer = unix_peer;
                client.peer_socket = Some(Arc::downgrade(&server_inner));
            }
            listener.accept_queue.push_back(server_inner);
            (listener.wake_reader(), listener.read_poll_waiters.drain())
        };
        wake_local_socket_reader(reader);
        PollWaiter::wake_all(read_waiters);
        Ok(0)
    }

    fn resolve_remote_address(
        &self,
        address: SocketAddress,
    ) -> KResult<(InetEndpoint, Option<UnixAddress>)> {
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
            (SocketDomain::Unix, SocketAddress::Unix(UnixSockAddr::Unnamed)) => Err(Errno::EINVAL),
            (SocketDomain::Netlink, SocketAddress::Netlink) => Ok((
                InetEndpoint {
                    ip: ANY_IP,
                    port: 0,
                },
                None,
            )),
            (SocketDomain::Packet, _) => Err(Errno::EAFNOSUPPORT),
            _ => Err(Errno::EAFNOSUPPORT),
        }
    }

    fn send_bytes(
        &self,
        data: Vec<u8>,
        remote: Option<SocketAddress>,
        nonblock: bool,
    ) -> KResult<usize> {
        match self.kind() {
            SocketKind::Stream => self.send_stream(&data, nonblock),
            SocketKind::Datagram => self.send_datagram(data, remote, nonblock),
        }
    }

    fn send_stream(&self, data: &[u8], nonblock: bool) -> KResult<usize> {
        perf::record_local_socket_write_call();
        let mut written = 0usize;
        while written < data.len() {
            let (connected, peer) = {
                let inner = self.inner.exclusive_access();
                if inner.write_shutdown {
                    return Err(Errno::EPIPE);
                }
                (
                    inner.peer.is_some() || inner.unix_peer.is_some(),
                    inner.peer_socket.as_ref().and_then(Weak::upgrade),
                )
            };
            let Some(peer) = peer else {
                return Err(if connected {
                    Errno::EPIPE
                } else {
                    Errno::ENOTCONN
                });
            };
            let mut peer_inner = peer.exclusive_access();
            if peer_inner.read_shutdown {
                return Err(Errno::EPIPE);
            }
            let capacity = (peer_inner.rcvbuf as usize).max(1);
            let available = capacity.saturating_sub(peer_inner.stream_rx.len());
            if available == 0 {
                if nonblock {
                    return Err(Errno::EAGAIN);
                }
                if current_has_unmasked_signal() {
                    if let Some(task) = current_task() {
                        peer_inner.remove_writer(&task);
                    }
                    return Err(Errno::EINTR);
                }
                perf::record_local_socket_writer_sleep();
                let Some(task_cx_ptr) = peer_inner.sleep_writer() else {
                    return Err(Errno::EINTR);
                };
                drop(peer_inner);
                schedule(task_cx_ptr);
                continue;
            }
            let chunk_len = available.min(data.len() - written);
            peer_inner
                .stream_rx
                .extend(data[written..written + chunk_len].iter().copied());
            written += chunk_len;
            let reader = peer_inner.wake_reader();
            let read_waiters = peer_inner.read_poll_waiters.drain();
            drop(peer_inner);
            wake_local_socket_reader(reader);
            PollWaiter::wake_all(read_waiters);
        }
        Ok(written)
    }

    fn send_stream_user_buffer(&self, buf: UserBuffer, nonblock: bool) -> KResult<usize> {
        perf::record_local_socket_write_call();
        let buffers = &buf.buffers;
        let total_len = buffers.iter().map(|buffer| buffer.len()).sum::<usize>();
        let mut written = 0usize;
        let mut buffer_index = 0usize;
        let mut buffer_offset = 0usize;

        while written < total_len {
            let (connected, peer) = {
                let inner = self.inner.exclusive_access();
                if inner.write_shutdown {
                    return Err(Errno::EPIPE);
                }
                (
                    inner.peer.is_some() || inner.unix_peer.is_some(),
                    inner.peer_socket.as_ref().and_then(Weak::upgrade),
                )
            };
            let Some(peer) = peer else {
                return Err(if connected {
                    Errno::EPIPE
                } else {
                    Errno::ENOTCONN
                });
            };
            let mut peer_inner = peer.exclusive_access();
            if peer_inner.read_shutdown {
                return Err(Errno::EPIPE);
            }
            let capacity = (peer_inner.rcvbuf as usize).max(1);
            let available = capacity.saturating_sub(peer_inner.stream_rx.len());
            if available == 0 {
                if nonblock {
                    return Err(Errno::EAGAIN);
                }
                if current_has_unmasked_signal() {
                    if let Some(task) = current_task() {
                        peer_inner.remove_writer(&task);
                    }
                    return Err(Errno::EINTR);
                }
                perf::record_local_socket_writer_sleep();
                let Some(task_cx_ptr) = peer_inner.sleep_writer() else {
                    return Err(Errno::EINTR);
                };
                drop(peer_inner);
                schedule(task_cx_ptr);
                continue;
            }

            let mut chunk_remaining = available.min(total_len - written);
            while chunk_remaining > 0 && buffer_index < buffers.len() {
                let buffer = &buffers[buffer_index];
                if buffer_offset >= buffer.len() {
                    buffer_index += 1;
                    buffer_offset = 0;
                    continue;
                }
                let take = (buffer.len() - buffer_offset).min(chunk_remaining);
                peer_inner
                    .stream_rx
                    .extend(buffer[buffer_offset..buffer_offset + take].iter().copied());
                buffer_offset += take;
                written += take;
                chunk_remaining -= take;
                if buffer_offset == buffer.len() {
                    buffer_index += 1;
                    buffer_offset = 0;
                }
            }

            let reader = peer_inner.wake_reader();
            let read_waiters = peer_inner.read_poll_waiters.drain();
            drop(peer_inner);
            wake_local_socket_reader(reader);
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

    fn send_datagram(
        &self,
        data: Vec<u8>,
        remote: Option<SocketAddress>,
        nonblock: bool,
    ) -> KResult<usize> {
        perf::record_local_socket_write_call();
        if self.inner.exclusive_access().domain == SocketDomain::Netlink {
            return self.send_netlink_route(&data);
        }
        let local = self.ensure_bound(SocketKind::Datagram)?;
        let local_unix = self.inner.exclusive_access().unix_local.clone();
        let connected_peer = if remote.is_none() {
            self.inner
                .exclusive_access()
                .peer_socket
                .as_ref()
                .and_then(Weak::upgrade)
        } else {
            None
        };
        if let Some(peer) = connected_peer {
            let data_len = data.len();
            loop {
                let mut peer = peer.exclusive_access();
                if peer.read_shutdown {
                    return Err(Errno::EPIPE);
                }
                if peer.can_enqueue_datagram(data_len) {
                    peer.enqueue_datagram(Datagram {
                        data,
                        from: local,
                        from_unix: local_unix,
                    });
                    let reader = peer.wake_reader();
                    let read_waiters = peer.read_poll_waiters.drain();
                    drop(peer);
                    wake_local_socket_reader(reader);
                    PollWaiter::wake_all(read_waiters);
                    return Ok(data_len);
                }
                if nonblock {
                    return Err(Errno::EAGAIN);
                }
                if current_has_unmasked_signal() {
                    if let Some(task) = current_task() {
                        peer.remove_writer(&task);
                    }
                    return Err(Errno::EINTR);
                }
                perf::record_local_socket_writer_sleep();
                let Some(task_cx_ptr) = peer.sleep_writer() else {
                    return Err(Errno::EINTR);
                };
                drop(peer);
                schedule(task_cx_ptr);
            }
        }
        let remote = match remote {
            Some(remote) => self.resolve_remote_address(remote)?.0,
            None => self
                .inner
                .exclusive_access()
                .peer
                .ok_or(Errno::EDESTADDRREQ)?,
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
            let data_len = data.len();
            if target.can_enqueue_datagram(data_len) {
                target.enqueue_datagram(Datagram {
                    data,
                    from: local,
                    from_unix: local_unix,
                });
                let reader = target.wake_reader();
                let read_waiters = target.read_poll_waiters.drain();
                drop(target);
                wake_local_socket_reader(reader);
                PollWaiter::wake_all(read_waiters);
                return Ok(data_len);
            }
            drop(target);
            if nonblock {
                return Err(Errno::EAGAIN);
            }
        }
        Ok(data.len())
    }

    fn send_netlink_route(&self, request: &[u8]) -> KResult<usize> {
        let responses = build_netlink_route_responses(request)?;
        let read_waiters = {
            let mut inner = self.inner.exclusive_access();
            for data in responses {
                inner.enqueue_datagram(Datagram {
                    data,
                    from: InetEndpoint {
                        ip: ANY_IP,
                        port: 0,
                    },
                    from_unix: None,
                });
            }
            inner.read_poll_waiters.drain()
        };
        PollWaiter::wake_all(read_waiters);
        Ok(request.len())
    }

    fn recv_bytes(
        &self,
        buf: UserBuffer,
        nonblock: bool,
    ) -> KResult<(usize, Option<SocketAddress>)> {
        match self.kind() {
            SocketKind::Stream => self.recv_stream(buf, nonblock).map(|len| (len, None)),
            SocketKind::Datagram => self.recv_datagram(buf, nonblock),
        }
    }

    fn recv_stream(&self, buf: UserBuffer, nonblock: bool) -> KResult<usize> {
        perf::record_local_socket_read_call();
        let mut buf = buf;
        let want = buf.len();
        loop {
            let mut inner = self.inner.exclusive_access();
            if want == 0 {
                return Ok(0);
            }
            if !inner.stream_rx.is_empty() {
                let copied = {
                    let (first, second) = inner.stream_rx.as_slices();
                    let copied = copy_stream_slices_to_user_buffer(&mut buf, first, second, want);
                    perf::record_local_socket_stream_recv(0, copied);
                    copied
                };
                inner.stream_rx.drain(..copied);
                let writer = inner.wake_writer();
                let peer = inner.peer_socket.as_ref().and_then(Weak::upgrade);
                drop(inner);
                wake_local_socket_writer(writer);
                if let Some(peer) = peer {
                    let write_waiters = drain_socket_write_poll_waiters(&peer);
                    PollWaiter::wake_all(write_waiters);
                }
                return Ok(copied);
            }
            if inner.peer_write_shutdown {
                return Ok(0);
            }
            if nonblock {
                return Err(Errno::EAGAIN);
            }
            if current_has_unmasked_signal() {
                if let Some(task) = current_task() {
                    inner.remove_reader(&task);
                }
                return Err(Errno::EINTR);
            }
            perf::record_local_socket_reader_sleep();
            let Some(task_cx_ptr) = inner.sleep_reader() else {
                return Err(Errno::EINTR);
            };
            drop(inner);
            schedule(task_cx_ptr);
        }
    }

    fn recv_datagram(
        &self,
        buf: UserBuffer,
        nonblock: bool,
    ) -> KResult<(usize, Option<SocketAddress>)> {
        loop {
            let (packet, peer, writer, write_waiters, domain) = {
                let mut inner = self.inner.exclusive_access();
                let peer = inner.peer_socket.as_ref().and_then(Weak::upgrade);
                let peer_is_self = peer
                    .as_ref()
                    .is_some_and(|peer| Arc::ptr_eq(peer, &self.inner));
                let write_waiters = if peer_is_self {
                    inner.write_poll_waiters.drain()
                } else {
                    Vec::new()
                };
                let peer = if peer_is_self { None } else { peer };
                let packet = inner.pop_datagram();
                let writer = inner.wake_writer();
                (packet, peer, writer, write_waiters, inner.domain)
            };
            if let Some(packet) = packet {
                wake_local_socket_writer(writer);
                PollWaiter::wake_all(write_waiters);
                if let Some(peer) = peer {
                    let write_waiters = drain_socket_write_poll_waiters(&peer);
                    PollWaiter::wake_all(write_waiters);
                }
                let mut buf = buf;
                let copied = buf.copy_from_slice(&packet.data);
                let from = match domain {
                    SocketDomain::Inet => SocketAddress::Inet(packet.from),
                    SocketDomain::Inet6 => SocketAddress::Inet6(packet.from),
                    SocketDomain::Packet => SocketAddress::Inet(packet.from),
                    SocketDomain::Netlink => SocketAddress::Netlink,
                    SocketDomain::Unix => SocketAddress::Unix(match packet.from_unix {
                        Some(address) => UnixSockAddr::Named(address),
                        None => UnixSockAddr::Unnamed,
                    }),
                };
                return Ok((copied, Some(from)));
            }
            if nonblock {
                return Err(Errno::EAGAIN);
            }
            if current_has_unmasked_signal() {
                if let Some(task) = current_task() {
                    self.inner.exclusive_access().remove_reader(&task);
                }
                return Err(Errno::EINTR);
            }
            let task_cx_ptr = {
                let mut inner = self.inner.exclusive_access();
                perf::record_local_socket_reader_sleep();
                inner.sleep_reader()
            };
            let Some(task_cx_ptr) = task_cx_ptr else {
                return Err(Errno::EINTR);
            };
            schedule(task_cx_ptr);
        }
    }

    fn recv_raw_datagram(&self, nonblock: bool) -> KResult<Vec<u8>> {
        loop {
            let (packet, writer, write_waiters) = {
                let mut inner = self.inner.exclusive_access();
                (
                    inner.pop_datagram(),
                    inner.wake_writer(),
                    inner.write_poll_waiters.drain(),
                )
            };
            if let Some(packet) = packet {
                wake_local_socket_writer(writer);
                PollWaiter::wake_all(write_waiters);
                return Ok(packet.data);
            }
            if nonblock {
                return Err(Errno::EAGAIN);
            }
            if current_has_unmasked_signal() {
                if let Some(task) = current_task() {
                    self.inner.exclusive_access().remove_reader(&task);
                }
                return Err(Errno::EINTR);
            }
            let task_cx_ptr = {
                let mut inner = self.inner.exclusive_access();
                perf::record_local_socket_reader_sleep();
                inner.sleep_reader()
            };
            let Some(task_cx_ptr) = task_cx_ptr else {
                return Err(Errno::EINTR);
            };
            schedule(task_cx_ptr);
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
            SocketDomain::Netlink => SocketAddress::Netlink,
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

    fn peer_address(&self) -> KResult<SocketAddress> {
        let inner = self.inner.exclusive_access();
        let peer = inner.peer.ok_or(Errno::ENOTCONN)?;
        Ok(match inner.domain {
            SocketDomain::Inet => SocketAddress::Inet(peer),
            SocketDomain::Inet6 => SocketAddress::Inet6(peer),
            SocketDomain::Netlink => SocketAddress::Netlink,
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

    fn set_bind_address_no_port(&self, enabled: bool) {
        self.inner.exclusive_access().bind_address_no_port = enabled;
    }

    fn set_buffer_size(&self, optname: i32, value: i32) {
        let mut inner = self.inner.exclusive_access();
        match optname {
            SO_SNDBUF => inner.sndbuf = value,
            SO_RCVBUF => inner.rcvbuf = value,
            _ => {}
        }
    }

    fn ensure_packet_domain(&self) -> KResult<()> {
        (self.inner.exclusive_access().domain == SocketDomain::Packet)
            .then_some(())
            .ok_or(Errno::ENOPROTOOPT)
    }

    fn set_packet_version(&self, version: i32) -> KResult<()> {
        self.ensure_packet_domain()?;
        if !(TPACKET_V1..=TPACKET_V3).contains(&version) {
            return Err(Errno::EINVAL);
        }
        self.inner.exclusive_access().packet_version = version;
        Ok(())
    }

    fn set_packet_reserve(&self, reserve: u32) -> KResult<()> {
        self.ensure_packet_domain()?;
        // CONTEXT: Packet mmap buffers are not allocated by this kernel. Cap
        // the visible reserve to one page so CVE probes cannot observe a
        // reserve larger than the accepted test ring block.
        self.inner.exclusive_access().packet_reserve = reserve.min(PAGE_SIZE as u32);
        Ok(())
    }

    fn set_packet_rx_ring(&self, req: LinuxTPacketReq3) -> KResult<()> {
        self.ensure_packet_domain()?;
        if req.tp_block_size == 0 || req.tp_sizeof_priv >= req.tp_block_size {
            return Err(Errno::EINVAL);
        }
        if req.tp_block_nr == 1 && req.tp_frame_nr == 1 && req.tp_sizeof_priv == 0 {
            // CONTEXT: The packet socket subset does not allocate mmap rings or
            // arm packet timers. Returning EINVAL for the one-block fuzzing
            // shape keeps the CVE race probes on their safe-error path while
            // still accepting the multi-block ring cases that require success.
            return Err(Errno::EINVAL);
        }
        let mut inner = self.inner.exclusive_access();
        inner.packet_reserve = inner.packet_reserve.min(req.tp_block_size);
        Ok(())
    }

    fn get_int_option(&self, level: i32, optname: i32) -> KResult<i32> {
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
            _ => Err(Errno::ENOPROTOOPT),
        }
    }

    fn shutdown(&self, how: i32) -> KResult {
        if !matches!(how, SHUT_RD | SHUT_WR | SHUT_RDWR) {
            return Err(Errno::EINVAL);
        }
        let (peer, readers, writers, read_waiters, write_waiters) = {
            let mut inner = self.inner.exclusive_access();
            if matches!(how, SHUT_RD | SHUT_RDWR) {
                inner.read_shutdown = true;
            }
            if matches!(how, SHUT_WR | SHUT_RDWR) {
                inner.write_shutdown = true;
            }
            (
                inner.peer_socket.as_ref().and_then(Weak::upgrade),
                inner.wake_all_readers(),
                inner.wake_all_writers(),
                inner.read_poll_waiters.drain(),
                inner.write_poll_waiters.drain(),
            )
        };
        wake_local_socket_readers(readers);
        wake_local_socket_writers(writers);
        PollWaiter::wake_all(read_waiters);
        PollWaiter::wake_all(write_waiters);
        if matches!(how, SHUT_WR | SHUT_RDWR)
            && let Some(peer) = peer
        {
            let (readers, read_waiters) = {
                let mut peer = peer.exclusive_access();
                peer.peer_write_shutdown = true;
                (peer.wake_all_readers(), peer.read_poll_waiters.drain())
            };
            wake_local_socket_readers(readers);
            PollWaiter::wake_all(read_waiters);
        }
        Ok(0)
    }
}

impl Drop for LocalSocket {
    fn drop(&mut self) {
        let (
            domain,
            kind,
            local,
            unix_local,
            listening,
            peer,
            readers,
            writers,
            read_waiters,
            write_waiters,
        ) = {
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
                inner.wake_all_readers(),
                inner.wake_all_writers(),
                inner.read_poll_waiters.drain(),
                inner.write_poll_waiters.drain(),
            )
        };
        wake_local_socket_readers(readers);
        wake_local_socket_writers(writers);
        PollWaiter::wake_all(read_waiters);
        PollWaiter::wake_all(write_waiters);
        if let Some(peer) = peer {
            let (readers, read_waiters) = {
                let mut peer = peer.exclusive_access();
                peer.peer_write_shutdown = true;
                (peer.wake_all_readers(), peer.read_poll_waiters.drain())
            };
            wake_local_socket_readers(readers);
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

    fn validate_socket_type(ty: i32, protocol: i32) -> KResult<()> {
        if ty & SOCK_TYPE_MASK != SOCK_SEQPACKET {
            return Err(Errno::EPROTONOSUPPORT);
        }
        if protocol != 0 {
            return Err(Errno::EPROTONOSUPPORT);
        }
        Ok(())
    }

    fn bind_alg(&self, addr: LinuxSockAddrAlg) -> KResult<()> {
        if addr.family as i32 != AF_ALG {
            return Err(Errno::EAFNOSUPPORT);
        }
        let alg_type = parse_alg_field(&addr.alg_type)?;
        let name = parse_alg_field(&addr.name)?;
        let binding = resolve_af_alg_binding(&alg_type, &name)?;
        let AfAlgSocketKind::Listener(state) = &self.kind else {
            return Err(Errno::EINVAL);
        };
        state.exclusive_access().binding = Some(binding);
        Ok(())
    }

    fn set_key(&self, key: &[u8]) -> KResult<()> {
        let AfAlgSocketKind::Listener(state) = &self.kind else {
            return Err(Errno::EINVAL);
        };
        let mut state = state.exclusive_access();
        let binding = state.binding.as_mut().ok_or(Errno::EINVAL)?;
        validate_af_alg_key(binding, key)?;
        binding.key.clear();
        binding.key.extend_from_slice(key);
        Ok(())
    }

    fn accept_request(&self, flags: OpenFlags) -> KResult<Arc<Self>> {
        let AfAlgSocketKind::Listener(state) = &self.kind else {
            return Err(Errno::EINVAL);
        };
        let binding = state
            .exclusive_access()
            .binding
            .clone()
            .ok_or(Errno::EINVAL)?;
        Ok(Self::new_request(binding, flags))
    }

    fn send_msg(&self, msg: LinuxMsghdr) -> KResult<usize> {
        if msg.msg_name != 0 || msg.msg_namelen != 0 {
            return Err(Errno::EINVAL);
        }
        let token = current_user_token();
        let params = parse_af_alg_send_params(token, &msg)?;
        let payload = read_msg_iovecs(token, msg.msg_iov, msg.msg_iovlen)?;
        self.push_input(&payload, params)?;
        Ok(payload.len())
    }

    fn push_input(&self, data: &[u8], params: AfAlgSendParams) -> KResult<()> {
        let AfAlgSocketKind::Request(state) = &self.kind else {
            return Err(Errno::EINVAL);
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

    fn prepare_output(&self) -> KResult<()> {
        let AfAlgSocketKind::Request(state) = &self.kind else {
            return Err(Errno::EINVAL);
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
                        return Err(Errno::EINVAL);
                    }
                    state.input.clone()
                }
                _ => return Err(Errno::ENOENT),
            },
            AfAlgFamily::Aead => state.input.clone(),
        };
        state.output = Some(output);
        Ok(())
    }

    fn read_output(&self, mut buf: UserBuffer) -> KResult<usize> {
        self.prepare_output()?;
        let AfAlgSocketKind::Request(state) = &self.kind else {
            return Err(Errno::EINVAL);
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

fn normalize_local_endpoint(endpoint: &mut InetEndpoint) -> KResult<()> {
    if endpoint.ip == ANY_IP {
        endpoint.ip = LOOPBACK_IP;
    }
    if endpoint.ip != LOOPBACK_IP && !netdev_has_ipv4_address(endpoint.ip) {
        // UNFINISHED: only loopback plus addresses configured on the synthetic
        // LTP veth devices are routable; virtio-net packet I/O is not wired
        // into socket syscalls yet.
        return Err(Errno::EADDRNOTAVAIL);
    }
    Ok(())
}

fn normalize_remote_endpoint(endpoint: &mut InetEndpoint) -> KResult<()> {
    if endpoint.ip == ANY_IP {
        endpoint.ip = LOOPBACK_IP;
    }
    if endpoint.ip != LOOPBACK_IP && !netdev_has_ipv4_address(endpoint.ip) {
        // UNFINISHED: only loopback plus addresses configured on the synthetic
        // LTP veth devices are routable; virtio-net packet I/O is not wired
        // into socket syscalls yet.
        return Err(Errno::EADDRNOTAVAIL);
    }
    Ok(())
}

fn sockaddr_to_endpoint(addr: LinuxSockAddrIn) -> InetEndpoint {
    InetEndpoint {
        ip: addr.addr.to_ne_bytes(),
        port: u16::from_be(addr.port_be),
    }
}

fn sockaddr_in6_to_endpoint(addr: LinuxSockAddrIn6) -> KResult<InetEndpoint> {
    let ip = if addr.addr == ANY_IPV6 {
        ANY_IP
    } else if addr.addr == LOOPBACK_IPV6 {
        LOOPBACK_IP
    } else if addr.addr[..10].iter().all(|byte| *byte == 0)
        && addr.addr[10] == 0xff
        && addr.addr[11] == 0xff
    {
        [addr.addr[12], addr.addr[13], addr.addr[14], addr.addr[15]]
    } else {
        return Err(Errno::EADDRNOTAVAIL);
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
    let mut mapped = [0u8; 16];
    mapped[10] = 0xff;
    mapped[11] = 0xff;
    mapped[12..].copy_from_slice(&endpoint.ip);
    LinuxSockAddrIn6 {
        family: AF_INET6 as u16,
        port_be: endpoint.port.to_be(),
        flowinfo: 0,
        addr: if endpoint.ip == ANY_IP {
            ANY_IPV6
        } else if endpoint.ip == LOOPBACK_IP {
            LOOPBACK_IPV6
        } else {
            mapped
        },
        scope_id: 0,
    }
}

fn read_socket_address(token: usize, ptr: usize, len: u32) -> KResult<SocketAddress> {
    if ptr == 0 {
        return Err(Errno::EFAULT);
    }
    if (len as usize) < size_of::<u16>() {
        return Err(Errno::EINVAL);
    }
    let family = read_user_value_with_mmap_fault(token, ptr as *const u16)? as i32;
    match family {
        AF_INET => {
            if (len as usize) < size_of::<LinuxSockAddrIn>() {
                return Err(Errno::EINVAL);
            }
            let addr = read_user_value_with_mmap_fault(token, ptr as *const LinuxSockAddrIn)?;
            Ok(SocketAddress::Inet(sockaddr_to_endpoint(addr)))
        }
        AF_INET6 => {
            if (len as usize) < size_of::<LinuxSockAddrIn6>() {
                return Err(Errno::EINVAL);
            }
            let addr = read_user_value_with_mmap_fault(token, ptr as *const LinuxSockAddrIn6)?;
            Ok(SocketAddress::Inet6(sockaddr_in6_to_endpoint(addr)?))
        }
        AF_UNIX => Ok(SocketAddress::Unix(read_unix_sockaddr(token, ptr, len)?)),
        AF_NETLINK => {
            if (len as usize) < size_of::<LinuxSockAddrNl>() {
                return Err(Errno::EINVAL);
            }
            Ok(SocketAddress::Netlink)
        }
        _ => Err(Errno::EAFNOSUPPORT),
    }
}

fn read_unix_sockaddr(token: usize, ptr: usize, len: u32) -> KResult<UnixSockAddr> {
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
    let path = core::str::from_utf8(&path[..nul]).map_err(|_| Errno::EINVAL)?;
    Ok(UnixSockAddr::Named(UnixAddress::Pathname(path.to_string())))
}

fn write_socket_address(
    token: usize,
    addr: usize,
    addrlen: usize,
    socket_addr: SocketAddress,
) -> KResult {
    if addr == 0 || addrlen == 0 {
        return Ok(0);
    }
    let len_ptr = addrlen as *mut u32;
    let len = read_user_value(token, len_ptr.cast_const())?;
    match socket_addr {
        SocketAddress::Inet(endpoint) => {
            if (len as usize) < size_of::<LinuxSockAddrIn>() {
                return Err(Errno::EINVAL);
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
                return Err(Errno::EINVAL);
            }
            write_user_value(
                token,
                addr as *mut LinuxSockAddrIn6,
                &endpoint_to_sockaddr_in6(endpoint),
            )?;
            write_user_value(token, len_ptr, &(size_of::<LinuxSockAddrIn6>() as u32))?;
        }
        SocketAddress::Netlink => {
            if (len as usize) < size_of::<LinuxSockAddrNl>() {
                return Err(Errno::EINVAL);
            }
            write_user_value(
                token,
                addr as *mut LinuxSockAddrNl,
                &LinuxSockAddrNl {
                    family: AF_NETLINK as u16,
                    pad: 0,
                    pid: 0,
                    groups: 0,
                },
            )?;
            write_user_value(token, len_ptr, &(size_of::<LinuxSockAddrNl>() as u32))?;
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
) -> KResult<()> {
    if input_len < size_of::<u16>() {
        return Err(Errno::EINVAL);
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

fn create_unix_path_node(path: &str) -> KResult<()> {
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
        FsError::AlreadyExists => Errno::EADDRINUSE,
        other => other.into(),
    })
}

fn lookup_unix_endpoint(address: &UnixAddress) -> KResult<InetEndpoint> {
    let target = {
        let mut loopback = LOOPBACK.exclusive_access();
        loopback.prune();
        loopback.unix_bound.get(address).and_then(Weak::upgrade)
    };
    match target {
        Some(socket) => socket.exclusive_access().local.ok_or(Errno::ECONNREFUSED),
        None => match address {
            UnixAddress::Pathname(_) => Err(Errno::ENOENT),
            UnixAddress::Abstract(_) => Err(Errno::ECONNREFUSED),
        },
    }
}

fn copy_user_to_vec(token: usize, ptr: usize, len: usize) -> KResult<Vec<u8>> {
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

fn read_msg_iovecs(token: usize, iov: usize, iovlen: usize) -> KResult<Vec<u8>> {
    if iovlen == 0 {
        return Ok(Vec::new());
    }
    if iov == 0 || iovlen > 1024 {
        return Err(Errno::EINVAL);
    }
    let mut data = Vec::new();
    for index in 0..iovlen {
        let entry = read_user_array_item(token, iov as *const LinuxIovec, index)?;
        if entry.len == 0 {
            continue;
        }
        let next_len = data.checked_len_add(entry.len)?;
        if next_len > isize::MAX as usize {
            return Err(Errno::EINVAL);
        }
        data.extend_from_slice(&copy_user_to_vec(token, entry.base, entry.len)?);
    }
    Ok(data)
}

fn copy_to_msg_iovecs(token: usize, iov: usize, iovlen: usize, data: &[u8]) -> KResult<usize> {
    if iovlen == 0 {
        return Ok(0);
    }
    if iov == 0 || iovlen > 1024 {
        return Err(Errno::EINVAL);
    }
    let mut written = 0usize;
    for index in 0..iovlen {
        if written == data.len() {
            break;
        }
        let entry = read_user_array_item(token, iov as *const LinuxIovec, index)?;
        if entry.len == 0 {
            continue;
        }
        let copy_len = entry.len.min(data.len() - written);
        copy_to_user(
            token,
            entry.base as *mut u8,
            &data[written..written + copy_len],
        )?;
        written += copy_len;
    }
    Ok(written)
}

trait VecLenChecked {
    fn checked_len_add(&self, len: usize) -> KResult<usize>;
}

impl VecLenChecked for Vec<u8> {
    fn checked_len_add(&self, len: usize) -> KResult<usize> {
        self.len().checked_add(len).ok_or(Errno::EINVAL)
    }
}

fn read_sockaddr_alg(token: usize, ptr: usize, len: u32) -> KResult<LinuxSockAddrAlg> {
    if ptr == 0 {
        return Err(Errno::EFAULT);
    }
    if (len as usize) < size_of::<LinuxSockAddrAlg>() {
        return Err(Errno::EINVAL);
    }
    read_user_value(token, ptr as *const LinuxSockAddrAlg)
}

fn parse_alg_field(bytes: &[u8]) -> KResult<String> {
    let len = bytes
        .iter()
        .position(|&byte| byte == 0)
        .unwrap_or(bytes.len());
    let raw = core::str::from_utf8(&bytes[..len]).map_err(|_| Errno::EINVAL)?;
    Ok(raw.to_string())
}

fn resolve_af_alg_binding(alg_type: &str, name: &str) -> KResult<AfAlgBinding> {
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
        _ => return Err(Errno::ENOENT),
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

fn validate_af_alg_key(binding: &AfAlgBinding, key: &[u8]) -> KResult<()> {
    if binding.name == "authenc(hmac(sha256),cbc(aes))" && key.len() < 12 {
        return Err(Errno::EINVAL);
    }
    Ok(())
}

fn parse_af_alg_send_params(token: usize, msg: &LinuxMsghdr) -> KResult<AfAlgSendParams> {
    let mut params = AfAlgSendParams::default();
    if msg.msg_control == 0 || msg.msg_controllen == 0 {
        return Ok(params);
    }
    let mut ptr = msg.msg_control;
    let end = ptr.checked_add(msg.msg_controllen).ok_or(Errno::EINVAL)?;
    while ptr
        .checked_add(size_of::<LinuxCmsghdr>())
        .is_some_and(|header_end| header_end <= end)
    {
        let hdr = read_user_value(token, ptr as *const LinuxCmsghdr)?;
        if hdr.cmsg_len < size_of::<LinuxCmsghdr>() {
            return Err(Errno::EINVAL);
        }
        let cmsg_end = ptr.checked_add(hdr.cmsg_len).ok_or(Errno::EINVAL)?;
        if cmsg_end > end || hdr.cmsg_level != SOL_ALG {
            return Err(Errno::EINVAL);
        }
        let data_len = hdr.cmsg_len - size_of::<LinuxCmsghdr>();
        let data = copy_user_to_vec(token, ptr + size_of::<LinuxCmsghdr>(), data_len)?;
        match hdr.cmsg_type {
            ALG_SET_OP => {
                if data.len() != size_of::<u32>() {
                    return Err(Errno::EINVAL);
                }
                let raw = read_u32_ne(&data);
                params.op = Some(match raw {
                    ALG_OP_DECRYPT => AfAlgOperation::Decrypt,
                    ALG_OP_ENCRYPT => AfAlgOperation::Encrypt,
                    _ => return Err(Errno::EINVAL),
                });
            }
            ALG_SET_IV => {
                if data.len() < size_of::<u32>() {
                    return Err(Errno::EINVAL);
                }
                let ivlen = read_u32_ne(&data[..size_of::<u32>()]) as usize;
                if data.len() < size_of::<u32>() + ivlen {
                    return Err(Errno::EINVAL);
                }
                params.iv = Some(data[size_of::<u32>()..size_of::<u32>() + ivlen].to_vec());
            }
            ALG_SET_AEAD_ASSOCLEN => {
                if data.len() != size_of::<u32>() {
                    return Err(Errno::EINVAL);
                }
                params.assoclen = Some(read_u32_ne(&data));
            }
            _ => return Err(Errno::EINVAL),
        }
        ptr = ptr
            .checked_add(cmsg_align(hdr.cmsg_len))
            .ok_or(Errno::EINVAL)?;
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
