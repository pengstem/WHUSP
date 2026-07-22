use super::dirent::{DT_DIR, DT_REG, RawDirEntry, write_dir_entries};
use super::{File, FileStat, FsError, FsResult, OpenFlags, PollEvents, S_IFDIR, S_IFREG};
use crate::mm::UserBuffer;
use crate::sync::UPIntrFreeCell;
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::any::Any;

// CONTEXT: Staticfs is a small compatibility overlay for procfs/sysfs-style
// nodes that userspace cannot create on the writable EXT4 root filesystem.
// Ordinary files such as `/etc/*` and module metadata are installed from the
// generated script disk instead. Keep
// lookup_absolute(), stat_node(), and dir_entries() synchronized when adding
// or removing nodes.
const PROC_BUS_INPUT_DEVICES: &[u8] =
    b"I: Bus=0003 Vendor=0001 Product=0001 Version=0001\nN: Name=\"virtual-device-ltp\"\n";
const SYS_INPUT0_NAME: &[u8] = b"virtual-device-ltp\n";
const PROC_RANDOM_ENTROPY_AVAIL: &[u8] = b"256\n";
const SYS_DEV_BLOCK_TMPFS_UEVENT: &[u8] = b"DEVNAME=loop0\n";
const SYS_NET_LO_ADDRESS: &[u8] = b"00:00:00:00:00:00\n";
const SYS_NET_VETH1_ADDRESS: &[u8] = b"02:00:00:00:00:0a\n";
const SYS_NET_VETH2_ADDRESS: &[u8] = b"02:00:00:00:00:0b\n";
const SYS_NET_LO_MTU: &[u8] = b"65536\n";
const SYS_NET_VETH_MTU: &[u8] = b"1500\n";
const SYS_NET_LO_OPERSTATE: &[u8] = b"unknown\n";
const SYS_NET_VETH_OPERSTATE: &[u8] = b"up\n";
const SYS_NET_LO_FLAGS: &[u8] = b"0x49\n";
const SYS_NET_VETH_FLAGS: &[u8] = b"0x41\n";
const SYS_NET_LO_IFINDEX: &[u8] = b"1\n";
const SYS_NET_VETH1_IFINDEX: &[u8] = b"10\n";
const SYS_NET_VETH2_IFINDEX: &[u8] = b"11\n";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum StaticNode {
    SysDir,
    SysBlockDir,
    SysLoop0Dir,
    SysLoopInnerDir,
    SysDevDir,
    SysDevBlockDir,
    SysDevBlockTmpfsDir,
    SysClassDir,
    SysClassBlockDir,
    SysClassNetDir,
    SysClassLoop0Dir,
    SysClassLoop0BdiDir,
    SysClassNetLoDir,
    SysClassNetVeth1Dir,
    SysClassNetVeth2Dir,
    SysDevicesDir,
    SysDevicesSystemDir,
    SysCpuDir,
    SysCpuOnline,
    SysCpuPossible,
    SysCpuPresent,
    SysDevicesVirtualDir,
    SysDevicesVirtualInputDir,
    SysInput0Dir,
    ProcBusInputDevices,
    SysInput0Name,
    ProcRandomEntropyAvail,
    SysLoopSize,
    SysLoopReadOnly,
    SysLoopStat,
    SysLoopPartscan,
    SysLoopAutoclear,
    SysLoopBackingFile,
    SysLoopDirectIo,
    SysLoopSizeLimit,
    SysLoopQueueDir,
    SysLoopLogicalBlockSize,
    SysLoopDmaAlignment,
    SysLoopReadAheadKb,
    SysDevBlockTmpfsUevent,
    SysClassNetLoAddress,
    SysClassNetLoMtu,
    SysClassNetLoOperstate,
    SysClassNetLoFlags,
    SysClassNetLoIfindex,
    SysClassNetVeth1Address,
    SysClassNetVeth1Mtu,
    SysClassNetVeth1Operstate,
    SysClassNetVeth1Flags,
    SysClassNetVeth1Ifindex,
    SysClassNetVeth2Address,
    SysClassNetVeth2Mtu,
    SysClassNetVeth2Operstate,
    SysClassNetVeth2Flags,
    SysClassNetVeth2Ifindex,
}

pub struct StaticFile {
    node: StaticNode,
    path: &'static str,
    offset: UPIntrFreeCell<usize>,
    status_flags: UPIntrFreeCell<OpenFlags>,
}

impl StaticFile {
    fn new(node: StaticNode, path: &'static str, flags: OpenFlags) -> Arc<Self> {
        Arc::new(Self {
            node,
            path,
            offset: unsafe { UPIntrFreeCell::new(0) },
            status_flags: unsafe { UPIntrFreeCell::new(OpenFlags::file_status_flags(flags)) },
        })
    }
}

