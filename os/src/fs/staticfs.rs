use super::dirent::{DT_DIR, DT_REG, RawDirEntry, write_dir_entries};
use super::{
    File, FileStat, FileTimestamp, FsError, FsResult, OpenFlags, PollEvents, S_IFDIR, S_IFREG,
};
use crate::mm::UserBuffer;
use crate::sync::SpinNoIrqLock;
use alloc::borrow::Cow;
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::any::Any;
use lazy_static::lazy_static;

// CONTEXT: The opt-in LTP compatibility overlay exposes only values backed by
// live kernel state. Ordinary static files belong on the generated script disk.

lazy_static! {
    static ref STATICFS_TIMESTAMP: FileTimestamp = FileTimestamp::now();
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum StaticNode {
    SysDir,
    SysBlockDir,
    SysLoop0Dir,
    SysLoopInnerDir,
    SysClassDir,
    SysClassBlockDir,
    SysClassLoop0Dir,
    SysClassLoop0BdiDir,
    SysDevicesDir,
    SysDevicesSystemDir,
    SysCpuRootDir,
    SysCpuOnline,
    SysCpuPossible,
    SysCpuPresent,
    SysCpuKernelMax,
    SysCpuDir(usize),
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
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum StaticNodeKind {
    Dir,
    File,
}

impl StaticNodeKind {
    fn is_dir(self) -> bool {
        matches!(self, Self::Dir)
    }

    fn dtype(self) -> u8 {
        match self {
            Self::Dir => DT_DIR,
            Self::File => DT_REG,
        }
    }
}

#[derive(Clone, Copy)]
struct StaticNodeDesc {
    node: StaticNode,
    path: &'static str,
    ino: u32,
    kind: StaticNodeKind,
    parent: Option<StaticNode>,
    name: &'static str,
}

const CPU_PATH_PREFIX: &str = "/sys/devices/system/cpu/cpu";
const SYS_CPU_INO_BASE: u32 = 81;

const STATIC_NODE_DESCS: &[StaticNodeDesc] = &[
    StaticNodeDesc {
        node: StaticNode::SysDir,
        path: "/sys",
        ino: 27,
        kind: StaticNodeKind::Dir,
        parent: None,
        name: "sys",
    },
    StaticNodeDesc {
        node: StaticNode::SysBlockDir,
        path: "/sys/block",
        ino: 28,
        kind: StaticNodeKind::Dir,
        parent: Some(StaticNode::SysDir),
        name: "block",
    },
    StaticNodeDesc {
        node: StaticNode::SysLoop0Dir,
        path: "/sys/block/loop0",
        ino: 29,
        kind: StaticNodeKind::Dir,
        parent: Some(StaticNode::SysBlockDir),
        name: "loop0",
    },
    StaticNodeDesc {
        node: StaticNode::SysLoopInnerDir,
        path: "/sys/block/loop0/loop",
        ino: 30,
        kind: StaticNodeKind::Dir,
        parent: Some(StaticNode::SysLoop0Dir),
        name: "loop",
    },
    StaticNodeDesc {
        node: StaticNode::SysLoopPartscan,
        path: "/sys/block/loop0/loop/partscan",
        ino: 19,
        kind: StaticNodeKind::File,
        parent: Some(StaticNode::SysLoopInnerDir),
        name: "partscan",
    },
    StaticNodeDesc {
        node: StaticNode::SysLoopAutoclear,
        path: "/sys/block/loop0/loop/autoclear",
        ino: 20,
        kind: StaticNodeKind::File,
        parent: Some(StaticNode::SysLoopInnerDir),
        name: "autoclear",
    },
    StaticNodeDesc {
        node: StaticNode::SysLoopBackingFile,
        path: "/sys/block/loop0/loop/backing_file",
        ino: 21,
        kind: StaticNodeKind::File,
        parent: Some(StaticNode::SysLoopInnerDir),
        name: "backing_file",
    },
    StaticNodeDesc {
        node: StaticNode::SysLoopDirectIo,
        path: "/sys/block/loop0/loop/dio",
        ino: 22,
        kind: StaticNodeKind::File,
        parent: Some(StaticNode::SysLoopInnerDir),
        name: "dio",
    },
    StaticNodeDesc {
        node: StaticNode::SysLoopSizeLimit,
        path: "/sys/block/loop0/loop/sizelimit",
        ino: 23,
        kind: StaticNodeKind::File,
        parent: Some(StaticNode::SysLoopInnerDir),
        name: "sizelimit",
    },
    StaticNodeDesc {
        node: StaticNode::SysLoopQueueDir,
        path: "/sys/block/loop0/queue",
        ino: 39,
        kind: StaticNodeKind::Dir,
        parent: Some(StaticNode::SysLoop0Dir),
        name: "queue",
    },
    StaticNodeDesc {
        node: StaticNode::SysLoopLogicalBlockSize,
        path: "/sys/block/loop0/queue/logical_block_size",
        ino: 40,
        kind: StaticNodeKind::File,
        parent: Some(StaticNode::SysLoopQueueDir),
        name: "logical_block_size",
    },
    StaticNodeDesc {
        node: StaticNode::SysLoopDmaAlignment,
        path: "/sys/block/loop0/queue/dma_alignment",
        ino: 41,
        kind: StaticNodeKind::File,
        parent: Some(StaticNode::SysLoopQueueDir),
        name: "dma_alignment",
    },
    StaticNodeDesc {
        node: StaticNode::SysLoopSize,
        path: "/sys/block/loop0/size",
        ino: 17,
        kind: StaticNodeKind::File,
        parent: Some(StaticNode::SysLoop0Dir),
        name: "size",
    },
    StaticNodeDesc {
        node: StaticNode::SysLoopReadOnly,
        path: "/sys/block/loop0/ro",
        ino: 18,
        kind: StaticNodeKind::File,
        parent: Some(StaticNode::SysLoop0Dir),
        name: "ro",
    },
    StaticNodeDesc {
        node: StaticNode::SysLoopStat,
        path: "/sys/block/loop0/stat",
        ino: 48,
        kind: StaticNodeKind::File,
        parent: Some(StaticNode::SysLoop0Dir),
        name: "stat",
    },
    StaticNodeDesc {
        node: StaticNode::SysClassDir,
        path: "/sys/class",
        ino: 43,
        kind: StaticNodeKind::Dir,
        parent: Some(StaticNode::SysDir),
        name: "class",
    },
    StaticNodeDesc {
        node: StaticNode::SysClassBlockDir,
        path: "/sys/class/block",
        ino: 44,
        kind: StaticNodeKind::Dir,
        parent: Some(StaticNode::SysClassDir),
        name: "block",
    },
    StaticNodeDesc {
        node: StaticNode::SysClassLoop0Dir,
        path: "/sys/class/block/loop0",
        ino: 45,
        kind: StaticNodeKind::Dir,
        parent: Some(StaticNode::SysClassBlockDir),
        name: "loop0",
    },
    StaticNodeDesc {
        node: StaticNode::SysClassLoop0BdiDir,
        path: "/sys/class/block/loop0/bdi",
        ino: 46,
        kind: StaticNodeKind::Dir,
        parent: Some(StaticNode::SysClassLoop0Dir),
        name: "bdi",
    },
    StaticNodeDesc {
        node: StaticNode::SysLoopReadAheadKb,
        path: "/sys/class/block/loop0/bdi/read_ahead_kb",
        ino: 47,
        kind: StaticNodeKind::File,
        parent: Some(StaticNode::SysClassLoop0BdiDir),
        name: "read_ahead_kb",
    },
    StaticNodeDesc {
        node: StaticNode::SysDevicesDir,
        path: "/sys/devices",
        ino: 33,
        kind: StaticNodeKind::Dir,
        parent: Some(StaticNode::SysDir),
        name: "devices",
    },
    StaticNodeDesc {
        node: StaticNode::SysDevicesSystemDir,
        path: "/sys/devices/system",
        ino: 76,
        kind: StaticNodeKind::Dir,
        parent: Some(StaticNode::SysDevicesDir),
        name: "system",
    },
    StaticNodeDesc {
        node: StaticNode::SysCpuRootDir,
        path: "/sys/devices/system/cpu",
        ino: 77,
        kind: StaticNodeKind::Dir,
        parent: Some(StaticNode::SysDevicesSystemDir),
        name: "cpu",
    },
    StaticNodeDesc {
        node: StaticNode::SysCpuOnline,
        path: "/sys/devices/system/cpu/online",
        ino: 78,
        kind: StaticNodeKind::File,
        parent: Some(StaticNode::SysCpuRootDir),
        name: "online",
    },
    StaticNodeDesc {
        node: StaticNode::SysCpuPossible,
        path: "/sys/devices/system/cpu/possible",
        ino: 79,
        kind: StaticNodeKind::File,
        parent: Some(StaticNode::SysCpuRootDir),
        name: "possible",
    },
    StaticNodeDesc {
        node: StaticNode::SysCpuPresent,
        path: "/sys/devices/system/cpu/present",
        ino: 80,
        kind: StaticNodeKind::File,
        parent: Some(StaticNode::SysCpuRootDir),
        name: "present",
    },
    StaticNodeDesc {
        node: StaticNode::SysCpuKernelMax,
        path: "/sys/devices/system/cpu/kernel_max",
        ino: 89,
        kind: StaticNodeKind::File,
        parent: Some(StaticNode::SysCpuRootDir),
        name: "kernel_max",
    },
];

pub struct StaticFile {
    node: StaticNode,
    path: Cow<'static, str>,
    offset: SpinNoIrqLock<usize>,
    status_flags: SpinNoIrqLock<OpenFlags>,
}

impl StaticFile {
    fn new(node: StaticNode, path: Cow<'static, str>, flags: OpenFlags) -> Arc<Self> {
        Arc::new(Self {
            node,
            path,
            offset: SpinNoIrqLock::new(0),
            status_flags: SpinNoIrqLock::new(OpenFlags::file_status_flags(flags)),
        })
    }
}

pub(crate) fn init() {
    let _ = *STATICFS_TIMESTAMP;
}

fn node_desc(node: StaticNode) -> Option<&'static StaticNodeDesc> {
    STATIC_NODE_DESCS.iter().find(|desc| desc.node == node)
}

fn cpu_id_from_path(path: &str) -> Option<usize> {
    let suffix = path.strip_prefix(CPU_PATH_PREFIX)?;
    if suffix.is_empty()
        || (suffix.len() > 1 && suffix.as_bytes()[0] == b'0')
        || suffix.bytes().any(|byte| !(b'0'..=b'9').contains(&byte))
    {
        return None;
    }
    let cpu = suffix.parse().ok()?;
    (cpu < crate::config::MAX_CPUS).then_some(cpu)
}

fn lookup_absolute(path: &str) -> Option<StaticNode> {
    let normalized_path = path.strip_suffix('/').unwrap_or(path);
    let has_trailing_slash = normalized_path != path;
    if let Some(cpu) = cpu_id_from_path(normalized_path) {
        return Some(StaticNode::SysCpuDir(cpu));
    }
    let desc = STATIC_NODE_DESCS
        .iter()
        .find(|desc| desc.path == normalized_path)?;
    if has_trailing_slash && !desc.kind.is_dir() {
        return None;
    }
    Some(desc.node)
}

fn canonical_path(node: StaticNode) -> Cow<'static, str> {
    match node {
        StaticNode::SysCpuDir(cpu) => Cow::Owned(format!("{CPU_PATH_PREFIX}{cpu}")),
        _ => Cow::Borrowed(
            node_desc(node)
                .expect("staticfs node missing from description table")
                .path,
        ),
    }
}

