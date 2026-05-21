use super::{File, FileStat, FsError, FsResult, OpenFlags, PollEvents, S_IFDIR, S_IFREG};
use crate::mm::UserBuffer;
use crate::sync::UPIntrFreeCell;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::any::Any;

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
const ETC_RESOLV_CONF: &[u8] = b"";
const ETC_PROTOCOLS: &[u8] = b"ip 0 IP\ntcp 6 TCP\nudp 17 UDP\n";
const PROC_BUS_INPUT_DEVICES: &[u8] =
    b"I: Bus=0003 Vendor=0001 Product=0001 Version=0001\nN: Name=\"virtual-device-ltp\"\n";
const SYS_INPUT0_NAME: &[u8] = b"virtual-device-ltp\n";
const PROC_RANDOM_ENTROPY_AVAIL: &[u8] = b"256\n";
const MODULES_LOOP_DEP: &[u8] = b"kernel/drivers/block/loop.ko:\n";
const MODULES_LOOP_BUILTIN: &[u8] = b"kernel/drivers/block/loop.ko\n";
const MODULES_CONFIG: &[u8] = b"CONFIG_FS_VERITY=y\n";
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
    SysDir,
    SysBlockDir,
    SysLoop0Dir,
    SysLoopInnerDir,
    SysDevDir,
    SysDevBlockDir,
    SysDevBlockTmpfsDir,
    SysDevicesDir,
    SysDevicesVirtualDir,
    SysDevicesVirtualInputDir,
    SysInput0Dir,
    NsswitchConf,
    Passwd,
    Group,
    Hosts,
    ResolvConf,
    Protocols,
    ModulesDep,
    ModulesBuiltin,
    ModulesConfig,
    ProcBusInputDevices,
    SysInput0Name,
    ProcRandomEntropyAvail,
    SysLoopSize,
    SysLoopReadOnly,
    SysLoopPartscan,
    SysLoopAutoclear,
    SysLoopBackingFile,
    SysLoopDirectIo,
    SysLoopSizeLimit,
    SysLoopQueueDir,
    SysLoopLogicalBlockSize,
    SysLoopDmaAlignment,
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
    offset: UPIntrFreeCell<usize>,
    status_flags: UPIntrFreeCell<OpenFlags>,
}

