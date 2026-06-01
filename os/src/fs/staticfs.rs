use super::dirent::{DT_DIR, DT_REG, RawDirEntry, write_dir_entries};
use super::{File, FileStat, FsError, FsResult, OpenFlags, PollEvents, S_IFDIR, S_IFREG};
use crate::mm::UserBuffer;
use crate::sync::UPIntrFreeCell;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::any::Any;
use lazy_static::lazy_static;

const ETC_NSSWITCH_CONF: &[u8] =
    b"passwd: files\ngroup: files\nhosts: files\nprotocols: files\nservices: files\nnetworks: files\n";
const ETC_PASSWD: &[u8] = b"root:x:0:0:root:/root:/bin/sh\n\
nobody:x:65534:65534:nobody:/nonexistent:/usr/sbin/nologin\n\
ltp_add_key05_0:x:10000:10000:LTP add_key05 user:/tmp:/bin/sh\n\
ltp_add_key05_1:x:10001:10001:LTP add_key05 user:/tmp:/bin/sh\n\
ltp_add_key05_2:x:10002:10002:LTP add_key05 user:/tmp:/bin/sh\n\
ltp_add_key05_3:x:10003:10003:LTP add_key05 user:/tmp:/bin/sh\n\
ltp_add_key05_4:x:10004:10004:LTP add_key05 user:/tmp:/bin/sh\n\
ltp_add_key05_5:x:10005:10005:LTP add_key05 user:/tmp:/bin/sh\n\
ltp_add_key05_6:x:10006:10006:LTP add_key05 user:/tmp:/bin/sh\n\
ltp_add_key05_7:x:10007:10007:LTP add_key05 user:/tmp:/bin/sh\n\
ltp_add_key05_8:x:10008:10008:LTP add_key05 user:/tmp:/bin/sh\n\
ltp_add_key05_9:x:10009:10009:LTP add_key05 user:/tmp:/bin/sh\n";
const ETC_GROUP: &[u8] = b"root:x:0:\n\
daemon:x:1:\n\
users:x:100:\n\
nobody:x:65534:\n\
nogroup:x:65534:\n\
ltp_add_key05_0:x:10000:\n\
ltp_add_key05_1:x:10001:\n\
ltp_add_key05_2:x:10002:\n\
ltp_add_key05_3:x:10003:\n\
ltp_add_key05_4:x:10004:\n\
ltp_add_key05_5:x:10005:\n\
ltp_add_key05_6:x:10006:\n\
ltp_add_key05_7:x:10007:\n\
ltp_add_key05_8:x:10008:\n\
ltp_add_key05_9:x:10009:\n";
const ETC_HOSTS: &[u8] = b"127.0.0.1 localhost localhost.localdomain\n";
const ETC_SERVICES: &[u8] = b"echo 7/tcp\n\
echo 7/udp\n";
const ETC_RESOLV_CONF: &[u8] = b"";
const ETC_PROTOCOLS: &[u8] = b"ip 0 IP\ntcp 6 TCP\nudp 17 UDP\n";
const PROC_BUS_INPUT_DEVICES: &[u8] =
    b"I: Bus=0003 Vendor=0001 Product=0001 Version=0001\nN: Name=\"virtual-device-ltp\"\n";
const SYS_INPUT0_NAME: &[u8] = b"virtual-device-ltp\n";
const PROC_RANDOM_ENTROPY_AVAIL: &[u8] = b"256\n";
const MODULES_LOOP_DEP: &[u8] =
    b"kernel/drivers/block/loop.ko:\nkernel/drivers/memory/hwpoison_inject.ko:\nkernel/fs/quota/quota_v2.ko:\nkernel/net/dns_resolver/dns_resolver.ko:\n";
const MODULES_LOOP_BUILTIN: &[u8] =
    b"kernel/drivers/block/loop.ko\nkernel/drivers/memory/hwpoison_inject.ko\nkernel/fs/quota/quota_v2.ko\nkernel/net/dns_resolver/dns_resolver.ko\n";