fn lookup_absolute(path: &str) -> Option<StaticNode> {
    match path {
        "/sys" | "/sys/" => Some(StaticNode::SysDir),
        "/sys/block" | "/sys/block/" => Some(StaticNode::SysBlockDir),
        "/sys/block/loop0" | "/sys/block/loop0/" => Some(StaticNode::SysLoop0Dir),
        "/sys/block/loop0/loop" | "/sys/block/loop0/loop/" => Some(StaticNode::SysLoopInnerDir),
        "/sys/block/loop0/queue" | "/sys/block/loop0/queue/" => Some(StaticNode::SysLoopQueueDir),
        "/sys/dev" | "/sys/dev/" => Some(StaticNode::SysDevDir),
        "/sys/dev/block" | "/sys/dev/block/" => Some(StaticNode::SysDevBlockDir),
        "/sys/dev/block/254:0" | "/sys/dev/block/254:0/" => Some(StaticNode::SysDevBlockTmpfsDir),
        "/sys/class" | "/sys/class/" => Some(StaticNode::SysClassDir),
        "/sys/class/block" | "/sys/class/block/" => Some(StaticNode::SysClassBlockDir),
        "/sys/class/net" | "/sys/class/net/" => Some(StaticNode::SysClassNetDir),
        "/sys/class/block/loop0" | "/sys/class/block/loop0/" => Some(StaticNode::SysClassLoop0Dir),
        "/sys/class/block/loop0/bdi" | "/sys/class/block/loop0/bdi/" => {
            Some(StaticNode::SysClassLoop0BdiDir)
        }
        "/sys/class/net/lo" | "/sys/class/net/lo/" => Some(StaticNode::SysClassNetLoDir),
        "/sys/class/net/ltp_ns_veth1" | "/sys/class/net/ltp_ns_veth1/" => {
            Some(StaticNode::SysClassNetVeth1Dir)
        }
        "/sys/class/net/ltp_ns_veth2" | "/sys/class/net/ltp_ns_veth2/" => {
            Some(StaticNode::SysClassNetVeth2Dir)
        }
        "/sys/devices" | "/sys/devices/" => Some(StaticNode::SysDevicesDir),
        "/sys/devices/system" | "/sys/devices/system/" => Some(StaticNode::SysDevicesSystemDir),
        "/sys/devices/system/cpu" | "/sys/devices/system/cpu/" => Some(StaticNode::SysCpuDir),
        "/sys/devices/system/cpu/online" => Some(StaticNode::SysCpuOnline),
        "/sys/devices/system/cpu/possible" => Some(StaticNode::SysCpuPossible),
        "/sys/devices/system/cpu/present" => Some(StaticNode::SysCpuPresent),
        "/sys/devices/virtual" | "/sys/devices/virtual/" => Some(StaticNode::SysDevicesVirtualDir),
        "/sys/devices/virtual/input" | "/sys/devices/virtual/input/" => {
            Some(StaticNode::SysDevicesVirtualInputDir)
        }
        "/sys/devices/virtual/input/input0" | "/sys/devices/virtual/input/input0/" => {
            Some(StaticNode::SysInput0Dir)
        }
        "/proc/bus/input/devices" => Some(StaticNode::ProcBusInputDevices),
        "/proc/sys/kernel/random/entropy_avail" => Some(StaticNode::ProcRandomEntropyAvail),
        "/sys/devices/virtual/input/input0/name" => Some(StaticNode::SysInput0Name),
        "/sys/block/loop0/size" => Some(StaticNode::SysLoopSize),
        "/sys/block/loop0/ro" => Some(StaticNode::SysLoopReadOnly),
        "/sys/block/loop0/stat" => Some(StaticNode::SysLoopStat),
        "/sys/block/loop0/loop/partscan" => Some(StaticNode::SysLoopPartscan),
        "/sys/block/loop0/loop/autoclear" => Some(StaticNode::SysLoopAutoclear),
        "/sys/block/loop0/loop/backing_file" => Some(StaticNode::SysLoopBackingFile),
        "/sys/block/loop0/loop/dio" => Some(StaticNode::SysLoopDirectIo),
        "/sys/block/loop0/loop/sizelimit" => Some(StaticNode::SysLoopSizeLimit),
        "/sys/block/loop0/queue/logical_block_size" => Some(StaticNode::SysLoopLogicalBlockSize),
        "/sys/block/loop0/queue/dma_alignment" => Some(StaticNode::SysLoopDmaAlignment),
        "/sys/class/block/loop0/bdi/read_ahead_kb" => Some(StaticNode::SysLoopReadAheadKb),
        "/sys/dev/block/254:0/uevent" => Some(StaticNode::SysDevBlockTmpfsUevent),
        "/sys/class/net/lo/address" => Some(StaticNode::SysClassNetLoAddress),
        "/sys/class/net/lo/mtu" => Some(StaticNode::SysClassNetLoMtu),
        "/sys/class/net/lo/operstate" => Some(StaticNode::SysClassNetLoOperstate),
        "/sys/class/net/lo/flags" => Some(StaticNode::SysClassNetLoFlags),
        "/sys/class/net/lo/ifindex" => Some(StaticNode::SysClassNetLoIfindex),
        "/sys/class/net/ltp_ns_veth1/address" => Some(StaticNode::SysClassNetVeth1Address),
        "/sys/class/net/ltp_ns_veth1/mtu" => Some(StaticNode::SysClassNetVeth1Mtu),
        "/sys/class/net/ltp_ns_veth1/operstate" => Some(StaticNode::SysClassNetVeth1Operstate),
        "/sys/class/net/ltp_ns_veth1/flags" => Some(StaticNode::SysClassNetVeth1Flags),
        "/sys/class/net/ltp_ns_veth1/ifindex" => Some(StaticNode::SysClassNetVeth1Ifindex),
        "/sys/class/net/ltp_ns_veth2/address" => Some(StaticNode::SysClassNetVeth2Address),
        "/sys/class/net/ltp_ns_veth2/mtu" => Some(StaticNode::SysClassNetVeth2Mtu),
        "/sys/class/net/ltp_ns_veth2/operstate" => Some(StaticNode::SysClassNetVeth2Operstate),
        "/sys/class/net/ltp_ns_veth2/flags" => Some(StaticNode::SysClassNetVeth2Flags),
        "/sys/class/net/ltp_ns_veth2/ifindex" => Some(StaticNode::SysClassNetVeth2Ifindex),
        _ => None,
    }
}