impl StaticFile {
    fn new(node: StaticNode, flags: OpenFlags) -> Arc<Self> {
        Arc::new(Self {
            node,
            offset: unsafe { UPIntrFreeCell::new(0) },
            status_flags: unsafe { UPIntrFreeCell::new(OpenFlags::file_status_flags(flags)) },
        })
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
        "/sys" | "/sys/" => Some(StaticNode::SysDir),
        "/sys/block" | "/sys/block/" => Some(StaticNode::SysBlockDir),
        "/sys/block/loop0" | "/sys/block/loop0/" => Some(StaticNode::SysLoop0Dir),
        "/sys/block/loop0/loop" | "/sys/block/loop0/loop/" => Some(StaticNode::SysLoopInnerDir),
        "/sys/block/loop0/queue" | "/sys/block/loop0/queue/" => Some(StaticNode::SysLoopQueueDir),
        "/sys/dev" | "/sys/dev/" => Some(StaticNode::SysDevDir),
        "/sys/dev/block" | "/sys/dev/block/" => Some(StaticNode::SysDevBlockDir),
        "/sys/dev/block/254:0" | "/sys/dev/block/254:0/" => Some(StaticNode::SysDevBlockTmpfsDir),
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
        "/etc/resolv.conf" => Some(StaticNode::ResolvConf),
        "/etc/protocols" => Some(StaticNode::Protocols),
        "/lib/modules/6.8.0-whusp/modules.dep" => Some(StaticNode::ModulesDep),
        "/lib/modules/6.8.0-whusp/modules.builtin" => Some(StaticNode::ModulesBuiltin),
        "/lib/modules/6.8.0-whusp/config" => Some(StaticNode::ModulesConfig),
        "/proc/bus/input/devices" => Some(StaticNode::ProcBusInputDevices),
        "/proc/sys/kernel/random/entropy_avail" => Some(StaticNode::ProcRandomEntropyAvail),
        "/sys/devices/virtual/input/input0/name" => Some(StaticNode::SysInput0Name),
        "/sys/block/loop0/size" => Some(StaticNode::SysLoopSize),
        "/sys/block/loop0/ro" => Some(StaticNode::SysLoopReadOnly),
        "/sys/block/loop0/loop/partscan" => Some(StaticNode::SysLoopPartscan),
        "/sys/block/loop0/loop/autoclear" => Some(StaticNode::SysLoopAutoclear),
        "/sys/block/loop0/loop/backing_file" => Some(StaticNode::SysLoopBackingFile),
        "/sys/block/loop0/loop/dio" => Some(StaticNode::SysLoopDirectIo),
        "/sys/block/loop0/loop/sizelimit" => Some(StaticNode::SysLoopSizeLimit),
        "/sys/block/loop0/queue/logical_block_size" => Some(StaticNode::SysLoopLogicalBlockSize),
        "/sys/block/loop0/queue/dma_alignment" => Some(StaticNode::SysLoopDmaAlignment),
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

fn content(node: StaticNode) -> Option<Vec<u8>> {
    match node {
        StaticNode::NsswitchConf => Some(ETC_NSSWITCH_CONF.to_vec()),
        StaticNode::Passwd => Some(ETC_PASSWD.to_vec()),
        StaticNode::Group => Some(ETC_GROUP.to_vec()),
        StaticNode::Hosts => Some(ETC_HOSTS.to_vec()),
        StaticNode::ResolvConf => Some(ETC_RESOLV_CONF.to_vec()),
        StaticNode::Protocols => Some(ETC_PROTOCOLS.to_vec()),
        StaticNode::ModulesDep => Some(MODULES_LOOP_DEP.to_vec()),
        StaticNode::ModulesBuiltin => Some(MODULES_LOOP_BUILTIN.to_vec()),
        StaticNode::ModulesConfig => Some(MODULES_CONFIG.to_vec()),
        StaticNode::ProcBusInputDevices => Some(PROC_BUS_INPUT_DEVICES.to_vec()),
        StaticNode::SysInput0Name => Some(SYS_INPUT0_NAME.to_vec()),
        StaticNode::ProcRandomEntropyAvail => Some(PROC_RANDOM_ENTROPY_AVAIL.to_vec()),
        StaticNode::SysLoopSize => super::devfs::loop_device_sysfs_content("/sys/block/loop0/size"),
        StaticNode::SysLoopReadOnly => {
            super::devfs::loop_device_sysfs_content("/sys/block/loop0/ro")
        }
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
        StaticNode::SysDevBlockTmpfsUevent => Some(SYS_DEV_BLOCK_TMPFS_UEVENT.to_vec()),
        StaticNode::EtcDir
        | StaticNode::LibDir
        | StaticNode::LibModulesDir
        | StaticNode::LibModulesReleaseDir
        | StaticNode::SysDir
        | StaticNode::SysBlockDir
        | StaticNode::SysLoop0Dir
        | StaticNode::SysLoopInnerDir
        | StaticNode::SysLoopQueueDir
        | StaticNode::SysDevDir
        | StaticNode::SysDevBlockDir
        | StaticNode::SysDevBlockTmpfsDir
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
        | StaticNode::SysDir
        | StaticNode::SysBlockDir
        | StaticNode::SysLoop0Dir
        | StaticNode::SysLoopInnerDir
        | StaticNode::SysLoopQueueDir
        | StaticNode::SysDevDir
        | StaticNode::SysDevBlockDir
        | StaticNode::SysDevBlockTmpfsDir
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
        StaticNode::ResolvConf => 6,
        StaticNode::Protocols => 7,
        StaticNode::ModulesDep => 15,
        StaticNode::ModulesBuiltin => 16,
        StaticNode::ModulesConfig => 42,
        StaticNode::ProcBusInputDevices => 12,
        StaticNode::SysInput0Name => 13,
        StaticNode::ProcRandomEntropyAvail => 14,
        StaticNode::SysLoopSize => 17,
        StaticNode::SysLoopReadOnly => 18,
        StaticNode::SysLoopPartscan => 19,
        StaticNode::SysLoopAutoclear => 20,
        StaticNode::SysLoopBackingFile => 21,
        StaticNode::SysLoopDirectIo => 22,
        StaticNode::SysLoopSizeLimit => 23,
        StaticNode::SysLoopLogicalBlockSize => 40,
        StaticNode::SysLoopDmaAlignment => 41,
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
        return Err(FsError::IsDir);
    }
    if flags.writable_target() || flags.contains(OpenFlags::TRUNC) {
        return Err(FsError::PermissionDenied);
    }
    // CONTEXT: glibc's NSS/protocol lookup probes these files during netperf
    // loopback startup. The contest image does not require mutable `/etc`
    // state, so a tiny read-only snapshot keeps libc on the files backend
    // instead of the currently unsupported DNS/NSS path.
    Ok(Some(StaticFile::new(node, flags)))
}

impl File for StaticFile {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn readable(&self) -> bool {
        content(self.node).is_some()
    }

    fn writable(&self) -> bool {
        false
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

    fn write(&self, _user_buf: UserBuffer) -> usize {
        0
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
}