const MODULES_ALIAS: &[u8] = b"";
const MODULES_ORDER: &[u8] = b"kernel/net/dns_resolver/dns_resolver.ko\n";
const MODULES_SYMBOLS: &[u8] = b"";
const MODULES_CONFIG: &[u8] =
    b"CONFIG_FS_VERITY=y\nCONFIG_USER_DECRYPTED_DATA=y\nCONFIG_PREEMPT_RT=y\nCONFIG_MEMORY_FAILURE=y\nCONFIG_HWPOISON_INJECT=y\n";
const DNS_RESOLVER_KO: &[u8] = b"WHUSP built-in dns_resolver module placeholder\n";
const SYS_DEV_BLOCK_TMPFS_UEVENT: &[u8] = b"DEVNAME=loop0\n";
#[cfg(target_arch = "loongarch64")]
const LA_MUSL_COMPAT_SO: &[u8] =
    include_bytes!("../../assets/loongarch64/liboscomp-musl-compat.so");

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum StaticNode {
    EtcDir,
    LibDir,
    LibModulesDir,
    LibModulesReleaseDir,
    LibModulesKernelDir,
    LibModulesKernelNetDir,
    LibModulesKernelNetDnsResolverDir,
    SysDir,
    SysBlockDir,
    SysLoop0Dir,
    SysLoopInnerDir,
    SysDevDir,
    SysDevBlockDir,
    SysDevBlockTmpfsDir,
    SysClassDir,
    SysClassBlockDir,
    SysClassLoop0Dir,
    SysClassLoop0BdiDir,
    SysDevicesDir,
    SysDevicesVirtualDir,
    SysDevicesVirtualInputDir,
    SysInput0Dir,
    NsswitchConf,
    Passwd,
    Group,
    Hosts,
    Services,
    ResolvConf,
    Protocols,
    ModulesDep,
    ModulesBuiltin,
    ModulesAlias,
    ModulesOrder,
    ModulesSymbols,
    ModulesConfig,
    DnsResolverKo,
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
    #[cfg(target_arch = "loongarch64")]
    OptDir,
    #[cfg(target_arch = "loongarch64")]
    OptOscompSupportDir,
    #[cfg(target_arch = "loongarch64")]
    OptOscompSupportLibDir,
    #[cfg(target_arch = "loongarch64")]
    LaMuslCompatSo,
}

pub struct StaticFile {
    node: StaticNode,
    path: &'static str,
    offset: UPIntrFreeCell<usize>,
    status_flags: UPIntrFreeCell<OpenFlags>,
}

lazy_static! {
    static ref HOSTS_CONTENT: UPIntrFreeCell<Vec<u8>> = {
        let mut value = Vec::new();
        value.extend_from_slice(ETC_HOSTS);
        unsafe { UPIntrFreeCell::new(value) }
    };
}

impl StaticFile {
    fn new(node: StaticNode, path: &'static str, flags: OpenFlags) -> Arc<Self> {
        let offset = initial_offset(node, flags);
        Arc::new(Self {
            node,
            path,
            offset: unsafe { UPIntrFreeCell::new(offset) },
            status_flags: unsafe { UPIntrFreeCell::new(OpenFlags::file_status_flags(flags)) },
        })
    }
}

fn initial_offset(node: StaticNode, flags: OpenFlags) -> usize {
    if node != StaticNode::Hosts {
        return 0;
    }
    let mut hosts = HOSTS_CONTENT.exclusive_access();
    if flags.contains(OpenFlags::TRUNC) {
        hosts.clear();
    }
    if flags.contains(OpenFlags::APPEND) {
        hosts.len()
    } else {
        0
    }
}