fn canonical_path(node: StaticNode) -> &'static str {
    match node {
        StaticNode::SysDir => "/sys",
        StaticNode::SysBlockDir => "/sys/block",
        StaticNode::SysLoop0Dir => "/sys/block/loop0",
        StaticNode::SysLoopInnerDir => "/sys/block/loop0/loop",
        StaticNode::SysLoopQueueDir => "/sys/block/loop0/queue",
        StaticNode::SysDevDir => "/sys/dev",
        StaticNode::SysDevBlockDir => "/sys/dev/block",
        StaticNode::SysDevBlockTmpfsDir => "/sys/dev/block/254:0",
        StaticNode::SysClassDir => "/sys/class",
        StaticNode::SysClassBlockDir => "/sys/class/block",
        StaticNode::SysClassNetDir => "/sys/class/net",
        StaticNode::SysClassLoop0Dir => "/sys/class/block/loop0",
        StaticNode::SysClassLoop0BdiDir => "/sys/class/block/loop0/bdi",
        StaticNode::SysClassNetLoDir => "/sys/class/net/lo",
        StaticNode::SysClassNetVeth1Dir => "/sys/class/net/ltp_ns_veth1",
        StaticNode::SysClassNetVeth2Dir => "/sys/class/net/ltp_ns_veth2",
        StaticNode::SysDevicesDir => "/sys/devices",
        StaticNode::SysDevicesSystemDir => "/sys/devices/system",
        StaticNode::SysCpuDir => "/sys/devices/system/cpu",
        StaticNode::SysCpuOnline => "/sys/devices/system/cpu/online",
        StaticNode::SysCpuPossible => "/sys/devices/system/cpu/possible",
        StaticNode::SysCpuPresent => "/sys/devices/system/cpu/present",
        StaticNode::SysDevicesVirtualDir => "/sys/devices/virtual",
        StaticNode::SysDevicesVirtualInputDir => "/sys/devices/virtual/input",
        StaticNode::SysInput0Dir => "/sys/devices/virtual/input/input0",
        StaticNode::ProcBusInputDevices => "/proc/bus/input/devices",
        StaticNode::SysInput0Name => "/sys/devices/virtual/input/input0/name",
        StaticNode::ProcRandomEntropyAvail => "/proc/sys/kernel/random/entropy_avail",
        StaticNode::SysLoopSize => "/sys/block/loop0/size",
        StaticNode::SysLoopReadOnly => "/sys/block/loop0/ro",
        StaticNode::SysLoopStat => "/sys/block/loop0/stat",
        StaticNode::SysLoopPartscan => "/sys/block/loop0/loop/partscan",
        StaticNode::SysLoopAutoclear => "/sys/block/loop0/loop/autoclear",
        StaticNode::SysLoopBackingFile => "/sys/block/loop0/loop/backing_file",
        StaticNode::SysLoopDirectIo => "/sys/block/loop0/loop/dio",
        StaticNode::SysLoopSizeLimit => "/sys/block/loop0/loop/sizelimit",
        StaticNode::SysLoopLogicalBlockSize => "/sys/block/loop0/queue/logical_block_size",
        StaticNode::SysLoopDmaAlignment => "/sys/block/loop0/queue/dma_alignment",
        StaticNode::SysLoopReadAheadKb => "/sys/class/block/loop0/bdi/read_ahead_kb",
        StaticNode::SysDevBlockTmpfsUevent => "/sys/dev/block/254:0/uevent",
        StaticNode::SysClassNetLoAddress => "/sys/class/net/lo/address",
        StaticNode::SysClassNetLoMtu => "/sys/class/net/lo/mtu",
        StaticNode::SysClassNetLoOperstate => "/sys/class/net/lo/operstate",
        StaticNode::SysClassNetLoFlags => "/sys/class/net/lo/flags",
        StaticNode::SysClassNetLoIfindex => "/sys/class/net/lo/ifindex",
        StaticNode::SysClassNetVeth1Address => "/sys/class/net/ltp_ns_veth1/address",
        StaticNode::SysClassNetVeth1Mtu => "/sys/class/net/ltp_ns_veth1/mtu",
        StaticNode::SysClassNetVeth1Operstate => "/sys/class/net/ltp_ns_veth1/operstate",
        StaticNode::SysClassNetVeth1Flags => "/sys/class/net/ltp_ns_veth1/flags",
        StaticNode::SysClassNetVeth1Ifindex => "/sys/class/net/ltp_ns_veth1/ifindex",
        StaticNode::SysClassNetVeth2Address => "/sys/class/net/ltp_ns_veth2/address",
        StaticNode::SysClassNetVeth2Mtu => "/sys/class/net/ltp_ns_veth2/mtu",
        StaticNode::SysClassNetVeth2Operstate => "/sys/class/net/ltp_ns_veth2/operstate",
        StaticNode::SysClassNetVeth2Flags => "/sys/class/net/ltp_ns_veth2/flags",
        StaticNode::SysClassNetVeth2Ifindex => "/sys/class/net/ltp_ns_veth2/ifindex",
    }
}