fn has_content(node: StaticNode) -> bool {
    if is_dir(node) {
        return false;
    }
    match node {
        StaticNode::SysLoopReadAheadKb => super::devfs::loop_device_is_attached(0),
        _ => true,
    }
}

fn decimal_len(mut value: u64) -> usize {
    let mut len = 1;
    while value >= 10 {
        value /= 10;
        len += 1;
    }
    len
}

fn cpu_list_len(mask: crate::cpu::CpuMask) -> usize {
    let mut len = 1;
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
            len += 1;
        }
        len += decimal_len(start as u64);
        if start != cpu {
            len += 1 + decimal_len(cpu as u64);
        }
        first_range = false;
        cpu += 1;
    }
    len
}

fn content_len(node: StaticNode) -> usize {
    match node {
        StaticNode::SysCpuOnline => cpu_list_len(crate::cpu::online_mask()),
        StaticNode::SysCpuPossible | StaticNode::SysCpuPresent => {
            cpu_list_len(crate::cpu::topology().possible_mask())
        }
        StaticNode::SysCpuKernelMax => decimal_len((crate::config::MAX_CPUS - 1) as u64) + 1,
        StaticNode::SysLoopSize => {
            super::devfs::loop_device_sysfs_content_len("/sys/block/loop0/size").unwrap_or(0)
        }
        StaticNode::SysLoopReadOnly => {
            super::devfs::loop_device_sysfs_content_len("/sys/block/loop0/ro").unwrap_or(0)
        }
        StaticNode::SysLoopStat => {
            super::devfs::loop_device_sysfs_content_len("/sys/block/loop0/stat").unwrap_or(0)
        }
        StaticNode::SysLoopPartscan => {
            super::devfs::loop_device_sysfs_content_len("/sys/block/loop0/loop/partscan")
                .unwrap_or(0)
        }
        StaticNode::SysLoopAutoclear => {
            super::devfs::loop_device_sysfs_content_len("/sys/block/loop0/loop/autoclear")
                .unwrap_or(0)
        }
        StaticNode::SysLoopBackingFile => {
            super::devfs::loop_device_sysfs_content_len("/sys/block/loop0/loop/backing_file")
                .unwrap_or(0)
        }
        StaticNode::SysLoopDirectIo => {
            super::devfs::loop_device_sysfs_content_len("/sys/block/loop0/loop/dio").unwrap_or(0)
        }
        StaticNode::SysLoopSizeLimit => {
            super::devfs::loop_device_sysfs_content_len("/sys/block/loop0/loop/sizelimit")
                .unwrap_or(0)
        }
        StaticNode::SysLoopLogicalBlockSize => {
            super::devfs::loop_device_sysfs_content_len("/sys/block/loop0/queue/logical_block_size")
                .unwrap_or(0)
        }
        StaticNode::SysLoopDmaAlignment => {
            super::devfs::loop_device_sysfs_content_len("/sys/block/loop0/queue/dma_alignment")
                .unwrap_or(0)
        }
        StaticNode::SysLoopReadAheadKb => {
            super::devfs::loop_device_sysfs_content_len("/sys/class/block/loop0/bdi/read_ahead_kb")
                .unwrap_or(0)
        }
        _ => 0,
    }
}