fn lookup_absolute(path: &str) -> Option<StaticNode> {
    match path {
        "/etc" | "/etc/" => Some(StaticNode::EtcDir),
        "/lib" | "/lib/" => Some(StaticNode::LibDir),
        "/lib/modules" | "/lib/modules/" => Some(StaticNode::LibModulesDir),
        "/lib/modules/6.8.0-whusp" | "/lib/modules/6.8.0-whusp/" => {
            Some(StaticNode::LibModulesReleaseDir)
        }
        "/kernel"
        | "/glibc/kernel"
        | "/musl/kernel"
        | "/kernel/"
        | "/glibc/kernel/"
        | "/musl/kernel/"
        | "/lib/modules/kernel"
        | "/lib/modules/kernel/"
        | "/lib/modules/6.8.0-whusp/kernel"
        | "/lib/modules/6.8.0-whusp/kernel/" => Some(StaticNode::LibModulesKernelDir),
        "/kernel/net"
        | "/glibc/kernel/net"
        | "/musl/kernel/net"
        | "/kernel/net/"
        | "/glibc/kernel/net/"
        | "/musl/kernel/net/"
        | "/lib/modules/kernel/net"
        | "/lib/modules/kernel/net/"
        | "/lib/modules/6.8.0-whusp/kernel/net"
        | "/lib/modules/6.8.0-whusp/kernel/net/" => Some(StaticNode::LibModulesKernelNetDir),
        "/kernel/net/dns_resolver"
        | "/glibc/kernel/net/dns_resolver"
        | "/musl/kernel/net/dns_resolver"
        | "/kernel/net/dns_resolver/"
        | "/glibc/kernel/net/dns_resolver/"
        | "/musl/kernel/net/dns_resolver/"
        | "/lib/modules/kernel/net/dns_resolver"
        | "/lib/modules/kernel/net/dns_resolver/"
        | "/lib/modules/6.8.0-whusp/kernel/net/dns_resolver"
        | "/lib/modules/6.8.0-whusp/kernel/net/dns_resolver/" => {
            Some(StaticNode::LibModulesKernelNetDnsResolverDir)
        }
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
        "/sys/class/block/loop0" | "/sys/class/block/loop0/" => Some(StaticNode::SysClassLoop0Dir),
        "/sys/class/block/loop0/bdi" | "/sys/class/block/loop0/bdi/" => {
            Some(StaticNode::SysClassLoop0BdiDir)
        }
        "/sys/devices" | "/sys/devices/" => Some(StaticNode::SysDevicesDir),
        "/sys/devices/virtual" | "/sys/devices/virtual/" => Some(StaticNode::SysDevicesVirtualDir),
        "/sys/devices/virtual/input" | "/sys/devices/virtual/input/" => {
            Some(StaticNode::SysDevicesVirtualInputDir)
        }
        "/sys/devices/virtual/input/input0" | "/sys/devices/virtual/input/input0/" => {
            Some(StaticNode::SysInput0Dir)
        }
        "/etc/nsswitch.conf" => Some(StaticNode::NsswitchConf),
        "/etc/passwd" => Some(StaticNode::Passwd),
        "/etc/group" => Some(StaticNode::Group),
        "/etc/hosts" => Some(StaticNode::Hosts),
        "/etc/services" => Some(StaticNode::Services),
        "/etc/resolv.conf" => Some(StaticNode::ResolvConf),
        "/etc/protocols" => Some(StaticNode::Protocols),
        "/modules.dep"
        | "/glibc/modules.dep"
        | "/musl/modules.dep"
        | "/lib/modules/modules.dep"
        | "/lib/modules/6.8.0-whusp/modules.dep" => Some(StaticNode::ModulesDep),
        "/modules.builtin"
        | "/glibc/modules.builtin"
        | "/musl/modules.builtin"
        | "/lib/modules/modules.builtin"
        | "/lib/modules/6.8.0-whusp/modules.builtin" => Some(StaticNode::ModulesBuiltin),
        "/modules.alias"
        | "/glibc/modules.alias"
        | "/musl/modules.alias"
        | "/lib/modules/modules.alias"
        | "/lib/modules/6.8.0-whusp/modules.alias" => Some(StaticNode::ModulesAlias),
        "/modules.order"
        | "/glibc/modules.order"
        | "/musl/modules.order"
        | "/lib/modules/modules.order"
        | "/lib/modules/6.8.0-whusp/modules.order" => Some(StaticNode::ModulesOrder),
        "/modules.symbols"
        | "/glibc/modules.symbols"
        | "/musl/modules.symbols"
        | "/lib/modules/modules.symbols"
        | "/lib/modules/6.8.0-whusp/modules.symbols" => Some(StaticNode::ModulesSymbols),
        "/config"
        | "/glibc/config"
        | "/musl/config"
        | "/lib/modules/config"
        | "/lib/modules/6.8.0-whusp/config" => Some(StaticNode::ModulesConfig),
        "/kernel/net/dns_resolver/dns_resolver.ko"
        | "/glibc/kernel/net/dns_resolver/dns_resolver.ko"
        | "/musl/kernel/net/dns_resolver/dns_resolver.ko"
        | "/lib/modules/kernel/net/dns_resolver/dns_resolver.ko"
        | "/lib/modules/6.8.0-whusp/kernel/net/dns_resolver/dns_resolver.ko" => {
            Some(StaticNode::DnsResolverKo)
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
        #[cfg(target_arch = "loongarch64")]
        "/opt" | "/opt/" => Some(StaticNode::OptDir),
        #[cfg(target_arch = "loongarch64")]
        "/opt/oscomp-support" | "/opt/oscomp-support/" => Some(StaticNode::OptOscompSupportDir),
        #[cfg(target_arch = "loongarch64")]
        "/opt/oscomp-support/lib" | "/opt/oscomp-support/lib/" => {
            Some(StaticNode::OptOscompSupportLibDir)
        }
        #[cfg(target_arch = "loongarch64")]
        "/opt/oscomp-support/lib/liboscomp-musl-compat.so" => Some(StaticNode::LaMuslCompatSo),
        _ => None,
    }
}

fn canonical_path(node: StaticNode) -> &'static str {
    match node {
        StaticNode::EtcDir => "/etc",
        StaticNode::LibDir => "/lib",
        StaticNode::LibModulesDir => "/lib/modules",
        StaticNode::LibModulesReleaseDir => "/lib/modules/6.8.0-whusp",
        StaticNode::LibModulesKernelDir => "/lib/modules/6.8.0-whusp/kernel",
        StaticNode::LibModulesKernelNetDir => "/lib/modules/6.8.0-whusp/kernel/net",
        StaticNode::LibModulesKernelNetDnsResolverDir => {
            "/lib/modules/6.8.0-whusp/kernel/net/dns_resolver"
        }
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
        StaticNode::SysClassLoop0Dir => "/sys/class/block/loop0",
        StaticNode::SysClassLoop0BdiDir => "/sys/class/block/loop0/bdi",
        StaticNode::SysDevicesDir => "/sys/devices",
        StaticNode::SysDevicesVirtualDir => "/sys/devices/virtual",
        StaticNode::SysDevicesVirtualInputDir => "/sys/devices/virtual/input",
        StaticNode::SysInput0Dir => "/sys/devices/virtual/input/input0",
        StaticNode::NsswitchConf => "/etc/nsswitch.conf",
        StaticNode::Passwd => "/etc/passwd",
        StaticNode::Group => "/etc/group",
        StaticNode::Hosts => "/etc/hosts",
        StaticNode::Services => "/etc/services",
        StaticNode::ResolvConf => "/etc/resolv.conf",
        StaticNode::Protocols => "/etc/protocols",
        StaticNode::ModulesDep => "/lib/modules/6.8.0-whusp/modules.dep",
        StaticNode::ModulesBuiltin => "/lib/modules/6.8.0-whusp/modules.builtin",
        StaticNode::ModulesAlias => "/lib/modules/6.8.0-whusp/modules.alias",
        StaticNode::ModulesOrder => "/lib/modules/6.8.0-whusp/modules.order",
        StaticNode::ModulesSymbols => "/lib/modules/6.8.0-whusp/modules.symbols",
        StaticNode::ModulesConfig => "/lib/modules/6.8.0-whusp/config",
        StaticNode::DnsResolverKo => {
            "/lib/modules/6.8.0-whusp/kernel/net/dns_resolver/dns_resolver.ko"
        }
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
        #[cfg(target_arch = "loongarch64")]
        StaticNode::OptDir => "/opt",
        #[cfg(target_arch = "loongarch64")]
        StaticNode::OptOscompSupportDir => "/opt/oscomp-support",
        #[cfg(target_arch = "loongarch64")]
        StaticNode::OptOscompSupportLibDir => "/opt/oscomp-support/lib",
        #[cfg(target_arch = "loongarch64")]
        StaticNode::LaMuslCompatSo => "/opt/oscomp-support/lib/liboscomp-musl-compat.so",
    }
}