fn content(node: StaticNode) -> Option<Vec<u8>> {
    match node {
        StaticNode::SysCpuOnline => Some(cpu_list_content(crate::cpu::online_mask())),
        StaticNode::SysCpuPossible | StaticNode::SysCpuPresent => {
            Some(cpu_list_content(crate::cpu::topology().possible_mask()))
        }
        StaticNode::ProcBusInputDevices => Some(PROC_BUS_INPUT_DEVICES.to_vec()),
        StaticNode::SysInput0Name => Some(SYS_INPUT0_NAME.to_vec()),
        StaticNode::ProcRandomEntropyAvail => Some(PROC_RANDOM_ENTROPY_AVAIL.to_vec()),
        StaticNode::SysLoopSize => super::devfs::loop_device_sysfs_content("/sys/block/loop0/size"),
        StaticNode::SysLoopReadOnly => {
            super::devfs::loop_device_sysfs_content("/sys/block/loop0/ro")
        }
        StaticNode::SysLoopStat => super::devfs::loop_device_sysfs_content("/sys/block/loop0/stat"),
        StaticNode::SysLoopPartscan => {
            super::devfs::loop_device_sysfs_content("/sys/block/loop0/loop/partscan")
        }
        StaticNode::SysLoopAutoclear => {
            super::devfs::loop_device_sysfs_content("/sys/block/loop0/loop/autoclear")
        }
        StaticNode::SysLoopBackingFile => {
            super::devfs::loop_device_sysfs_content("/sys/block/loop0/loop/backing_file")
        }
        StaticNode::SysLoopDirectIo => {
            super::devfs::loop_device_sysfs_content("/sys/block/loop0/loop/dio")
        }
        StaticNode::SysLoopSizeLimit => {
            super::devfs::loop_device_sysfs_content("/sys/block/loop0/loop/sizelimit")
        }
        StaticNode::SysLoopLogicalBlockSize => {
            super::devfs::loop_device_sysfs_content("/sys/block/loop0/queue/logical_block_size")
        }
        StaticNode::SysLoopDmaAlignment => {
            super::devfs::loop_device_sysfs_content("/sys/block/loop0/queue/dma_alignment")
        }
        StaticNode::SysLoopReadAheadKb => {
            super::devfs::loop_device_sysfs_content("/sys/class/block/loop0/bdi/read_ahead_kb")
        }
        StaticNode::SysDevBlockTmpfsUevent => Some(SYS_DEV_BLOCK_TMPFS_UEVENT.to_vec()),
        StaticNode::SysClassNetLoAddress => Some(SYS_NET_LO_ADDRESS.to_vec()),
        StaticNode::SysClassNetLoMtu => Some(SYS_NET_LO_MTU.to_vec()),
        StaticNode::SysClassNetLoOperstate => Some(SYS_NET_LO_OPERSTATE.to_vec()),
        StaticNode::SysClassNetLoFlags => Some(SYS_NET_LO_FLAGS.to_vec()),
        StaticNode::SysClassNetLoIfindex => Some(SYS_NET_LO_IFINDEX.to_vec()),
        StaticNode::SysClassNetVeth1Address => Some(SYS_NET_VETH1_ADDRESS.to_vec()),
        StaticNode::SysClassNetVeth1Mtu => Some(SYS_NET_VETH_MTU.to_vec()),
        StaticNode::SysClassNetVeth1Operstate => Some(SYS_NET_VETH_OPERSTATE.to_vec()),
        StaticNode::SysClassNetVeth1Flags => Some(SYS_NET_VETH_FLAGS.to_vec()),
        StaticNode::SysClassNetVeth1Ifindex => Some(SYS_NET_VETH1_IFINDEX.to_vec()),
        StaticNode::SysClassNetVeth2Address => Some(SYS_NET_VETH2_ADDRESS.to_vec()),
        StaticNode::SysClassNetVeth2Mtu => Some(SYS_NET_VETH_MTU.to_vec()),
        StaticNode::SysClassNetVeth2Operstate => Some(SYS_NET_VETH_OPERSTATE.to_vec()),
        StaticNode::SysClassNetVeth2Flags => Some(SYS_NET_VETH_FLAGS.to_vec()),
        StaticNode::SysClassNetVeth2Ifindex => Some(SYS_NET_VETH2_IFINDEX.to_vec()),
        StaticNode::SysDir
        | StaticNode::SysBlockDir
        | StaticNode::SysLoop0Dir
        | StaticNode::SysLoopInnerDir
        | StaticNode::SysLoopQueueDir
        | StaticNode::SysDevDir
        | StaticNode::SysDevBlockDir
        | StaticNode::SysDevBlockTmpfsDir
        | StaticNode::SysClassDir
        | StaticNode::SysClassBlockDir
        | StaticNode::SysClassNetDir
        | StaticNode::SysClassLoop0Dir
        | StaticNode::SysClassLoop0BdiDir
        | StaticNode::SysClassNetLoDir
        | StaticNode::SysClassNetVeth1Dir
        | StaticNode::SysClassNetVeth2Dir
        | StaticNode::SysDevicesDir
        | StaticNode::SysDevicesSystemDir
        | StaticNode::SysCpuDir
        | StaticNode::SysDevicesVirtualDir
        | StaticNode::SysDevicesVirtualInputDir
        | StaticNode::SysInput0Dir => None,
    }
}