fn content(node: StaticNode) -> Option<Cow<'static, [u8]>> {
    match node {
        StaticNode::SysCpuOnline => Some(Cow::Owned(cpu_list_content(crate::cpu::online_mask()))),
        StaticNode::SysCpuPossible | StaticNode::SysCpuPresent => Some(Cow::Owned(
            cpu_list_content(crate::cpu::topology().possible_mask()),
        )),
        StaticNode::SysCpuKernelMax => Some(Cow::Owned(
            format!("{}\n", crate::config::MAX_CPUS - 1).into_bytes(),
        )),
        StaticNode::SysLoopSize => {
            super::devfs::loop_device_sysfs_content("/sys/block/loop0/size").map(Cow::Owned)
        }
        StaticNode::SysLoopReadOnly => {
            super::devfs::loop_device_sysfs_content("/sys/block/loop0/ro").map(Cow::Owned)
        }
        StaticNode::SysLoopStat => {
            super::devfs::loop_device_sysfs_content("/sys/block/loop0/stat").map(Cow::Owned)
        }
        StaticNode::SysLoopPartscan => {
            super::devfs::loop_device_sysfs_content("/sys/block/loop0/loop/partscan")
                .map(Cow::Owned)
        }
        StaticNode::SysLoopAutoclear => {
            super::devfs::loop_device_sysfs_content("/sys/block/loop0/loop/autoclear")
                .map(Cow::Owned)
        }
        StaticNode::SysLoopBackingFile => {
            super::devfs::loop_device_sysfs_content("/sys/block/loop0/loop/backing_file")
                .map(Cow::Owned)
        }
        StaticNode::SysLoopDirectIo => {
            super::devfs::loop_device_sysfs_content("/sys/block/loop0/loop/dio").map(Cow::Owned)
        }
        StaticNode::SysLoopSizeLimit => {
            super::devfs::loop_device_sysfs_content("/sys/block/loop0/loop/sizelimit")
                .map(Cow::Owned)
        }
        StaticNode::SysLoopLogicalBlockSize => {
            super::devfs::loop_device_sysfs_content("/sys/block/loop0/queue/logical_block_size")
                .map(Cow::Owned)
        }
        StaticNode::SysLoopDmaAlignment => {
            super::devfs::loop_device_sysfs_content("/sys/block/loop0/queue/dma_alignment")
                .map(Cow::Owned)
        }
        StaticNode::SysLoopReadAheadKb => {
            super::devfs::loop_device_sysfs_content("/sys/class/block/loop0/bdi/read_ahead_kb")
                .map(Cow::Owned)
        }
        _ => None,
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
        StaticNode::SysCpuDir(_) => true,
        _ => node_desc(node)
            .map(|desc| desc.kind.is_dir())
            .unwrap_or(false),
    }
}