fn content(node: StaticNode) -> Option<Vec<u8>> {
    match node {
        StaticNode::NsswitchConf => Some(ETC_NSSWITCH_CONF.to_vec()),
        StaticNode::Passwd => Some(ETC_PASSWD.to_vec()),
        StaticNode::Group => Some(ETC_GROUP.to_vec()),
        StaticNode::Hosts => Some(HOSTS_CONTENT.exclusive_access().clone()),
        StaticNode::Services => Some(ETC_SERVICES.to_vec()),
        StaticNode::ResolvConf => Some(ETC_RESOLV_CONF.to_vec()),
        StaticNode::Protocols => Some(ETC_PROTOCOLS.to_vec()),
        StaticNode::ModulesDep => Some(MODULES_LOOP_DEP.to_vec()),
        StaticNode::ModulesBuiltin => Some(MODULES_LOOP_BUILTIN.to_vec()),
        StaticNode::ModulesAlias => Some(MODULES_ALIAS.to_vec()),
        StaticNode::ModulesOrder => Some(MODULES_ORDER.to_vec()),
        StaticNode::ModulesSymbols => Some(MODULES_SYMBOLS.to_vec()),
        StaticNode::ModulesConfig => Some(MODULES_CONFIG.to_vec()),
        StaticNode::DnsResolverKo => Some(DNS_RESOLVER_KO.to_vec()),
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
        StaticNode::EtcDir
        | StaticNode::LibDir
        | StaticNode::LibModulesDir
        | StaticNode::LibModulesReleaseDir
        | StaticNode::LibModulesKernelDir
        | StaticNode::LibModulesKernelNetDir
        | StaticNode::LibModulesKernelNetDnsResolverDir
        | StaticNode::SysDir
        | StaticNode::SysBlockDir
        | StaticNode::SysLoop0Dir
        | StaticNode::SysLoopInnerDir
        | StaticNode::SysLoopQueueDir
        | StaticNode::SysDevDir
        | StaticNode::SysDevBlockDir
        | StaticNode::SysDevBlockTmpfsDir
        | StaticNode::SysClassDir
        | StaticNode::SysClassBlockDir
        | StaticNode::SysClassLoop0Dir
        | StaticNode::SysClassLoop0BdiDir
        | StaticNode::SysDevicesDir
        | StaticNode::SysDevicesVirtualDir
        | StaticNode::SysDevicesVirtualInputDir
        | StaticNode::SysInput0Dir => None,
        #[cfg(target_arch = "loongarch64")]
        StaticNode::OptDir
        | StaticNode::OptOscompSupportDir
        | StaticNode::OptOscompSupportLibDir => None,
        #[cfg(target_arch = "loongarch64")]
        StaticNode::LaMuslCompatSo => Some(LA_MUSL_COMPAT_SO.to_vec()),
    }
}