fn cpu_list_content(mask: crate::cpu::CpuMask) -> Vec<u8> {
    let mut list = String::new();
    let mut cpu = 0;
    let mut first_range = true;
    while cpu < crate::config::MAX_CPUS {
        if !mask.contains(cpu) {
            cpu += 1;
            continue;
        }
        let start = cpu;
        while cpu + 1 < crate::config::MAX_CPUS && mask.contains(cpu + 1) {
            cpu += 1;
        }
        if !first_range {
            list.push(',');
        }
        if start == cpu {
            list.push_str(&format!("{start}"));
        } else {
            list.push_str(&format!("{start}-{cpu}"));
        }
        first_range = false;
        cpu += 1;
    }
    list.push('\n');
    list.into_bytes()
}

fn is_dir(node: StaticNode) -> bool {
    match node {
        StaticNode::SysDir
        | StaticNode::SysBlockDir
        | StaticNode::SysLoop0Dir
        | StaticNode::SysLoopInnerDir
        | StaticNode::SysLoopQueueDir
        | StaticNode::SysDevDir
        | StaticNode::SysDevBlockDir
        | StaticNode::SysDevBlockTmpfsDir
        | StaticNode::SysClassDir
        | StaticNode::SysClassBlockDir
        | StaticNode::SysClassNetDir
        | StaticNode::SysClassLoop0Dir
        | StaticNode::SysClassLoop0BdiDir
        | StaticNode::SysClassNetLoDir
        | StaticNode::SysClassNetVeth1Dir
        | StaticNode::SysClassNetVeth2Dir
        | StaticNode::SysDevicesDir
        | StaticNode::SysDevicesSystemDir
        | StaticNode::SysCpuDir
        | StaticNode::SysDevicesVirtualDir
        | StaticNode::SysDevicesVirtualInputDir
        | StaticNode::SysInput0Dir => true,
        _ => false,
    }
}