fn stat_node(node: StaticNode) -> FileStat {
    let (kind, ino) = match node {
        StaticNode::SysCpuDir(cpu) => (StaticNodeKind::Dir, SYS_CPU_INO_BASE as u64 + cpu as u64),
        _ => {
            let desc = node_desc(node).expect("staticfs node missing from description table");
            (desc.kind, desc.ino as u64)
        }
    };
    let mut stat = if kind.is_dir() {
        FileStat::with_mode(S_IFDIR | 0o555)
    } else {
        FileStat::with_mode(S_IFREG | 0o444)
    };
    stat.dev = 0x657463;
    stat.ino = ino;
    stat.nlink = if kind.is_dir() { 2 } else { 1 };
    stat.size = content_len(node) as u64;
    let timestamp = *STATICFS_TIMESTAMP;
    stat.atime_sec = timestamp.sec;
    stat.atime_nsec = timestamp.nsec;
    stat.mtime_sec = timestamp.sec;
    stat.mtime_nsec = timestamp.nsec;
    stat.ctime_sec = timestamp.sec;
    stat.ctime_nsec = timestamp.nsec;
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

fn parent_node(node: StaticNode) -> StaticNode {
    match node {
        StaticNode::SysCpuDir(_) => StaticNode::SysCpuRootDir,
        _ => node_desc(node).and_then(|desc| desc.parent).unwrap_or(node),
    }
}

fn dir_entries(node: StaticNode) -> Option<Vec<RawDirEntry>> {
    if !is_dir(node) {
        return None;
    }
    let mut entries = Vec::new();
    entries.push(dir_entry(node, ".", DT_DIR));
    entries.push(dir_entry(parent_node(node), "..", DT_DIR));
    for desc in STATIC_NODE_DESCS {
        if desc.parent == Some(node) {
            entries.push(dir_entry(desc.node, desc.name, desc.kind.dtype()));
        }
    }
    if node == StaticNode::SysCpuRootDir {
        for cpu in 0..crate::cpu::topology().possible_count() {
            entries.push(dir_entry(
                StaticNode::SysCpuDir(cpu),
                &format!("cpu{cpu}"),
                DT_DIR,
            ));
        }
    }
    Some(entries)
}

impl File for StaticFile {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn readable(&self) -> bool {
        has_content(self.node)
    }

    fn writable(&self) -> bool {
        matches!(self.node, StaticNode::SysLoopReadAheadKb)
    }

    fn read(&self, mut user_buf: UserBuffer) -> usize {
        let Some(content) = content(self.node) else {
            return 0;
        };
        let mut offset = self.offset.lock();
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
        let len = content_len(self.node);
        let base = match whence {
            super::SeekWhence::Set => 0,
            super::SeekWhence::Current => *self.offset.lock() as i64,
            super::SeekWhence::End => len as i64,
            super::SeekWhence::Data => {
                if offset < 0 {
                    return Err(FsError::InvalidInput);
                }
                let offset = offset as usize;
                if offset >= len {
                    return Err(FsError::NoDeviceOrAddress);
                }
                *self.offset.lock() = offset;
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
                *self.offset.lock() = len;
                return Ok(len);
            }
        };
        let next = base.checked_add(offset).ok_or(FsError::InvalidInput)?;
        if next < 0 {
            return Err(FsError::InvalidInput);
        }
        *self.offset.lock() = next as usize;
        Ok(next as usize)
    }

    fn status_flags(&self) -> OpenFlags {
        *self.status_flags.lock()
    }

    fn set_status_flags(&self, flags: OpenFlags) {
        *self.status_flags.lock() = flags;
    }

    fn read_dirent64(&self, mut user_buf: UserBuffer) -> FsResult<isize> {
        let Some(entries) = dir_entries(self.node) else {
            return Err(FsError::NotDir);
        };
        let mut kernel_buf = vec![0u8; user_buf.len()];
        let mut offset = self.offset.lock();
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
        Some(String::from(self.path.as_ref()))
    }
}