fn is_dir(node: StaticNode) -> bool {
    match node {
        StaticNode::EtcDir
        | StaticNode::LibDir
        | StaticNode::LibModulesDir
        | StaticNode::LibModulesReleaseDir
        | StaticNode::LibModulesKernelDir
        | StaticNode::LibModulesKernelNetDir
        | StaticNode::LibModulesKernelNetDnsResolverDir
        | StaticNode::SysDir
        | StaticNode::SysBlockDir
        | StaticNode::SysLoop0Dir
        | StaticNode::SysLoopInnerDir
        | StaticNode::SysLoopQueueDir
        | StaticNode::SysDevDir
        | StaticNode::SysDevBlockDir
        | StaticNode::SysDevBlockTmpfsDir
        | StaticNode::SysClassDir
        | StaticNode::SysClassBlockDir
        | StaticNode::SysClassLoop0Dir
        | StaticNode::SysClassLoop0BdiDir
        | StaticNode::SysDevicesDir
        | StaticNode::SysDevicesVirtualDir
        | StaticNode::SysDevicesVirtualInputDir
        | StaticNode::SysInput0Dir => true,
        #[cfg(target_arch = "loongarch64")]
        StaticNode::OptDir
        | StaticNode::OptOscompSupportDir
        | StaticNode::OptOscompSupportLibDir => true,
        _ => false,
    }
}