fn stat_node(node: StaticNode) -> FileStat {
    let mut stat = if is_dir(node) {
        FileStat::with_mode(S_IFDIR | 0o555)
    } else {
        FileStat::with_mode(S_IFREG | 0o444)
    };
    stat.dev = 0x657463;
    stat.ino = match node {
        StaticNode::SysDir => 27,
        StaticNode::SysBlockDir => 28,
        StaticNode::SysLoop0Dir => 29,
        StaticNode::SysLoopInnerDir => 30,
        StaticNode::SysLoopQueueDir => 39,
        StaticNode::SysDevDir => 31,
        StaticNode::SysDevBlockDir => 32,
        StaticNode::SysDevicesDir => 33,
        StaticNode::SysDevicesSystemDir => 76,
        StaticNode::SysCpuDir => 77,
        StaticNode::SysCpuOnline => 78,
        StaticNode::SysCpuPossible => 79,
        StaticNode::SysCpuPresent => 80,
        StaticNode::SysDevicesVirtualDir => 34,
        StaticNode::SysDevicesVirtualInputDir => 35,
        StaticNode::SysInput0Dir => 36,
        StaticNode::SysDevBlockTmpfsDir => 37,
        StaticNode::ProcBusInputDevices => 12,
        StaticNode::SysInput0Name => 13,
        StaticNode::ProcRandomEntropyAvail => 14,
        StaticNode::SysLoopSize => 17,
        StaticNode::SysLoopReadOnly => 18,
        StaticNode::SysLoopStat => 48,
        StaticNode::SysLoopPartscan => 19,
        StaticNode::SysLoopAutoclear => 20,
        StaticNode::SysLoopBackingFile => 21,
        StaticNode::SysLoopDirectIo => 22,
        StaticNode::SysLoopSizeLimit => 23,
        StaticNode::SysLoopLogicalBlockSize => 40,
        StaticNode::SysLoopDmaAlignment => 41,
        StaticNode::SysClassDir => 43,
        StaticNode::SysClassBlockDir => 44,
        StaticNode::SysClassLoop0Dir => 45,
        StaticNode::SysClassLoop0BdiDir => 46,
        StaticNode::SysLoopReadAheadKb => 47,
        StaticNode::SysDevBlockTmpfsUevent => 38,
        StaticNode::SysClassNetDir => 57,
        StaticNode::SysClassNetLoDir => 58,
        StaticNode::SysClassNetVeth1Dir => 59,
        StaticNode::SysClassNetVeth2Dir => 60,
        StaticNode::SysClassNetLoAddress => 61,
        StaticNode::SysClassNetLoMtu => 62,
        StaticNode::SysClassNetLoOperstate => 63,
        StaticNode::SysClassNetLoFlags => 64,
        StaticNode::SysClassNetLoIfindex => 65,
        StaticNode::SysClassNetVeth1Address => 66,
        StaticNode::SysClassNetVeth1Mtu => 67,
        StaticNode::SysClassNetVeth1Operstate => 68,
        StaticNode::SysClassNetVeth1Flags => 69,
        StaticNode::SysClassNetVeth1Ifindex => 70,
        StaticNode::SysClassNetVeth2Address => 71,
        StaticNode::SysClassNetVeth2Mtu => 72,
        StaticNode::SysClassNetVeth2Operstate => 73,
        StaticNode::SysClassNetVeth2Flags => 74,
        StaticNode::SysClassNetVeth2Ifindex => 75,
    };
    stat.nlink = if is_dir(node) { 2 } else { 1 };
    stat.size = content(node).map_or(0, |content| content.len() as u64);
    let now = super::FileTimestamp::now();
    stat.atime_sec = now.sec;
    stat.atime_nsec = now.nsec;
    stat.mtime_sec = now.sec;
    stat.mtime_nsec = now.nsec;
    stat.ctime_sec = now.sec;
    stat.ctime_nsec = now.nsec;
    stat
}

pub(crate) fn stat_path(path: &str) -> Option<FileStat> {
    lookup_absolute(path).map(stat_node)
}

pub(crate) fn open_path(
    path: &str,
    flags: OpenFlags,
) -> FsResult<Option<Arc<dyn File + Send + Sync>>> {
    let Some(node) = lookup_absolute(path) else {
        return Ok(None);
    };
    if is_dir(node) {
        if flags.can_open_directory() {
            return Ok(Some(StaticFile::new(node, canonical_path(node), flags)));
        }
        return Err(FsError::IsDir);
    }
    if (flags.writable_target() || flags.contains(OpenFlags::TRUNC))
        && !matches!(node, StaticNode::SysLoopReadAheadKb)
    {
        return Err(FsError::PermissionDenied);
    }
    // loop read_ahead_kb is a writable staticfs knob because LTP setup scripts
    // treat it like sysfs. Ordinary mutable files live on the EXT4 root instead.
    Ok(Some(StaticFile::new(node, canonical_path(node), flags)))
}

fn dir_entry(node: StaticNode, name: &str, dtype: u8) -> RawDirEntry {
    RawDirEntry {
        ino: stat_node(node).ino as u32,
        name: String::from(name),
        dtype,
    }
}

