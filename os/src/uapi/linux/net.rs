//! Linux socket constants and 64-bit userspace ABI layouts.

pub(crate) const AF_UNSPEC: i32 = 0;
pub(crate) const AF_UNIX: i32 = 1;
pub(crate) const AF_INET: i32 = 2;
pub(crate) const AF_INET6: i32 = 10;
pub(crate) const AF_NETLINK: i32 = 16;
pub(crate) const AF_PACKET: i32 = 17;
pub(crate) const SOCK_STREAM: i32 = 1;
pub(crate) const SOCK_DGRAM: i32 = 2;
pub(crate) const SOCK_RAW: i32 = 3;
pub(crate) const SOCK_SEQPACKET: i32 = 5;
pub(crate) const SOCK_TYPE_MASK: i32 = 0xf;
pub(crate) const IPPROTO_IP: i32 = 0;
pub(crate) const IPPROTO_TCP: i32 = 6;
pub(crate) const IPPROTO_UDP: i32 = 17;
pub(crate) const IPPROTO_IPV6: i32 = 41;
pub(crate) const IP_BIND_ADDRESS_NO_PORT: i32 = 24;
pub(crate) const SOL_SOCKET: i32 = 1;
pub(crate) const SOL_PACKET: i32 = 263;
pub(crate) const SO_REUSEADDR: i32 = 2;
pub(crate) const SO_TYPE: i32 = 3;
pub(crate) const SO_ERROR: i32 = 4;
pub(crate) const SO_DONTROUTE: i32 = 5;
pub(crate) const SO_SNDBUF: i32 = 7;
pub(crate) const SO_RCVBUF: i32 = 8;
pub(crate) const SO_KEEPALIVE: i32 = 9;
pub(crate) const SO_OOBINLINE: i32 = 10;
pub(crate) const SO_NO_CHECK: i32 = 11;
pub(crate) const SO_LINGER: i32 = 13;
pub(crate) const SO_RCVTIMEO_OLD: i32 = 20;
pub(crate) const SO_SNDTIMEO_OLD: i32 = 21;
pub(crate) const SO_BINDTODEVICE: i32 = 25;
pub(crate) const SO_SNDBUFFORCE: i32 = 32;
pub(crate) const SO_RCVTIMEO_NEW: i32 = 66;
pub(crate) const SO_SNDTIMEO_NEW: i32 = 67;
pub(crate) const TCP_NODELAY: i32 = 1;
pub(crate) const TCP_MAXSEG: i32 = 2;
pub(crate) const IPV6_V6ONLY: i32 = 26;
pub(crate) const MCAST_JOIN_GROUP: i32 = 42;
pub(crate) const MCAST_LEAVE_GROUP: i32 = 45;
pub(crate) const IPT_SO_SET_REPLACE: i32 = 64;
pub(crate) const NETLINK_ROUTE: i32 = 0;
pub(crate) const NLMSG_ERROR: u16 = 2;
pub(crate) const NLMSG_DONE: u16 = 3;
pub(crate) const RTM_NEWLINK: u16 = 16;
pub(crate) const RTM_DELLINK: u16 = 17;
pub(crate) const RTM_GETLINK: u16 = 18;
pub(crate) const RTM_NEWADDR: u16 = 20;
pub(crate) const RTM_DELADDR: u16 = 21;
pub(crate) const RTM_GETADDR: u16 = 22;
pub(crate) const RTM_NEWROUTE: u16 = 24;
pub(crate) const RTM_DELROUTE: u16 = 25;
pub(crate) const RTM_GETROUTE: u16 = 26;
pub(crate) const IFA_ADDRESS: u16 = 1;
pub(crate) const IFA_LOCAL: u16 = 2;
pub(crate) const IFLA_ADDRESS: u16 = 1;
pub(crate) const IFLA_IFNAME: u16 = 3;
pub(crate) const IFLA_MTU: u16 = 4;
pub(crate) const IFLA_LINKINFO: u16 = 18;
pub(crate) const IFLA_INFO_KIND: u16 = 1;
pub(crate) const IFLA_INFO_DATA: u16 = 2;
pub(crate) const VETH_INFO_PEER: u16 = 1;
pub(crate) const NLM_F_MULTI: u16 = 0x2;
pub(crate) const IFF_UP: u32 = 0x1;
pub(crate) const IFF_LOOPBACK: u32 = 0x8;
pub(crate) const IFF_RUNNING: u32 = 0x40;
pub(crate) const ARPHRD_ETHER: u16 = 1;
pub(crate) const ARPHRD_LOOPBACK: u16 = 772;
pub(crate) const LOOPBACK_IF_INDEX: i32 = 1;
pub(crate) const IFADDRMSG_LEN: usize = 8;
pub(crate) const IFINFOMSG_LEN: usize = 16;
pub(crate) const PACKET_RX_RING: i32 = 5;
pub(crate) const PACKET_VERSION: i32 = 10;
pub(crate) const PACKET_RESERVE: i32 = 12;
pub(crate) const PACKET_VNET_HDR: i32 = 15;
pub(crate) const PACKET_FANOUT: i32 = 18;
pub(crate) const PACKET_FANOUT_ROLLOVER: i32 = 3;
pub(crate) const TPACKET_V1: i32 = 0;
pub(crate) const TPACKET_V3: i32 = 2;
pub(crate) const SHUT_RD: i32 = 0;
pub(crate) const SHUT_WR: i32 = 1;
pub(crate) const SHUT_RDWR: i32 = 2;
pub(crate) const MSG_DONTWAIT: i32 = 0x40;
pub(crate) const MSG_WAITFORONE: i32 = 0x10000;
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct LinuxSockAddrIn {
    pub(crate) family: u16,
    pub(crate) port_be: u16,
    pub(crate) addr: u32,
    pub(crate) zero: [u8; 8],
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct LinuxSockAddrIn6 {
    pub(crate) family: u16,
    pub(crate) port_be: u16,
    pub(crate) flowinfo: u32,
    pub(crate) addr: [u8; 16],
    pub(crate) scope_id: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub(crate) struct LinuxSockAddrUn {
    pub(crate) family: u16,
    pub(crate) path: [u8; 108],
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct LinuxSockAddrNl {
    pub(crate) family: u16,
    pub(crate) pad: u16,
    pub(crate) pid: u32,
    pub(crate) groups: u32,
}
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct LinuxMsghdr {
    pub(crate) msg_name: usize,
    pub(crate) msg_namelen: u32,
    pub(crate) msg_iov: usize,
    pub(crate) msg_iovlen: usize,
    pub(crate) msg_control: usize,
    pub(crate) msg_controllen: usize,
    pub(crate) msg_flags: i32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct LinuxMmsghdr {
    pub(crate) msg_hdr: LinuxMsghdr,
    pub(crate) msg_len: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct LinuxOldTimespec {
    pub(crate) tv_sec: isize,
    pub(crate) tv_nsec: isize,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct LinuxTPacketReq3 {
    pub(crate) tp_block_size: u32,
    pub(crate) tp_block_nr: u32,
    pub(crate) tp_frame_size: u32,
    pub(crate) tp_frame_nr: u32,
    pub(crate) tp_retire_blk_tov: u32,
    pub(crate) tp_sizeof_priv: u32,
    pub(crate) tp_feature_req_word: u32,
}

const _: [(); 16] = [(); core::mem::size_of::<LinuxSockAddrIn>()];
const _: [(); 28] = [(); core::mem::size_of::<LinuxSockAddrIn6>()];
const _: [(); 110] = [(); core::mem::size_of::<LinuxSockAddrUn>()];
const _: [(); 12] = [(); core::mem::size_of::<LinuxSockAddrNl>()];
const _: [(); 56] = [(); core::mem::size_of::<LinuxMsghdr>()];
const _: [(); 64] = [(); core::mem::size_of::<LinuxMmsghdr>()];
const _: [(); 16] = [(); core::mem::size_of::<LinuxOldTimespec>()];
const _: [(); 28] = [(); core::mem::size_of::<LinuxTPacketReq3>()];