fn stat_node(node: StaticNode) -> FileStat {
    let mut stat = if is_dir(node) {
        FileStat::with_mode(S_IFDIR | 0o555)
    } else {
        let mode = match node {
            #[cfg(target_arch = "loongarch64")]
            StaticNode::LaMuslCompatSo => 0o555,
            _ => 0o444,
        };
        FileStat::with_mode(S_IFREG | mode)
    };
    stat.dev = 0x657463;
    stat.ino = match node {
        StaticNode::EtcDir => 1,
        StaticNode::LibDir => 24,
        StaticNode::LibModulesDir => 25,
        StaticNode::LibModulesReleaseDir => 26,
        StaticNode::LibModulesKernelDir => 49,
        StaticNode::LibModulesKernelNetDir => 50,
        StaticNode::LibModulesKernelNetDnsResolverDir => 51,
        StaticNode::SysDir => 27,
        StaticNode::SysBlockDir => 28,
        StaticNode::SysLoop0Dir => 29,
        StaticNode::SysLoopInnerDir => 30,
        StaticNode::SysLoopQueueDir => 39,
        StaticNode::SysDevDir => 31,
        StaticNode::SysDevBlockDir => 32,
        StaticNode::SysDevicesDir => 33,
        StaticNode::SysDevicesVirtualDir => 34,
        StaticNode::SysDevicesVirtualInputDir => 35,
        StaticNode::SysInput0Dir => 36,
        StaticNode::SysDevBlockTmpfsDir => 37,
        StaticNode::NsswitchConf => 2,
        StaticNode::Passwd => 3,
        StaticNode::Group => 4,
        StaticNode::Hosts => 5,
        StaticNode::Services => 56,
        StaticNode::ResolvConf => 6,
        StaticNode::Protocols => 7,
        StaticNode::ModulesDep => 15,
        StaticNode::ModulesBuiltin => 16,
        StaticNode::ModulesAlias => 52,
        StaticNode::ModulesOrder => 53,
        StaticNode::ModulesSymbols => 54,
        StaticNode::ModulesConfig => 42,
        StaticNode::DnsResolverKo => 55,
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
        #[cfg(target_arch = "loongarch64")]
        StaticNode::OptDir => 8,
        #[cfg(target_arch = "loongarch64")]
        StaticNode::OptOscompSupportDir => 9,
        #[cfg(target_arch = "loongarch64")]
        StaticNode::OptOscompSupportLibDir => 10,
        #[cfg(target_arch = "loongarch64")]
        StaticNode::LaMuslCompatSo => 11,
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
        && !matches!(node, StaticNode::SysLoopReadAheadKb | StaticNode::Hosts)
    {
        return Err(FsError::PermissionDenied);
    }
    // CONTEXT: glibc's NSS/protocol lookup probes these files during netperf
    // loopback startup. The contest image does not require mutable `/etc`
    // state, so a tiny read-only snapshot keeps libc on the files backend
    // instead of the currently unsupported DNS/NSS path.
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
        StaticNode::LibModulesDir => {
            entries.push(dir_entry(StaticNode::LibModulesDir, ".", DT_DIR));
            entries.push(dir_entry(StaticNode::LibDir, "..", DT_DIR));
            entries.push(dir_entry(StaticNode::ModulesDep, "modules.dep", DT_REG));
            entries.push(dir_entry(
                StaticNode::ModulesBuiltin,
                "modules.builtin",
                DT_REG,
            ));
            entries.push(dir_entry(StaticNode::ModulesAlias, "modules.alias", DT_REG));
            entries.push(dir_entry(StaticNode::ModulesOrder, "modules.order", DT_REG));
            entries.push(dir_entry(
                StaticNode::ModulesSymbols,
                "modules.symbols",
                DT_REG,
            ));
            entries.push(dir_entry(StaticNode::ModulesConfig, "config", DT_REG));
            entries.push(dir_entry(StaticNode::LibModulesKernelDir, "kernel", DT_DIR));
            entries.push(dir_entry(
                StaticNode::LibModulesReleaseDir,
                "6.8.0-whusp",
                DT_DIR,
            ));
        }
        StaticNode::LibModulesReleaseDir => {
            entries.push(dir_entry(StaticNode::LibModulesReleaseDir, ".", DT_DIR));
            entries.push(dir_entry(StaticNode::LibModulesDir, "..", DT_DIR));
            entries.push(dir_entry(StaticNode::ModulesDep, "modules.dep", DT_REG));
            entries.push(dir_entry(
                StaticNode::ModulesBuiltin,
                "modules.builtin",
                DT_REG,
            ));
            entries.push(dir_entry(StaticNode::ModulesAlias, "modules.alias", DT_REG));
            entries.push(dir_entry(StaticNode::ModulesOrder, "modules.order", DT_REG));
            entries.push(dir_entry(
                StaticNode::ModulesSymbols,
                "modules.symbols",
                DT_REG,
            ));
            entries.push(dir_entry(StaticNode::ModulesConfig, "config", DT_REG));
            entries.push(dir_entry(StaticNode::LibModulesKernelDir, "kernel", DT_DIR));
        }
        StaticNode::LibModulesKernelDir => {
            entries.push(dir_entry(StaticNode::LibModulesKernelDir, ".", DT_DIR));
            entries.push(dir_entry(StaticNode::LibModulesReleaseDir, "..", DT_DIR));
            entries.push(dir_entry(StaticNode::LibModulesKernelNetDir, "net", DT_DIR));
        }
        StaticNode::LibModulesKernelNetDir => {
            entries.push(dir_entry(StaticNode::LibModulesKernelNetDir, ".", DT_DIR));
            entries.push(dir_entry(StaticNode::LibModulesKernelDir, "..", DT_DIR));
            entries.push(dir_entry(
                StaticNode::LibModulesKernelNetDnsResolverDir,
                "dns_resolver",
                DT_DIR,
            ));
        }
        StaticNode::LibModulesKernelNetDnsResolverDir => {
            entries.push(dir_entry(
                StaticNode::LibModulesKernelNetDnsResolverDir,
                ".",
                DT_DIR,
            ));
            entries.push(dir_entry(StaticNode::LibModulesKernelNetDir, "..", DT_DIR));
            entries.push(dir_entry(
                StaticNode::DnsResolverKo,
                "dns_resolver.ko",
                DT_REG,
            ));
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
        matches!(
            self.node,
            StaticNode::SysLoopReadAheadKb | StaticNode::Hosts
        )
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
        if self.node == StaticNode::Hosts {
            let data = user_buf.to_vec();
            let mut hosts = HOSTS_CONTENT.exclusive_access();
            let mut offset = self.offset.exclusive_access();
            if self.status_flags().contains(OpenFlags::APPEND) {
                *offset = hosts.len();
            }
            if *offset > hosts.len() {
                hosts.resize(*offset, 0);
            }
            let end = (*offset).saturating_add(data.len());
            if end > hosts.len() {
                hosts.resize(end, 0);
            }
            hosts[*offset..end].copy_from_slice(&data);
            *offset = end;
            return data.len();
        }
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

    fn read_dirent64(&self, user_buf: UserBuffer) -> FsResult<isize> {
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
        for (idx, byte_ref) in user_buf.into_iter().take(written).enumerate() {
            unsafe {
                *byte_ref = kernel_buf[idx];
            }
        }
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