fn dir_entries(node: StaticNode) -> Option<Vec<RawDirEntry>> {
    let mut entries = Vec::new();
    match node {
        StaticNode::SysDir => {
            entries.push(dir_entry(StaticNode::SysDir, ".", DT_DIR));
            entries.push(dir_entry(StaticNode::SysDir, "..", DT_DIR));
            entries.push(dir_entry(StaticNode::SysBlockDir, "block", DT_DIR));
            entries.push(dir_entry(StaticNode::SysDevDir, "dev", DT_DIR));
            entries.push(dir_entry(StaticNode::SysClassDir, "class", DT_DIR));
            entries.push(dir_entry(StaticNode::SysDevicesDir, "devices", DT_DIR));
        }
        StaticNode::SysClassDir => {
            entries.push(dir_entry(StaticNode::SysClassDir, ".", DT_DIR));
            entries.push(dir_entry(StaticNode::SysDir, "..", DT_DIR));
            entries.push(dir_entry(StaticNode::SysClassBlockDir, "block", DT_DIR));
            entries.push(dir_entry(StaticNode::SysClassNetDir, "net", DT_DIR));
        }
        StaticNode::SysClassNetDir => {
            entries.push(dir_entry(StaticNode::SysClassNetDir, ".", DT_DIR));
            entries.push(dir_entry(StaticNode::SysClassDir, "..", DT_DIR));
            entries.push(dir_entry(StaticNode::SysClassNetLoDir, "lo", DT_DIR));
            entries.push(dir_entry(
                StaticNode::SysClassNetVeth1Dir,
                "ltp_ns_veth1",
                DT_DIR,
            ));
            entries.push(dir_entry(
                StaticNode::SysClassNetVeth2Dir,
                "ltp_ns_veth2",
                DT_DIR,
            ));
        }
        StaticNode::SysClassNetLoDir => {
            entries.push(dir_entry(StaticNode::SysClassNetLoDir, ".", DT_DIR));
            entries.push(dir_entry(StaticNode::SysClassNetDir, "..", DT_DIR));
            entries.push(dir_entry(
                StaticNode::SysClassNetLoAddress,
                "address",
                DT_REG,
            ));
            entries.push(dir_entry(StaticNode::SysClassNetLoMtu, "mtu", DT_REG));
            entries.push(dir_entry(
                StaticNode::SysClassNetLoOperstate,
                "operstate",
                DT_REG,
            ));
            entries.push(dir_entry(StaticNode::SysClassNetLoFlags, "flags", DT_REG));
            entries.push(dir_entry(
                StaticNode::SysClassNetLoIfindex,
                "ifindex",
                DT_REG,
            ));
        }
        StaticNode::SysClassNetVeth1Dir => {
            entries.push(dir_entry(StaticNode::SysClassNetVeth1Dir, ".", DT_DIR));
            entries.push(dir_entry(StaticNode::SysClassNetDir, "..", DT_DIR));
            entries.push(dir_entry(
                StaticNode::SysClassNetVeth1Address,
                "address",
                DT_REG,
            ));
            entries.push(dir_entry(StaticNode::SysClassNetVeth1Mtu, "mtu", DT_REG));
            entries.push(dir_entry(
                StaticNode::SysClassNetVeth1Operstate,
                "operstate",
                DT_REG,
            ));
            entries.push(dir_entry(
                StaticNode::SysClassNetVeth1Flags,
                "flags",
                DT_REG,
            ));
            entries.push(dir_entry(
                StaticNode::SysClassNetVeth1Ifindex,
                "ifindex",
                DT_REG,
            ));
        }
        StaticNode::SysClassNetVeth2Dir => {
            entries.push(dir_entry(StaticNode::SysClassNetVeth2Dir, ".", DT_DIR));
            entries.push(dir_entry(StaticNode::SysClassNetDir, "..", DT_DIR));
            entries.push(dir_entry(
                StaticNode::SysClassNetVeth2Address,
                "address",
                DT_REG,
            ));
            entries.push(dir_entry(StaticNode::SysClassNetVeth2Mtu, "mtu", DT_REG));
            entries.push(dir_entry(
                StaticNode::SysClassNetVeth2Operstate,
                "operstate",
                DT_REG,
            ));
            entries.push(dir_entry(
                StaticNode::SysClassNetVeth2Flags,
                "flags",
                DT_REG,
            ));
            entries.push(dir_entry(
                StaticNode::SysClassNetVeth2Ifindex,
                "ifindex",
                DT_REG,
            ));
        }
        StaticNode::SysDevicesDir => {
            entries.push(dir_entry(StaticNode::SysDevicesDir, ".", DT_DIR));
            entries.push(dir_entry(StaticNode::SysDir, "..", DT_DIR));
            entries.push(dir_entry(StaticNode::SysDevicesSystemDir, "system", DT_DIR));
            entries.push(dir_entry(
                StaticNode::SysDevicesVirtualDir,
                "virtual",
                DT_DIR,
            ));
        }
        StaticNode::SysDevicesSystemDir => {
            entries.push(dir_entry(StaticNode::SysDevicesSystemDir, ".", DT_DIR));
            entries.push(dir_entry(StaticNode::SysDevicesDir, "..", DT_DIR));
            entries.push(dir_entry(StaticNode::SysCpuDir, "cpu", DT_DIR));
        }
        StaticNode::SysCpuDir => {
            entries.push(dir_entry(StaticNode::SysCpuDir, ".", DT_DIR));
            entries.push(dir_entry(StaticNode::SysDevicesSystemDir, "..", DT_DIR));
            entries.push(dir_entry(StaticNode::SysCpuOnline, "online", DT_REG));
            entries.push(dir_entry(StaticNode::SysCpuPossible, "possible", DT_REG));
            entries.push(dir_entry(StaticNode::SysCpuPresent, "present", DT_REG));
        }
        _ => return None,
    }
    Some(entries)
}

impl File for StaticFile {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn readable(&self) -> bool {
        content(self.node).is_some()
    }

    fn writable(&self) -> bool {
        matches!(self.node, StaticNode::SysLoopReadAheadKb)
    }

    fn read(&self, mut user_buf: UserBuffer) -> usize {
        let Some(content) = content(self.node) else {
            return 0;
        };
        let mut offset = self.offset.exclusive_access();
        let start = (*offset).min(content.len());
        let copied = user_buf.copy_from_slice(&content[start..]);
        *offset = start + copied;
        copied
    }

    fn write(&self, user_buf: UserBuffer) -> usize {
        if self.node != StaticNode::SysLoopReadAheadKb {
            return 0;
        }
        let data = user_buf.to_vec();
        let Ok(text) = core::str::from_utf8(&data) else {
            return 0;
        };
        let Ok(read_ahead_kb) = text.trim().parse::<usize>() else {
            return 0;
        };
        if super::devfs::loop_device_set_read_ahead(0, read_ahead_kb).is_err() {
            return 0;
        }
        data.len()
    }

    fn poll(&self, events: PollEvents) -> PollEvents {
        let mut ready = PollEvents::empty();
        if events.contains(PollEvents::POLLIN) && self.readable() {
            ready |= PollEvents::POLLIN;
        }
        ready
    }

    fn stat(&self) -> FsResult<FileStat> {
        Ok(stat_node(self.node))
    }

    fn read_at(&self, offset: usize, buf: &mut [u8]) -> usize {
        let Some(content) = content(self.node) else {
            return 0;
        };
        let start = offset.min(content.len());
        let len = buf.len().min(content.len() - start);
        buf[..len].copy_from_slice(&content[start..start + len]);
        len
    }

    fn seek(&self, offset: i64, whence: super::SeekWhence) -> FsResult<usize> {
        let len = content(self.node).map_or(0, |content| content.len());
        let base = match whence {
            super::SeekWhence::Set => 0,
            super::SeekWhence::Current => *self.offset.exclusive_access() as i64,
            super::SeekWhence::End => len as i64,
            super::SeekWhence::Data => {
                if offset < 0 {
                    return Err(FsError::InvalidInput);
                }
                let offset = offset as usize;
                if offset >= len {
                    return Err(FsError::NoDeviceOrAddress);
                }
                *self.offset.exclusive_access() = offset;
                return Ok(offset);
            }
            super::SeekWhence::Hole => {
                if offset < 0 {
                    return Err(FsError::InvalidInput);
                }
                let offset = offset as usize;
                if offset > len {
                    return Err(FsError::NoDeviceOrAddress);
                }
                *self.offset.exclusive_access() = len;
                return Ok(len);
            }
        };
        let next = base.checked_add(offset).ok_or(FsError::InvalidInput)?;
        if next < 0 {
            return Err(FsError::InvalidInput);
        }
        *self.offset.exclusive_access() = next as usize;
        Ok(next as usize)
    }

    fn status_flags(&self) -> OpenFlags {
        *self.status_flags.exclusive_access()
    }

    fn set_status_flags(&self, flags: OpenFlags) {
        *self.status_flags.exclusive_access() = flags;
    }

    fn read_dirent64(&self, mut user_buf: UserBuffer) -> FsResult<isize> {
        let Some(entries) = dir_entries(self.node) else {
            return Err(FsError::NotDir);
        };
        let mut kernel_buf = vec![0u8; user_buf.len()];
        let mut offset = self.offset.exclusive_access();
        let (written, next_offset) = write_dir_entries(&entries, *offset as u64, &mut kernel_buf)?;
        *offset = next_offset as usize;
        if written == 0 {
            return Ok(0);
        }
        assert_eq!(user_buf.copy_from_slice(&kernel_buf[..written]), written);
        Ok(written as isize)
    }

    fn working_dir(&self) -> Option<super::path::WorkingDir> {
        if !is_dir(self.node) {
            return None;
        }
        // CONTEXT: Static compatibility directories are not backed by a VFS
        // mount, but openat() only needs a directory anchor to preserve the
        // normalized static path kept in the fd table.
        Some(
            crate::task::current_process()
                .path_snapshot()
                .context
                .root(),
        )
    }

    fn proc_fd_target(&self) -> Option<String> {
        Some(String::from(self.path))
    }
}
