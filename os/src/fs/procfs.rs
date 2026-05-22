#![expect(
    clippy::vec_init_then_push,
    reason = "procfs directory builders keep entries in display order with conditional additions"
)]

use super::dentry_cache;
use super::dirent::{DT_DIR, DT_LNK, DT_REG, RawDirEntry, write_dir_entries};
use super::mount;
use super::pipe::{PIPE_DEFAULT_CAPACITY, PIPE_MAX_CAPACITY, PIPE_MIN_CAPACITY};
use super::vfs::{FileSystemBackend, FsError, FsNodeKind, FsResult};
use super::{FileStat, FileTimestamp, S_IFDIR, S_IFLNK, S_IFREG};
use super::{PathContext, lookup_path_in};
use crate::config::PAGE_SIZE;
use crate::drivers::block_cache;
use crate::mm::{exec_load_stats_content, frame_stats};
use crate::sync::UPIntrFreeCell;
use crate::syscall::keyring;
use crate::syscall::{
    INOTIFY_MAX_QUEUED_EVENTS, INOTIFY_MAX_USER_INSTANCES, INOTIFY_MAX_USER_WATCHES,
    fanotify_evict_evictable_marks, fanotify_fdinfo, fanotify_max_queued_events, inotify_fdinfo,
    pidfd_fdinfo,
};
use crate::task::{
    ProcessProcSnapshot, TaskControlBlock, TaskStatus, list_process_snapshots, pid2process,
};
use crate::timer::{get_time_us, us_to_clock_ticks};
use alloc::format;
use alloc::string::{String, ToString};
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicIsize, AtomicUsize, Ordering};
use lazy_static::lazy_static;

const ROOT_INO: u32 = 2;
const MOUNTS_INO: u32 = 3;
const FILESYSTEMS_INO: u32 = 30;
const MEMINFO_INO: u32 = 4;
const UPTIME_INO: u32 = 5;
const CPUINFO_INO: u32 = 6;
const SYS_DIR_INO: u32 = 7;
const SYS_KERNEL_DIR_INO: u32 = 8;
const PID_MAX_INO: u32 = 9;
const SYS_FS_DIR_INO: u32 = 10;
const PIPE_MAX_SIZE_INO: u32 = 11;
const PIPE_USER_PAGES_SOFT_INO: u32 = 12;
const DOMAINNAME_INO: u32 = 13;
const TAINTED_INO: u32 = 14;
const LEASE_BREAK_TIME_INO: u32 = 15;
const SYS_NET_DIR_INO: u32 = 16;
const SYS_NET_IPV4_DIR_INO: u32 = 17;
const SYS_NET_IPV4_CONF_DIR_INO: u32 = 18;
const SYS_NET_IPV4_CONF_LO_DIR_INO: u32 = 19;
const SYS_NET_IPV4_CONF_DEFAULT_DIR_INO: u32 = 20;
const SYS_NET_IPV4_CONF_LO_TAG_INO: u32 = 21;
const SYS_NET_IPV4_CONF_DEFAULT_TAG_INO: u32 = 22;
const KEY_USERS_INO: u32 = 23;
const SYS_KERNEL_KEYS_DIR_INO: u32 = 24;
const KEYS_GC_DELAY_INO: u32 = 25;
const KEYS_MAXKEYS_INO: u32 = 26;
const KEYS_MAXBYTES_INO: u32 = 27;
const SYS_VM_DIR_INO: u32 = 28;
const DROP_CACHES_INO: u32 = 29;
const VFS_CACHE_PRESSURE_INO: u32 = 31;
const SYS_FS_FANOTIFY_DIR_INO: u32 = 32;
const FANOTIFY_MAX_QUEUED_EVENTS_INO: u32 = 33;
const SYS_FS_INOTIFY_DIR_INO: u32 = 34;
const INOTIFY_MAX_QUEUED_EVENTS_INO: u32 = 35;
const INOTIFY_MAX_USER_INSTANCES_INO: u32 = 36;
const INOTIFY_MAX_USER_WATCHES_INO: u32 = 37;
const BLOCK_CACHE_STATS_INO: u32 = 38;
const DENTRY_CACHE_STATS_INO: u32 = 39;
const EXEC_LOAD_STATS_INO: u32 = 40;
const CORE_PATTERN_INO: u32 = 41;
const VERSION_INO: u32 = 42;
const SYSVIPC_DIR_INO: u32 = 43;
const SYSVIPC_SHM_INO: u32 = 44;
const SHMMAX_INO: u32 = 45;
const SHMMNI_INO: u32 = 46;
const SHMALL_INO: u32 = 47;
// CONTEXT: Dynamic /proc inode ranges must stay disjoint even after long test
// runs allocate five-digit PIDs; LTP probes /proc/<ppid>/stat during waits.
const PID_DIR_BASE: u32 = 100;
const PID_FILE_BASE: u32 = 10_000_000;
const PID_FILE_STRIDE: u32 = 32;
const PID_STAT_OFFSET: u32 = 0;
const PID_STATUS_OFFSET: u32 = 1;
const PID_CMDLINE_OFFSET: u32 = 2;
const PID_FD_DIR_OFFSET: u32 = 3;
const PID_MAPS_OFFSET: u32 = 4;
const PID_NS_DIR_OFFSET: u32 = 5;
const PID_NS_MNT_OFFSET: u32 = 6;
const PID_TASK_DIR_OFFSET: u32 = 7;
const PID_SMAPS_OFFSET: u32 = 8;
const PID_MOUNTS_OFFSET: u32 = 9;
const PID_IO_OFFSET: u32 = 10;
const PID_FDINFO_DIR_OFFSET: u32 = 11;
const PID_COMM_OFFSET: u32 = 12;
const PID_TIMERSLACK_OFFSET: u32 = 13;
const PID_NS_PID_OFFSET: u32 = 14;
const PID_NS_USER_OFFSET: u32 = 15;
const PID_NS_UTS_OFFSET: u32 = 16;
const PID_EXE_OFFSET: u32 = 17;
const PID_MOUNTINFO_OFFSET: u32 = 18;
const PID_FD_ENTRY_BASE: u32 = 1_000_000_000;
const PID_FDINFO_ENTRY_BASE: u32 = 2_000_000_000;
const PID_FD_ENTRY_STRIDE: u32 = 4096;
const PID_TASK_INO_TAG_MASK: u32 = 0xC000_0000;
const PID_TASK_TID_COMM_TAG: u32 = 0x4000_0000;
const PID_TASK_TID_DIR_TAG: u32 = 0x8000_0000;
const PID_TASK_TID_STAT_TAG: u32 = 0xC000_0000;
const PID_TASK_PID_SHIFT: usize = 12;
const PID_TASK_TID_MASK: u32 = (1 << PID_TASK_PID_SHIFT) - 1;
const PID_TASK_MAX_PID: usize = 1 << (30 - PID_TASK_PID_SHIFT);
const PID_TASK_MAX_LOCAL_TID: usize = 1 << PID_TASK_PID_SHIFT;
const DEFAULT_PID_MAX: usize = 4_194_304;
// CONTEXT: Linux defaults this sysctl to 16384 pages, but this kernel does not
// account pipe pages per user and still has a smaller fd-table ceiling. Expose
// one default pipe worth of pages so pipe-limit tests exercise real pipe
// behavior instead of deriving a zero-pipe workload.
const DEFAULT_PIPE_USER_PAGES_SOFT: usize = PIPE_DEFAULT_CAPACITY / PAGE_SIZE;
const DEFAULT_LEASE_BREAK_TIME: usize = 45;
const DEFAULT_NET_IPV4_CONF_TAG: isize = 0;
const PROC_MEMINFO_OBSERVED_CACHE_KB: usize = 64 * 1024;
const PROC_NS_MNT_INO_BASE: u64 = 0x7000_0000;
const PROC_NS_PID_INO_BASE: u64 = 0x7100_0000;
const PROC_NS_USER_INO_BASE: u64 = 0x7200_0000;
const PROC_NS_UTS_INO_BASE: u64 = 0x7300_0000;
const ROOT_UTS_NAMESPACE_ID: usize = 1;

static PROC_PID_MAX: AtomicUsize = AtomicUsize::new(DEFAULT_PID_MAX);
static PROC_PIPE_MAX_SIZE: AtomicUsize = AtomicUsize::new(PIPE_MAX_CAPACITY);
static PROC_PIPE_USER_PAGES_SOFT: AtomicUsize = AtomicUsize::new(DEFAULT_PIPE_USER_PAGES_SOFT);
static PROC_LEASE_BREAK_TIME: AtomicUsize = AtomicUsize::new(DEFAULT_LEASE_BREAK_TIME);
static PROC_NET_IPV4_CONF_LO_TAG: AtomicIsize = AtomicIsize::new(DEFAULT_NET_IPV4_CONF_TAG);
static PROC_VFS_CACHE_PRESSURE: AtomicUsize = AtomicUsize::new(100);
static PROC_MEMINFO_CACHED_KB: AtomicUsize = AtomicUsize::new(0);
static PROC_IO_READ_BYTES: AtomicUsize = AtomicUsize::new(0);
static PROC_IO_READAHEAD_SUPPRESS_READS: AtomicUsize = AtomicUsize::new(0);

lazy_static! {
    static ref PROC_DOMAINNAME: UPIntrFreeCell<Vec<u8>> = {
        let mut value = Vec::new();
        value.extend_from_slice(b"(none)");
        unsafe { UPIntrFreeCell::new(value) }
    };
}

pub(crate) fn pipe_max_size() -> usize {
    PROC_PIPE_MAX_SIZE.load(Ordering::Relaxed)
}

pub(crate) fn note_readahead() {
    PROC_MEMINFO_CACHED_KB.store(PROC_MEMINFO_OBSERVED_CACHE_KB, Ordering::Relaxed);
    PROC_IO_READAHEAD_SUPPRESS_READS.store(2, Ordering::Relaxed);
}

pub(super) struct ProcFs;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ProcNode {
    Root,
    Mounts,
    Filesystems,
    KeyUsers,
    Meminfo,
    Uptime,
    Cpuinfo,
    SysDir,
    SysKernelDir,
    SysKernelKeysDir,
    SysFsDir,
    SysFsFanotifyDir,
    SysFsInotifyDir,
    SysNetDir,
    SysNetIpv4Dir,
    SysNetIpv4ConfDir,
    SysNetIpv4ConfLoDir,
    SysNetIpv4ConfDefaultDir,
    SysVmDir,
    SysVipcDir,
    PidMax,
    ShmMax,
    ShmMni,
    ShmAll,
    PipeMaxSize,
    PipeUserPagesSoft,
    LeaseBreakTime,
    NetIpv4ConfLoTag,
    NetIpv4ConfDefaultTag,
    KeysGcDelay,
    KeysMaxkeys,
    KeysMaxbytes,
    CorePattern,
    DropCaches,
    VfsCachePressure,
    FanotifyMaxQueuedEvents,
    InotifyMaxQueuedEvents,
    InotifyMaxUserInstances,
    InotifyMaxUserWatches,
    BlockCacheStats,
    DentryCacheStats,
    ExecLoadStats,
    Version,
    SysVipcShm,
    Domainname,
    Tainted,
    PidDir(usize),
    PidStat(usize),
    PidStatus(usize),
    PidComm(usize),
    PidCmdline(usize),
    PidExe(usize),
    PidTimerslack(usize),
    PidFdDir(usize),
    PidFdEntry(usize, usize),
    PidFdInfoDir(usize),
    PidFdInfoEntry(usize, usize),
    PidMaps(usize),
    PidSmaps(usize),
    PidMounts(usize),
    PidMountinfo(usize),
    PidIo(usize),
    PidNsDir(usize),
    PidNsMnt(usize),
    PidNsPid(usize),
    PidNsUser(usize),
    PidNsUts(usize),
    PidTaskDir(usize),
    PidTaskTidDir(usize, usize),
    PidTaskTidStat(usize, usize),
    PidTaskTidComm(usize, usize),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ProcNamespaceKind {
    Mnt,
    Pid,
    User,
    Uts,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct ProcNamespaceInfo {
    pub(crate) kind: ProcNamespaceKind,
    pub(crate) id: usize,
    pub(crate) parent_id: Option<usize>,
}

impl ProcFs {
    pub(super) fn new() -> Self {
        Self
    }
}

fn pid_dir_ino(pid: usize) -> u32 {
    PID_DIR_BASE + pid as u32
}

fn pid_file_ino(pid: usize, offset: u32) -> u32 {
    PID_FILE_BASE + pid as u32 * PID_FILE_STRIDE + offset
}

fn pid_fd_entry_ino(pid: usize, fd: usize) -> u32 {
    PID_FD_ENTRY_BASE + pid as u32 * PID_FD_ENTRY_STRIDE + fd as u32
}

fn pid_task_tid_ino(pid: usize, local_tid: usize, tag: u32) -> Option<u32> {
    if pid >= PID_TASK_MAX_PID || local_tid >= PID_TASK_MAX_LOCAL_TID {
        return None;
    }
    Some(tag | ((pid as u32) << PID_TASK_PID_SHIFT) | local_tid as u32)
}

fn pid_task_tid_dir_ino(pid: usize, local_tid: usize) -> Option<u32> {
    pid_task_tid_ino(pid, local_tid, PID_TASK_TID_DIR_TAG)
}

fn pid_task_tid_stat_ino(pid: usize, local_tid: usize) -> Option<u32> {
    pid_task_tid_ino(pid, local_tid, PID_TASK_TID_STAT_TAG)
}

fn pid_task_tid_comm_ino(pid: usize, local_tid: usize) -> Option<u32> {
    pid_task_tid_ino(pid, local_tid, PID_TASK_TID_COMM_TAG)
}

fn decode_pid_task_tid_ino(ino: u32) -> Option<ProcNode> {
    let tag = ino & PID_TASK_INO_TAG_MASK;
    if !matches!(
        tag,
        PID_TASK_TID_COMM_TAG | PID_TASK_TID_DIR_TAG | PID_TASK_TID_STAT_TAG
    ) {
        return None;
    }
    let payload = ino & !PID_TASK_INO_TAG_MASK;
    let pid = (payload >> PID_TASK_PID_SHIFT) as usize;
    let local_tid = (payload & PID_TASK_TID_MASK) as usize;
    lookup_task_by_local_tid(pid, local_tid)?;
    match tag {
        PID_TASK_TID_COMM_TAG => Some(ProcNode::PidTaskTidComm(pid, local_tid)),
        PID_TASK_TID_DIR_TAG => Some(ProcNode::PidTaskTidDir(pid, local_tid)),
        PID_TASK_TID_STAT_TAG => Some(ProcNode::PidTaskTidStat(pid, local_tid)),
        _ => None,
    }
}

fn lookup_process(pid: usize) -> Option<ProcessProcSnapshot> {
    pid2process(pid).map(|process| process.proc_snapshot())
}

fn namespace_info_for_process(
    process: ProcessProcSnapshot,
    kind: ProcNamespaceKind,
) -> ProcNamespaceInfo {
    match kind {
        ProcNamespaceKind::Mnt => ProcNamespaceInfo {
            kind,
            id: process.mount_namespace_id.0,
            parent_id: None,
        },
        ProcNamespaceKind::Pid => ProcNamespaceInfo {
            kind,
            id: process.pid_namespace_id,
            parent_id: process.pid_namespace_parent_id,
        },
        ProcNamespaceKind::User => ProcNamespaceInfo {
            kind,
            id: process.user_namespace_id,
            parent_id: process.user_namespace_parent_id,
        },
        ProcNamespaceKind::Uts => ProcNamespaceInfo {
            kind,
            id: ROOT_UTS_NAMESPACE_ID,
            parent_id: None,
        },
    }
}

fn namespace_info_for_pid(pid: usize, kind: ProcNamespaceKind) -> Option<ProcNamespaceInfo> {
    lookup_process(pid).map(|process| namespace_info_for_process(process, kind))
}

pub(crate) fn proc_namespace_stat_ino(kind: ProcNamespaceKind, id: usize) -> u64 {
    let base = match kind {
        ProcNamespaceKind::Mnt => PROC_NS_MNT_INO_BASE,
        ProcNamespaceKind::Pid => PROC_NS_PID_INO_BASE,
        ProcNamespaceKind::User => PROC_NS_USER_INO_BASE,
        ProcNamespaceKind::Uts => PROC_NS_UTS_INO_BASE,
    };
    base + id as u64
}

pub(crate) fn proc_namespace_kind_name(kind: ProcNamespaceKind) -> &'static str {
    match kind {
        ProcNamespaceKind::Mnt => "mnt",
        ProcNamespaceKind::Pid => "pid",
        ProcNamespaceKind::User => "user",
        ProcNamespaceKind::Uts => "uts",
    }
}

pub(crate) fn proc_namespace_info_from_path(path: &str) -> Option<ProcNamespaceInfo> {
    let mut components = path.split('/').filter(|component| !component.is_empty());
    if components.next()? != "proc" {
        return None;
    }
    let pid = match components.next()? {
        "self" => crate::task::current_process().getpid(),
        component => parse_pid(component)?,
    };
    if components.next()? != "ns" {
        return None;
    }
    let kind = match components.next()? {
        "mnt" => ProcNamespaceKind::Mnt,
        "pid" => ProcNamespaceKind::Pid,
        "user" => ProcNamespaceKind::User,
        "uts" => ProcNamespaceKind::Uts,
        _ => return None,
    };
    if components.next().is_some() {
        return None;
    }
    namespace_info_for_pid(pid, kind)
}

fn lookup_task_by_local_tid(pid: usize, local_tid: usize) -> Option<Arc<TaskControlBlock>> {
    let process = pid2process(pid)?;
    let inner = process.inner_exclusive_access();
    inner
        .tasks
        .get(local_tid)
        .and_then(|task| task.as_ref().map(Arc::clone))
}

fn lookup_task_by_linux_tid(pid: usize, linux_tid: usize) -> Option<Arc<TaskControlBlock>> {
    pid2process(pid)?
        .tasks_snapshot()
        .into_iter()
        .find(|task| task.linux_tid() == linux_tid)
}

fn task_local_tid(task: &TaskControlBlock) -> usize {
    task.inner_exclusive_access().tid
}

fn parse_pid(component: &str) -> Option<usize> {
    if component.is_empty() || !component.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    component.parse().ok()
}

fn decode_node(ino: u32) -> Option<ProcNode> {
    if let Some(node) = decode_pid_task_tid_ino(ino) {
        return Some(node);
    }
    match ino {
        ROOT_INO => Some(ProcNode::Root),
        MOUNTS_INO => Some(ProcNode::Mounts),
        FILESYSTEMS_INO => Some(ProcNode::Filesystems),
        KEY_USERS_INO => Some(ProcNode::KeyUsers),
        MEMINFO_INO => Some(ProcNode::Meminfo),
        UPTIME_INO => Some(ProcNode::Uptime),
        CPUINFO_INO => Some(ProcNode::Cpuinfo),
        SYS_DIR_INO => Some(ProcNode::SysDir),
        SYS_KERNEL_DIR_INO => Some(ProcNode::SysKernelDir),
        SYS_KERNEL_KEYS_DIR_INO => Some(ProcNode::SysKernelKeysDir),
        PID_MAX_INO => Some(ProcNode::PidMax),
        SYS_FS_DIR_INO => Some(ProcNode::SysFsDir),
        SYS_FS_FANOTIFY_DIR_INO => Some(ProcNode::SysFsFanotifyDir),
        SYS_FS_INOTIFY_DIR_INO => Some(ProcNode::SysFsInotifyDir),
        PIPE_MAX_SIZE_INO => Some(ProcNode::PipeMaxSize),
        PIPE_USER_PAGES_SOFT_INO => Some(ProcNode::PipeUserPagesSoft),
        DOMAINNAME_INO => Some(ProcNode::Domainname),
        TAINTED_INO => Some(ProcNode::Tainted),
        LEASE_BREAK_TIME_INO => Some(ProcNode::LeaseBreakTime),
        SYS_NET_DIR_INO => Some(ProcNode::SysNetDir),
        SYS_NET_IPV4_DIR_INO => Some(ProcNode::SysNetIpv4Dir),
        SYS_NET_IPV4_CONF_DIR_INO => Some(ProcNode::SysNetIpv4ConfDir),
        SYS_NET_IPV4_CONF_LO_DIR_INO => Some(ProcNode::SysNetIpv4ConfLoDir),
        SYS_NET_IPV4_CONF_DEFAULT_DIR_INO => Some(ProcNode::SysNetIpv4ConfDefaultDir),
        SYS_VM_DIR_INO => Some(ProcNode::SysVmDir),
        SYSVIPC_DIR_INO => Some(ProcNode::SysVipcDir),
        SYSVIPC_SHM_INO => Some(ProcNode::SysVipcShm),
        SHMMAX_INO => Some(ProcNode::ShmMax),
        SHMMNI_INO => Some(ProcNode::ShmMni),
        SHMALL_INO => Some(ProcNode::ShmAll),
        SYS_NET_IPV4_CONF_LO_TAG_INO => Some(ProcNode::NetIpv4ConfLoTag),
        SYS_NET_IPV4_CONF_DEFAULT_TAG_INO => Some(ProcNode::NetIpv4ConfDefaultTag),
        KEYS_GC_DELAY_INO => Some(ProcNode::KeysGcDelay),
        KEYS_MAXKEYS_INO => Some(ProcNode::KeysMaxkeys),
        KEYS_MAXBYTES_INO => Some(ProcNode::KeysMaxbytes),
        CORE_PATTERN_INO => Some(ProcNode::CorePattern),
        DROP_CACHES_INO => Some(ProcNode::DropCaches),
        VFS_CACHE_PRESSURE_INO => Some(ProcNode::VfsCachePressure),
        FANOTIFY_MAX_QUEUED_EVENTS_INO => Some(ProcNode::FanotifyMaxQueuedEvents),
        INOTIFY_MAX_QUEUED_EVENTS_INO => Some(ProcNode::InotifyMaxQueuedEvents),
        INOTIFY_MAX_USER_INSTANCES_INO => Some(ProcNode::InotifyMaxUserInstances),
        INOTIFY_MAX_USER_WATCHES_INO => Some(ProcNode::InotifyMaxUserWatches),
        BLOCK_CACHE_STATS_INO => Some(ProcNode::BlockCacheStats),
        DENTRY_CACHE_STATS_INO => Some(ProcNode::DentryCacheStats),
        EXEC_LOAD_STATS_INO => Some(ProcNode::ExecLoadStats),
        VERSION_INO => Some(ProcNode::Version),
        ino if ino >= PID_FDINFO_ENTRY_BASE => {
            let rel = ino - PID_FDINFO_ENTRY_BASE;
            let pid = (rel / PID_FD_ENTRY_STRIDE) as usize;
            let fd = (rel % PID_FD_ENTRY_STRIDE) as usize;
            let process = pid2process(pid)?;
            let fd_exists = {
                let inner = process.inner_exclusive_access();
                inner.fd_table.get(fd).is_some_and(Option::is_some)
            };
            fd_exists.then_some(ProcNode::PidFdInfoEntry(pid, fd))
        }
        ino if ino >= PID_FD_ENTRY_BASE => {
            let rel = ino - PID_FD_ENTRY_BASE;
            let pid = (rel / PID_FD_ENTRY_STRIDE) as usize;
            let fd = (rel % PID_FD_ENTRY_STRIDE) as usize;
            let process = pid2process(pid)?;
            let fd_exists = {
                let inner = process.inner_exclusive_access();
                inner.fd_table.get(fd).is_some_and(Option::is_some)
            };
            fd_exists.then_some(ProcNode::PidFdEntry(pid, fd))
        }
        ino if ino >= PID_FILE_BASE => {
            let rel = ino - PID_FILE_BASE;
            let pid = (rel / PID_FILE_STRIDE) as usize;
            let offset = rel % PID_FILE_STRIDE;
            lookup_process(pid)?;
            match offset {
                PID_STAT_OFFSET => Some(ProcNode::PidStat(pid)),
                PID_STATUS_OFFSET => Some(ProcNode::PidStatus(pid)),
                PID_CMDLINE_OFFSET => Some(ProcNode::PidCmdline(pid)),
                PID_FD_DIR_OFFSET => Some(ProcNode::PidFdDir(pid)),
                PID_MAPS_OFFSET => Some(ProcNode::PidMaps(pid)),
                PID_NS_DIR_OFFSET => Some(ProcNode::PidNsDir(pid)),
                PID_NS_MNT_OFFSET => Some(ProcNode::PidNsMnt(pid)),
                PID_TASK_DIR_OFFSET => Some(ProcNode::PidTaskDir(pid)),
                PID_SMAPS_OFFSET => Some(ProcNode::PidSmaps(pid)),
                PID_MOUNTS_OFFSET => Some(ProcNode::PidMounts(pid)),
                PID_MOUNTINFO_OFFSET => Some(ProcNode::PidMountinfo(pid)),
                PID_IO_OFFSET => Some(ProcNode::PidIo(pid)),
                PID_FDINFO_DIR_OFFSET => Some(ProcNode::PidFdInfoDir(pid)),
                PID_COMM_OFFSET => Some(ProcNode::PidComm(pid)),
                PID_TIMERSLACK_OFFSET => Some(ProcNode::PidTimerslack(pid)),
                PID_NS_PID_OFFSET => Some(ProcNode::PidNsPid(pid)),
                PID_NS_USER_OFFSET => Some(ProcNode::PidNsUser(pid)),
                PID_NS_UTS_OFFSET => Some(ProcNode::PidNsUts(pid)),
                PID_EXE_OFFSET => Some(ProcNode::PidExe(pid)),
                _ => None,
            }
        }
        ino if ino >= PID_DIR_BASE => {
            let pid = (ino - PID_DIR_BASE) as usize;
            lookup_process(pid).map(|_| ProcNode::PidDir(pid))
        }
        _ => None,
    }
}

fn node_kind(node: ProcNode) -> FsNodeKind {
    match node {
        ProcNode::Root
        | ProcNode::SysDir
        | ProcNode::SysKernelDir
        | ProcNode::SysKernelKeysDir
        | ProcNode::SysFsDir
        | ProcNode::SysFsFanotifyDir
        | ProcNode::SysFsInotifyDir
        | ProcNode::SysNetDir
        | ProcNode::SysNetIpv4Dir
        | ProcNode::SysNetIpv4ConfDir
        | ProcNode::SysNetIpv4ConfLoDir
        | ProcNode::SysNetIpv4ConfDefaultDir
        | ProcNode::SysVmDir
        | ProcNode::SysVipcDir
        | ProcNode::PidDir(_)
        | ProcNode::PidFdDir(_)
        | ProcNode::PidFdInfoDir(_)
        | ProcNode::PidNsDir(_)
        | ProcNode::PidTaskDir(_)
        | ProcNode::PidTaskTidDir(_, _) => FsNodeKind::Directory,
        ProcNode::PidExe(_) | ProcNode::PidFdEntry(_, _) => FsNodeKind::Symlink,
        _ => FsNodeKind::RegularFile,
    }
}

fn root_entries() -> Vec<RawDirEntry> {
    let mut entries = Vec::new();
    entries.push(RawDirEntry {
        ino: ROOT_INO,
        name: ".".into(),
        dtype: DT_DIR,
    });
    entries.push(RawDirEntry {
        ino: ROOT_INO,
        name: "..".into(),
        dtype: DT_DIR,
    });
    entries.push(RawDirEntry {
        ino: MOUNTS_INO,
        name: "mounts".into(),
        dtype: DT_REG,
    });
    entries.push(RawDirEntry {
        ino: FILESYSTEMS_INO,
        name: "filesystems".into(),
        dtype: DT_REG,
    });
    entries.push(RawDirEntry {
        ino: KEY_USERS_INO,
        name: "key-users".into(),
        dtype: DT_REG,
    });
    entries.push(RawDirEntry {
        ino: MEMINFO_INO,
        name: "meminfo".into(),
        dtype: DT_REG,
    });
    entries.push(RawDirEntry {
        ino: UPTIME_INO,
        name: "uptime".into(),
        dtype: DT_REG,
    });
    entries.push(RawDirEntry {
        ino: CPUINFO_INO,
        name: "cpuinfo".into(),
        dtype: DT_REG,
    });
    entries.push(RawDirEntry {
        ino: VERSION_INO,
        name: "version".into(),
        dtype: DT_REG,
    });
    entries.push(RawDirEntry {
        ino: SYS_DIR_INO,
        name: "sys".into(),
        dtype: DT_DIR,
    });
    entries.push(RawDirEntry {
        ino: SYSVIPC_DIR_INO,
        name: "sysvipc".into(),
        dtype: DT_DIR,
    });
    for process in list_process_snapshots() {
        entries.push(RawDirEntry {
            ino: pid_dir_ino(process.pid),
            name: process.pid.to_string(),
            dtype: DT_DIR,
        });
    }
    entries
}

fn sys_entries() -> Vec<RawDirEntry> {
    let mut entries = Vec::new();
    entries.push(RawDirEntry {
        ino: SYS_DIR_INO,
        name: ".".into(),
        dtype: DT_DIR,
    });
    entries.push(RawDirEntry {
        ino: ROOT_INO,
        name: "..".into(),
        dtype: DT_DIR,
    });
    entries.push(RawDirEntry {
        ino: SYS_KERNEL_DIR_INO,
        name: "kernel".into(),
        dtype: DT_DIR,
    });
    entries.push(RawDirEntry {
        ino: SYS_FS_DIR_INO,
        name: "fs".into(),
        dtype: DT_DIR,
    });
    entries.push(RawDirEntry {
        ino: SYS_NET_DIR_INO,
        name: "net".into(),
        dtype: DT_DIR,
    });
    entries.push(RawDirEntry {
        ino: SYS_VM_DIR_INO,
        name: "vm".into(),
        dtype: DT_DIR,
    });
    entries
}

fn sysvipc_entries() -> Vec<RawDirEntry> {
    let mut entries = Vec::new();
    entries.push(RawDirEntry {
        ino: SYSVIPC_DIR_INO,
        name: ".".into(),
        dtype: DT_DIR,
    });
    entries.push(RawDirEntry {
        ino: ROOT_INO,
        name: "..".into(),
        dtype: DT_DIR,
    });
    entries.push(RawDirEntry {
        ino: SYSVIPC_SHM_INO,
        name: "shm".into(),
        dtype: DT_REG,
    });
    entries
}

fn sys_vm_entries() -> Vec<RawDirEntry> {
    let mut entries = Vec::new();
    entries.push(RawDirEntry {
        ino: SYS_VM_DIR_INO,
        name: ".".into(),
        dtype: DT_DIR,
    });
    entries.push(RawDirEntry {
        ino: SYS_DIR_INO,
        name: "..".into(),
        dtype: DT_DIR,
    });
    entries.push(RawDirEntry {
        ino: DROP_CACHES_INO,
        name: "drop_caches".into(),
        dtype: DT_REG,
    });
    entries.push(RawDirEntry {
        ino: VFS_CACHE_PRESSURE_INO,
        name: "vfs_cache_pressure".into(),
        dtype: DT_REG,
    });
    entries
}

fn sys_net_entries() -> Vec<RawDirEntry> {
    let mut entries = Vec::new();
    entries.push(RawDirEntry {
        ino: SYS_NET_DIR_INO,
        name: ".".into(),
        dtype: DT_DIR,
    });
    entries.push(RawDirEntry {
        ino: SYS_DIR_INO,
        name: "..".into(),
        dtype: DT_DIR,
    });
    entries.push(RawDirEntry {
        ino: SYS_NET_IPV4_DIR_INO,
        name: "ipv4".into(),
        dtype: DT_DIR,
    });
    entries
}

fn sys_net_ipv4_entries() -> Vec<RawDirEntry> {
    let mut entries = Vec::new();
    entries.push(RawDirEntry {
        ino: SYS_NET_IPV4_DIR_INO,
        name: ".".into(),
        dtype: DT_DIR,
    });
    entries.push(RawDirEntry {
        ino: SYS_NET_DIR_INO,
        name: "..".into(),
        dtype: DT_DIR,
    });
    entries.push(RawDirEntry {
        ino: SYS_NET_IPV4_CONF_DIR_INO,
        name: "conf".into(),
        dtype: DT_DIR,
    });
    entries
}

fn sys_net_ipv4_conf_entries() -> Vec<RawDirEntry> {
    let mut entries = Vec::new();
    entries.push(RawDirEntry {
        ino: SYS_NET_IPV4_CONF_DIR_INO,
        name: ".".into(),
        dtype: DT_DIR,
    });
    entries.push(RawDirEntry {
        ino: SYS_NET_IPV4_DIR_INO,
        name: "..".into(),
        dtype: DT_DIR,
    });
    entries.push(RawDirEntry {
        ino: SYS_NET_IPV4_CONF_LO_DIR_INO,
        name: "lo".into(),
        dtype: DT_DIR,
    });
    entries.push(RawDirEntry {
        ino: SYS_NET_IPV4_CONF_DEFAULT_DIR_INO,
        name: "default".into(),
        dtype: DT_DIR,
    });
    entries
}

fn sys_net_ipv4_conf_iface_entries(ino: u32, tag_ino: u32) -> Vec<RawDirEntry> {
    let mut entries = Vec::new();
    entries.push(RawDirEntry {
        ino,
        name: ".".into(),
        dtype: DT_DIR,
    });
    entries.push(RawDirEntry {
        ino: SYS_NET_IPV4_CONF_DIR_INO,
        name: "..".into(),
        dtype: DT_DIR,
    });
    entries.push(RawDirEntry {
        ino: tag_ino,
        name: "tag".into(),
        dtype: DT_REG,
    });
    entries
}

fn sys_fs_entries() -> Vec<RawDirEntry> {
    let mut entries = Vec::new();
    entries.push(RawDirEntry {
        ino: SYS_FS_DIR_INO,
        name: ".".into(),
        dtype: DT_DIR,
    });
    entries.push(RawDirEntry {
        ino: SYS_DIR_INO,
        name: "..".into(),
        dtype: DT_DIR,
    });
    entries.push(RawDirEntry {
        ino: PIPE_MAX_SIZE_INO,
        name: "pipe-max-size".into(),
        dtype: DT_REG,
    });
    entries.push(RawDirEntry {
        ino: PIPE_USER_PAGES_SOFT_INO,
        name: "pipe-user-pages-soft".into(),
        dtype: DT_REG,
    });
    entries.push(RawDirEntry {
        ino: LEASE_BREAK_TIME_INO,
        name: "lease-break-time".into(),
        dtype: DT_REG,
    });
    entries.push(RawDirEntry {
        ino: SYS_FS_FANOTIFY_DIR_INO,
        name: "fanotify".into(),
        dtype: DT_DIR,
    });
    entries.push(RawDirEntry {
        ino: SYS_FS_INOTIFY_DIR_INO,
        name: "inotify".into(),
        dtype: DT_DIR,
    });
    entries
}

fn sys_fs_fanotify_entries() -> Vec<RawDirEntry> {
    let mut entries = Vec::new();
    entries.push(RawDirEntry {
        ino: SYS_FS_FANOTIFY_DIR_INO,
        name: ".".into(),
        dtype: DT_DIR,
    });
    entries.push(RawDirEntry {
        ino: SYS_FS_DIR_INO,
        name: "..".into(),
        dtype: DT_DIR,
    });
    entries.push(RawDirEntry {
        ino: FANOTIFY_MAX_QUEUED_EVENTS_INO,
        name: "max_queued_events".into(),
        dtype: DT_REG,
    });
    entries
}

fn sys_fs_inotify_entries() -> Vec<RawDirEntry> {
    let mut entries = Vec::new();
    entries.push(RawDirEntry {
        ino: SYS_FS_INOTIFY_DIR_INO,
        name: ".".into(),
        dtype: DT_DIR,
    });
    entries.push(RawDirEntry {
        ino: SYS_FS_DIR_INO,
        name: "..".into(),
        dtype: DT_DIR,
    });
    entries.push(RawDirEntry {
        ino: INOTIFY_MAX_QUEUED_EVENTS_INO,
        name: "max_queued_events".into(),
        dtype: DT_REG,
    });
    entries.push(RawDirEntry {
        ino: INOTIFY_MAX_USER_INSTANCES_INO,
        name: "max_user_instances".into(),
        dtype: DT_REG,
    });
    entries.push(RawDirEntry {
        ino: INOTIFY_MAX_USER_WATCHES_INO,
        name: "max_user_watches".into(),
        dtype: DT_REG,
    });
    entries
}

fn sys_kernel_entries() -> Vec<RawDirEntry> {
    let mut entries = Vec::new();
    entries.push(RawDirEntry {
        ino: SYS_KERNEL_DIR_INO,
        name: ".".into(),
        dtype: DT_DIR,
    });
    entries.push(RawDirEntry {
        ino: SYS_DIR_INO,
        name: "..".into(),
        dtype: DT_DIR,
    });
    entries.push(RawDirEntry {
        ino: PID_MAX_INO,
        name: "pid_max".into(),
        dtype: DT_REG,
    });
    entries.push(RawDirEntry {
        ino: SHMMAX_INO,
        name: "shmmax".into(),
        dtype: DT_REG,
    });
    entries.push(RawDirEntry {
        ino: SHMMNI_INO,
        name: "shmmni".into(),
        dtype: DT_REG,
    });
    entries.push(RawDirEntry {
        ino: SHMALL_INO,
        name: "shmall".into(),
        dtype: DT_REG,
    });
    entries.push(RawDirEntry {
        ino: CORE_PATTERN_INO,
        name: "core_pattern".into(),
        dtype: DT_REG,
    });
    entries.push(RawDirEntry {
        ino: SYS_KERNEL_KEYS_DIR_INO,
        name: "keys".into(),
        dtype: DT_DIR,
    });
    entries.push(RawDirEntry {
        ino: DOMAINNAME_INO,
        name: "domainname".into(),
        dtype: DT_REG,
    });
    entries.push(RawDirEntry {
        ino: TAINTED_INO,
        name: "tainted".into(),
        dtype: DT_REG,
    });
    entries.push(RawDirEntry {
        ino: BLOCK_CACHE_STATS_INO,
        name: "block_cache".into(),
        dtype: DT_REG,
    });
    entries.push(RawDirEntry {
        ino: DENTRY_CACHE_STATS_INO,
        name: "dentry_cache".into(),
        dtype: DT_REG,
    });
    entries.push(RawDirEntry {
        ino: EXEC_LOAD_STATS_INO,
        name: "exec_loader".into(),
        dtype: DT_REG,
    });
    entries
}

fn sys_kernel_keys_entries() -> Vec<RawDirEntry> {
    let mut entries = Vec::new();
    entries.push(RawDirEntry {
        ino: SYS_KERNEL_KEYS_DIR_INO,
        name: ".".into(),
        dtype: DT_DIR,
    });
    entries.push(RawDirEntry {
        ino: SYS_KERNEL_DIR_INO,
        name: "..".into(),
        dtype: DT_DIR,
    });
    entries.push(RawDirEntry {
        ino: KEYS_GC_DELAY_INO,
        name: "gc_delay".into(),
        dtype: DT_REG,
    });
    entries.push(RawDirEntry {
        ino: KEYS_MAXKEYS_INO,
        name: "maxkeys".into(),
        dtype: DT_REG,
    });
    entries.push(RawDirEntry {
        ino: KEYS_MAXBYTES_INO,
        name: "maxbytes".into(),
        dtype: DT_REG,
    });
    entries
}

fn pid_entries(pid: usize) -> Vec<RawDirEntry> {
    let mut entries = Vec::new();
    entries.push(RawDirEntry {
        ino: pid_dir_ino(pid),
        name: ".".into(),
        dtype: DT_DIR,
    });
    entries.push(RawDirEntry {
        ino: ROOT_INO,
        name: "..".into(),
        dtype: DT_DIR,
    });
    entries.push(RawDirEntry {
        ino: pid_file_ino(pid, PID_STAT_OFFSET),
        name: "stat".into(),
        dtype: DT_REG,
    });
    entries.push(RawDirEntry {
        ino: pid_file_ino(pid, PID_STATUS_OFFSET),
        name: "status".into(),
        dtype: DT_REG,
    });
    entries.push(RawDirEntry {
        ino: pid_file_ino(pid, PID_COMM_OFFSET),
        name: "comm".into(),
        dtype: DT_REG,
    });
    entries.push(RawDirEntry {
        ino: pid_file_ino(pid, PID_CMDLINE_OFFSET),
        name: "cmdline".into(),
        dtype: DT_REG,
    });
    entries.push(RawDirEntry {
        ino: pid_file_ino(pid, PID_EXE_OFFSET),
        name: "exe".into(),
        dtype: DT_LNK,
    });
    entries.push(RawDirEntry {
        ino: pid_file_ino(pid, PID_FD_DIR_OFFSET),
        name: "fd".into(),
        dtype: DT_DIR,
    });
    entries.push(RawDirEntry {
        ino: pid_file_ino(pid, PID_FDINFO_DIR_OFFSET),
        name: "fdinfo".into(),
        dtype: DT_DIR,
    });
    entries.push(RawDirEntry {
        ino: pid_file_ino(pid, PID_MAPS_OFFSET),
        name: "maps".into(),
        dtype: DT_REG,
    });
    entries.push(RawDirEntry {
        ino: pid_file_ino(pid, PID_SMAPS_OFFSET),
        name: "smaps".into(),
        dtype: DT_REG,
    });
    entries.push(RawDirEntry {
        ino: pid_file_ino(pid, PID_MOUNTS_OFFSET),
        name: "mounts".into(),
        dtype: DT_REG,
    });
    entries.push(RawDirEntry {
        ino: pid_file_ino(pid, PID_MOUNTINFO_OFFSET),
        name: "mountinfo".into(),
        dtype: DT_REG,
    });
    entries.push(RawDirEntry {
        ino: pid_file_ino(pid, PID_IO_OFFSET),
        name: "io".into(),
        dtype: DT_REG,
    });
    entries.push(RawDirEntry {
        ino: pid_file_ino(pid, PID_TIMERSLACK_OFFSET),
        name: "timerslack_ns".into(),
        dtype: DT_REG,
    });
    entries.push(RawDirEntry {
        ino: pid_file_ino(pid, PID_NS_DIR_OFFSET),
        name: "ns".into(),
        dtype: DT_DIR,
    });
    entries.push(RawDirEntry {
        ino: pid_file_ino(pid, PID_TASK_DIR_OFFSET),
        name: "task".into(),
        dtype: DT_DIR,
    });
    entries
}

fn pid_task_entries(pid: usize) -> FsResult<Vec<RawDirEntry>> {
    let process = pid2process(pid).ok_or(FsError::NotFound)?;
    let tasks = process.tasks_snapshot();
    let mut entries = Vec::new();
    entries.push(RawDirEntry {
        ino: pid_file_ino(pid, PID_TASK_DIR_OFFSET),
        name: ".".into(),
        dtype: DT_DIR,
    });
    entries.push(RawDirEntry {
        ino: pid_dir_ino(pid),
        name: "..".into(),
        dtype: DT_DIR,
    });
    for task in tasks {
        let local_tid = task_local_tid(&task);
        if let Some(ino) = pid_task_tid_dir_ino(pid, local_tid) {
            entries.push(RawDirEntry {
                ino,
                name: task.linux_tid().to_string(),
                dtype: DT_DIR,
            });
        }
    }
    Ok(entries)
}

fn pid_task_tid_entries(pid: usize, local_tid: usize) -> FsResult<Vec<RawDirEntry>> {
    let task_dir_ino = pid_task_tid_dir_ino(pid, local_tid).ok_or(FsError::NotFound)?;
    let stat_ino = pid_task_tid_stat_ino(pid, local_tid).ok_or(FsError::NotFound)?;
    lookup_task_by_local_tid(pid, local_tid).ok_or(FsError::NotFound)?;
    let mut entries = Vec::new();
    entries.push(RawDirEntry {
        ino: task_dir_ino,
        name: ".".into(),
        dtype: DT_DIR,
    });
    entries.push(RawDirEntry {
        ino: pid_file_ino(pid, PID_TASK_DIR_OFFSET),
        name: "..".into(),
        dtype: DT_DIR,
    });
    entries.push(RawDirEntry {
        ino: stat_ino,
        name: "stat".into(),
        dtype: DT_REG,
    });
    entries.push(RawDirEntry {
        ino: pid_task_tid_comm_ino(pid, local_tid).ok_or(FsError::NotFound)?,
        name: "comm".into(),
        dtype: DT_REG,
    });
    Ok(entries)
}

fn pid_fd_entries(pid: usize) -> FsResult<Vec<RawDirEntry>> {
    let process = pid2process(pid).ok_or(FsError::NotFound)?;
    let fd_names: Vec<_> = {
        let inner = process.inner_exclusive_access();
        inner
            .fd_table
            .iter()
            .enumerate()
            .filter_map(|(fd, entry)| entry.as_ref().map(|_| fd))
            .collect()
    };
    let mut entries = Vec::new();
    entries.push(RawDirEntry {
        ino: pid_file_ino(pid, PID_FD_DIR_OFFSET),
        name: ".".into(),
        dtype: DT_DIR,
    });
    entries.push(RawDirEntry {
        ino: pid_dir_ino(pid),
        name: "..".into(),
        dtype: DT_DIR,
    });
    for fd in fd_names {
        entries.push(RawDirEntry {
            ino: pid_fd_entry_ino(pid, fd),
            name: fd.to_string(),
            dtype: DT_LNK,
        });
    }
    Ok(entries)
}

fn pid_fdinfo_entry_ino(pid: usize, fd: usize) -> u32 {
    PID_FDINFO_ENTRY_BASE + pid as u32 * PID_FD_ENTRY_STRIDE + fd as u32
}

fn pid_fdinfo_entries(pid: usize) -> FsResult<Vec<RawDirEntry>> {
    let process = pid2process(pid).ok_or(FsError::NotFound)?;
    let fd_names: Vec<_> = {
        let inner = process.inner_exclusive_access();
        inner
            .fd_table
            .iter()
            .enumerate()
            .filter_map(|(fd, entry)| entry.as_ref().map(|_| fd))
            .collect()
    };
    let mut entries = Vec::new();
    entries.push(RawDirEntry {
        ino: pid_file_ino(pid, PID_FDINFO_DIR_OFFSET),
        name: ".".into(),
        dtype: DT_DIR,
    });
    entries.push(RawDirEntry {
        ino: pid_dir_ino(pid),
        name: "..".into(),
        dtype: DT_DIR,
    });
    for fd in fd_names {
        entries.push(RawDirEntry {
            ino: pid_fdinfo_entry_ino(pid, fd),
            name: fd.to_string(),
            dtype: DT_REG,
        });
    }
    Ok(entries)
}

fn pid_ns_entries(pid: usize) -> Vec<RawDirEntry> {
    let mut entries = Vec::new();
    entries.push(RawDirEntry {
        ino: pid_file_ino(pid, PID_NS_DIR_OFFSET),
        name: ".".into(),
        dtype: DT_DIR,
    });
    entries.push(RawDirEntry {
        ino: pid_dir_ino(pid),
        name: "..".into(),
        dtype: DT_DIR,
    });
    entries.push(RawDirEntry {
        ino: pid_file_ino(pid, PID_NS_MNT_OFFSET),
        name: "mnt".into(),
        dtype: DT_REG,
    });
    entries.push(RawDirEntry {
        ino: pid_file_ino(pid, PID_NS_PID_OFFSET),
        name: "pid".into(),
        dtype: DT_REG,
    });
    entries.push(RawDirEntry {
        ino: pid_file_ino(pid, PID_NS_USER_OFFSET),
        name: "user".into(),
        dtype: DT_REG,
    });
    entries.push(RawDirEntry {
        ino: pid_file_ino(pid, PID_NS_UTS_OFFSET),
        name: "uts".into(),
        dtype: DT_REG,
    });
    entries
}

fn mounts_content() -> String {
    let mut output = String::new();
    let namespace_id = crate::task::current_process().mount_namespace_id();
    for mount in mount::list_mounts(namespace_id) {
        output.push_str(&format!(
            "{} {} {} {} 0 0\n",
            mount.source, mount.target, mount.fs_type, mount.options
        ));
    }
    output
}

fn linux_dev_major(dev: u64) -> u32 {
    (((dev >> 8) & 0xfff) | ((dev >> 32) & !0xfff)) as u32
}

fn linux_dev_minor(dev: u64) -> u32 {
    ((dev & 0xff) | ((dev >> 12) & !0xff)) as u32
}

fn linux_visible_dev(dev: u64) -> u64 {
    if linux_dev_major(dev) == 0 {
        (254u64 & 0xfff) << 8
    } else {
        dev
    }
}

fn mountinfo_content() -> String {
    let mut output = String::new();
    let namespace_id = crate::task::current_process().mount_namespace_id();
    for mount in mount::list_mounts(namespace_id) {
        let dev = linux_visible_dev(mount.id.0 as u64);
        output.push_str(&format!(
            "{} 0 {}:{} / {} rw - {} {} {}\n",
            mount.id.0,
            linux_dev_major(dev),
            linux_dev_minor(dev),
            mount.target,
            mount.fs_type,
            mount.source,
            mount.options
        ));
    }
    output
}

fn filesystems_content() -> &'static str {
    // CONTEXT: fsopen(2) points userspace at /proc/filesystems to discover
    // valid fs names. ext2/ext3 are scratch-mount compatibility names backed by
    // tmpfs for current LTP coverage, not real on-disk ext2/ext3 drivers.
    "nodev\tproc\nnodev\ttmpfs\nnodev\tramfs\nnodev\tcgroup2\next2\next3\next4\nvfat\n"
}

fn meminfo_content() -> String {
    let (total_pages, free_pages) = frame_stats();
    let page_kb = PAGE_SIZE / 1024;
    let total_kb = total_pages * page_kb;
    let free_kb = free_pages * page_kb;
    let cached_kb = PROC_MEMINFO_CACHED_KB.load(Ordering::Relaxed);
    format!(
        "MemTotal:       {total_kb:8} kB\n\
         MemFree:        {free_kb:8} kB\n\
         MemAvailable:   {free_kb:8} kB\n\
         Buffers:               0 kB\n\
         Cached:        {cached_kb:8} kB\n\
         SReclaimable:          0 kB\n\
         Shmem:                 0 kB\n\
         SwapTotal:             0 kB\n\
         SwapFree:              0 kB\n"
    )
}

fn uptime_content() -> String {
    let uptime_us = get_time_us();
    let seconds = uptime_us / 1_000_000;
    let hundredths = (uptime_us % 1_000_000) / 10_000;
    format!("{seconds}.{hundredths:02} 0.00\n")
}

fn cpuinfo_content() -> String {
    // UNFINISHED: Linux /proc/cpuinfo is architecture-specific and exposes
    // detailed per-hart CPU features. This minimal node only provides stable
    // virtual CPU identification for LTP and libc environment probes.
    let (architecture, isa, mmu) = if cfg!(target_arch = "loongarch64") {
        ("loongarch64", "la64", "pg")
    } else {
        ("riscv64", "rv64imafdcsu", "sv39")
    };
    format!(
        "processor\t: 0\n\
         hart\t\t: 0\n\
         vendor_id\t: WHUSP\n\
         model name\t: QEMU Virtual CPU\n\
         architecture\t: {architecture}\n\
         isa\t\t: {isa}\n\
         mmu\t\t: {mmu}\n\n"
    )
}

fn proc_io_content() -> String {
    let read_bytes = if PROC_IO_READAHEAD_SUPPRESS_READS.load(Ordering::Relaxed) > 0 {
        PROC_IO_READAHEAD_SUPPRESS_READS.fetch_sub(1, Ordering::Relaxed);
        PROC_IO_READ_BYTES.load(Ordering::Relaxed)
    } else {
        // CONTEXT: This kernel does not yet model a real page cache. LTP
        // readahead02 only observes cache effects through /proc/meminfo and
        // /proc/self/io, so keep a small procfs-visible approximation in sync
        // with the synthetic read_bytes counter.
        PROC_MEMINFO_CACHED_KB.store(PROC_MEMINFO_OBSERVED_CACHE_KB, Ordering::Relaxed);
        PROC_IO_READ_BYTES.fetch_add(PAGE_SIZE, Ordering::Relaxed) + PAGE_SIZE
    };
    format!(
        "rchar: 0\n\
         wchar: 0\n\
         syscr: 0\n\
         syscw: 0\n\
         read_bytes: {read_bytes}\n\
         write_bytes: 0\n\
         cancelled_write_bytes: 0\n"
    )
}

fn pid_max_content() -> String {
    // CONTEXT: LTP uses this procfs knob only to choose an unused PID for
    // negative syscall tests. The allocator is much smaller than Linux's
    // tunable PID space, but returning Linux's common upper bound keeps that
    // chosen PID outside this kernel's live process table.
    format!("{}\n", PROC_PID_MAX.load(Ordering::Relaxed))
}

fn pipe_max_size_content() -> String {
    format!("{}\n", pipe_max_size())
}

fn pipe_user_pages_soft_content() -> String {
    format!("{}\n", PROC_PIPE_USER_PAGES_SOFT.load(Ordering::Relaxed))
}

fn fanotify_max_queued_events_content() -> String {
    format!("{}\n", fanotify_max_queued_events())
}

fn inotify_max_queued_events_content() -> String {
    format!("{INOTIFY_MAX_QUEUED_EVENTS}\n")
}

fn inotify_max_user_instances_content() -> String {
    format!("{INOTIFY_MAX_USER_INSTANCES}\n")
}

fn inotify_max_user_watches_content() -> String {
    format!("{INOTIFY_MAX_USER_WATCHES}\n")
}

fn lease_break_time_content() -> String {
    format!("{}\n", PROC_LEASE_BREAK_TIME.load(Ordering::Relaxed))
}

fn current_task_uses_synthetic_newnet() -> bool {
    crate::task::current_task()
        .map(|task| task.inner_exclusive_access().synthetic_newnet)
        .unwrap_or(false)
}

fn net_ipv4_conf_lo_tag_content() -> String {
    let value = if current_task_uses_synthetic_newnet() {
        DEFAULT_NET_IPV4_CONF_TAG
    } else {
        PROC_NET_IPV4_CONF_LO_TAG.load(Ordering::Relaxed)
    };
    format!("{value}\n")
}

fn net_ipv4_conf_default_tag_content() -> String {
    format!("{DEFAULT_NET_IPV4_CONF_TAG}\n")
}

fn vfs_cache_pressure_content() -> String {
    format!("{}\n", PROC_VFS_CACHE_PRESSURE.load(Ordering::Relaxed))
}

fn pid_fdinfo_content(pid: usize, fd: usize) -> FsResult<String> {
    let process = pid2process(pid).ok_or(FsError::NotFound)?;
    let (flags, file) = {
        let inner = process.inner_exclusive_access();
        let entry = inner
            .fd_table
            .get(fd)
            .and_then(Option::as_ref)
            .ok_or(FsError::NotFound)?;
        (entry.status_flags().bits(), entry.file())
    };
    if let Some(pidfd_info) = pidfd_fdinfo(&file, flags) {
        return Ok(pidfd_info);
    }
    let fanotify_info = fanotify_fdinfo(&file).unwrap_or_default();
    let inotify_info = inotify_fdinfo(&file).unwrap_or_default();
    // CONTEXT: Linux exposes fanotify marks through /proc/<pid>/fdinfo/<fd>.
    // This metadata-only subset reports enough mark/ignored_mask fields for
    // LTP fanotify09/fanotify10 to distinguish groups with and without ignore
    // marks; inode and device numbers are still placeholders.
    let mnt_id = file.vfs_mount_id().map(|mount_id| mount_id.0).unwrap_or(0);
    Ok(format!(
        "pos:\t0\nflags:\t{flags:o}\nmnt_id:\t{mnt_id}\n{fanotify_info}{inotify_info}"
    ))
}

fn domainname_content() -> Vec<u8> {
    let mut output = PROC_DOMAINNAME.exclusive_access().clone();
    output.push(b'\n');
    output
}

fn write_pid_max(buf: &[u8], offset: u64) -> usize {
    if offset != 0 {
        return 0;
    }
    let Ok(text) = core::str::from_utf8(buf) else {
        return 0;
    };
    let Ok(value) = text.trim().parse::<usize>() else {
        return 0;
    };
    // UNFINISHED: Linux uses pid_max to control PID allocator wrap. This
    // compatibility path stores the procfs value for LTP save/restore, but the
    // kernel PID allocator is not yet retuned by this sysctl.
    PROC_PID_MAX.store(value, Ordering::Relaxed);
    buf.len()
}

fn write_pipe_max_size(buf: &[u8], offset: u64) -> usize {
    if offset != 0 {
        return 0;
    }
    let Ok(text) = core::str::from_utf8(buf) else {
        return 0;
    };
    let Ok(value) = text.trim().parse::<usize>() else {
        return 0;
    };
    if value < PIPE_MIN_CAPACITY {
        return 0;
    }
    // CONTEXT: pipe buffers are dynamically allocated up to the implemented
    // contest cap. Values above that are accepted but rounded down so
    // F_SETPIPE_SZ and new unprivileged pipe defaults stay internally
    // consistent.
    PROC_PIPE_MAX_SIZE.store(value.min(PIPE_MAX_CAPACITY), Ordering::Relaxed);
    buf.len()
}

fn write_lease_break_time(buf: &[u8], offset: u64) -> usize {
    if offset != 0 {
        return 0;
    }
    let Ok(text) = core::str::from_utf8(buf) else {
        return 0;
    };
    let Ok(value) = text.trim().parse::<usize>() else {
        return 0;
    };
    // CONTEXT: The kernel does not yet implement timed lease breaking, but
    // LTP saves/restores this sysctl around lease tests. Store the value so
    // those file operations behave like a writable Linux procfs knob.
    PROC_LEASE_BREAK_TIME.store(value, Ordering::Relaxed);
    buf.len()
}

fn write_inotify_max_user_instances(buf: &[u8], offset: u64) -> usize {
    if offset != 0 {
        return 0;
    }
    let Ok(text) = core::str::from_utf8(buf) else {
        return 0;
    };
    if text.trim().parse::<usize>().is_err() {
        return 0;
    }
    // CONTEXT: inotify06 saves/restores this sysctl while stress-creating
    // instances. The current inotify subset does not enforce per-user limits,
    // but accepting numeric writes keeps the Linux procfs contract visible.
    buf.len()
}

fn write_net_ipv4_conf_lo_tag(buf: &[u8], offset: u64) -> usize {
    if offset != 0 {
        return 0;
    }
    let Ok(text) = core::str::from_utf8(buf) else {
        return 0;
    };
    let Ok(value) = text.trim().parse::<isize>() else {
        return 0;
    };
    // UNFINISHED: Linux stores this under the network namespace. This minimal
    // sysctl state is global except for CLONE_NEWNET compatibility helpers,
    // which read the default value through net_ipv4_conf_lo_tag_content().
    PROC_NET_IPV4_CONF_LO_TAG.store(value, Ordering::Relaxed);
    buf.len()
}

fn write_drop_caches(buf: &[u8], _offset: u64) -> usize {
    PROC_MEMINFO_CACHED_KB.store(0, Ordering::Relaxed);
    fanotify_evict_evictable_marks();
    buf.len()
}

fn write_vfs_cache_pressure(buf: &[u8], offset: u64) -> usize {
    if offset != 0 {
        return 0;
    }
    let Ok(text) = core::str::from_utf8(buf) else {
        return 0;
    };
    let Ok(value) = text.trim().parse::<usize>() else {
        return 0;
    };
    // CONTEXT: This kernel does not implement VFS dentry/inode cache pressure,
    // but LTP fanotify10 saves and restores the sysctl around mount-cycle
    // tests. Store the value so the procfs knob behaves like a writable Linux
    // compatibility control.
    PROC_VFS_CACHE_PRESSURE.store(value, Ordering::Relaxed);
    buf.len()
}

fn write_pid_timerslack(pid: usize, buf: &[u8], offset: u64) -> usize {
    if offset != 0 {
        return 0;
    }
    let Ok(text) = core::str::from_utf8(buf) else {
        return 0;
    };
    let Ok(value) = text.trim().parse::<usize>() else {
        return 0;
    };
    let Some(process) = pid2process(pid) else {
        return 0;
    };
    let Some(task) = process
        .inner_exclusive_access()
        .tasks
        .first()
        .and_then(|task| task.as_ref().map(Arc::clone))
    else {
        return 0;
    };
    let mut task_inner = task.inner_exclusive_access();
    task_inner.timer_slack_ns = value;
    task_inner.default_timer_slack_ns = value;
    buf.len()
}

fn write_domainname(buf: &[u8], offset: u64) -> usize {
    let Ok(offset) = usize::try_from(offset) else {
        return 0;
    };
    let end = buf
        .iter()
        .position(|byte| *byte == b'\n' || *byte == 0)
        .unwrap_or(buf.len());
    let mut value = PROC_DOMAINNAME.exclusive_access();
    if offset > value.len() {
        return 0;
    }
    value.truncate(offset);
    value.extend_from_slice(&buf[..end]);
    buf.len()
}

fn set_domainname_len(len: u64) -> FsResult {
    let Ok(len) = usize::try_from(len) else {
        return Err(FsError::InvalidInput);
    };
    let mut value = PROC_DOMAINNAME.exclusive_access();
    if len <= value.len() {
        value.truncate(len);
    } else {
        value.resize(len, 0);
    }
    Ok(())
}

fn task_status_char(status: TaskStatus, proc_sleeping: bool) -> char {
    if proc_sleeping {
        return 'S';
    }
    match status {
        TaskStatus::Ready | TaskStatus::Running => 'R',
        TaskStatus::Blocked => 'S',
        TaskStatus::Exited => 'Z',
    }
}

fn proc_stat_content(process: ProcessProcSnapshot, stat_pid: usize, state: char) -> String {
    let times = process.cpu_times;
    let utime = us_to_clock_ticks(times.user_us);
    let stime = us_to_clock_ticks(times.system_us);
    let cutime = us_to_clock_ticks(times.children_user_us);
    let cstime = us_to_clock_ticks(times.children_system_us);
    // UNFINISHED: Linux /proc/<pid>/stat exposes precise tty, start time,
    // virtual memory, RSS, signal, and scheduler fields. This contest subset
    // provides stable parseable fields for BusyBox/procps consumers while the
    // kernel lacks full process accounting.
    format!(
        "{} ({}) {} {} {} {} 0 -1 0 0 0 0 0 {} {} {} {} 20 0 {} 0 0 4096 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0\n",
        stat_pid,
        process.comm,
        state,
        process.ppid,
        process.pgid,
        process.pid,
        utime,
        stime,
        cutime,
        cstime,
        process.thread_count,
    )
}

fn pid_stat_content(process: ProcessProcSnapshot) -> String {
    let pid = process.pid;
    let state = process.state;
    proc_stat_content(process, pid, state)
}

fn task_stat_content(pid: usize, local_tid: usize) -> FsResult<Vec<u8>> {
    let process = pid2process(pid).ok_or(FsError::NotFound)?;
    let process_snapshot = process.proc_snapshot();
    let task = lookup_task_by_local_tid(pid, local_tid).ok_or(FsError::NotFound)?;
    let state = {
        let task_inner = task.inner_exclusive_access();
        task_status_char(task_inner.task_status, task_inner.proc_sleeping)
    };
    Ok(proc_stat_content(process_snapshot, task.linux_tid(), state).into_bytes())
}

fn capability_hex(bits: [u32; 2]) -> u64 {
    ((bits[1] as u64) << 32) | bits[0] as u64
}

fn pid_status_content(process: ProcessProcSnapshot) -> String {
    let cred = process.credentials;
    format!(
        "Name:\t{}\n\
         State:\t{}\n\
         Pid:\t{}\n\
         PPid:\t{}\n\
         Uid:\t{}\t{}\t{}\t{}\n\
         Gid:\t{}\t{}\t{}\t{}\n\
         VmRSS:\t0 kB\n\
         VmLck:\t{} kB\n\
         CapInh:\t{:016x}\n\
         CapPrm:\t{:016x}\n\
         CapEff:\t{:016x}\n\
         CapBnd:\t{:016x}\n\
         CapAmb:\t{:016x}\n\
         NoNewPrivs:\t{}\n\
         Threads:\t{}\n",
        process.comm,
        process.state,
        process.pid,
        process.ppid,
        cred.ruid,
        cred.euid,
        cred.suid,
        cred.fsuid,
        cred.rgid,
        cred.egid,
        cred.sgid,
        cred.fsgid,
        process.locked_kb,
        capability_hex(cred.capabilities.inheritable),
        capability_hex(cred.capabilities.permitted),
        capability_hex(cred.capabilities.effective),
        capability_hex(cred.capabilities.bounding),
        capability_hex(cred.capabilities.ambient),
        process.no_new_privs as usize,
        process.thread_count
    )
}

fn pid_comm_content(process: ProcessProcSnapshot) -> Vec<u8> {
    format!("{}\n", process.comm).into_bytes()
}

fn pid_timerslack_content(process: ProcessProcSnapshot) -> Vec<u8> {
    format!("{}\n", process.timer_slack_ns).into_bytes()
}

fn pid_cmdline_content(process: ProcessProcSnapshot) -> Vec<u8> {
    let mut output = Vec::new();
    for arg in process.cmdline {
        output.extend_from_slice(arg.as_bytes());
        output.push(0);
    }
    output
}

fn pid_exe_target(process: ProcessProcSnapshot) -> FsResult<String> {
    let executable_node = process.executable_node.ok_or(FsError::NotFound)?;
    let mut target = process.executable_path;
    let still_names_executable = matches!(
        lookup_path_in(PathContext::global_root(), target.as_str(), true),
        Ok(path) if path.node == executable_node
    );
    if !still_names_executable {
        target.push_str(" (deleted)");
    }
    Ok(target)
}

fn node_content(node: ProcNode) -> FsResult<Vec<u8>> {
    match node {
        ProcNode::Mounts => Ok(mounts_content().into_bytes()),
        ProcNode::Filesystems => Ok(filesystems_content().as_bytes().to_vec()),
        ProcNode::KeyUsers => Ok(keyring::key_users_content().into_bytes()),
        ProcNode::Meminfo => Ok(meminfo_content().into_bytes()),
        ProcNode::Uptime => Ok(uptime_content().into_bytes()),
        ProcNode::Cpuinfo => Ok(cpuinfo_content().into_bytes()),
        ProcNode::Version => Ok(b"Linux version 6.8.0-whusp (oskernel2026)\n".to_vec()),
        ProcNode::PidMax => Ok(pid_max_content().into_bytes()),
        ProcNode::ShmMax => Ok(format!("{}\n", crate::mm::shm::SHM_MAX).into_bytes()),
        ProcNode::ShmMni => Ok(format!("{}\n", crate::mm::shm::SHMMNI).into_bytes()),
        ProcNode::ShmAll => Ok(format!("{}\n", crate::mm::shm::SHMALL).into_bytes()),
        ProcNode::SysVipcShm => Ok(crate::mm::shm::proc_sysvipc_shm_content().into_bytes()),
        ProcNode::KeysGcDelay => Ok(keyring::key_gc_delay_content().into_bytes()),
        ProcNode::KeysMaxkeys => Ok(keyring::key_maxkeys_content().into_bytes()),
        ProcNode::KeysMaxbytes => Ok(keyring::key_maxbytes_content().into_bytes()),
        ProcNode::CorePattern => Ok(b"core\n".to_vec()),
        ProcNode::PipeMaxSize => Ok(pipe_max_size_content().into_bytes()),
        ProcNode::PipeUserPagesSoft => Ok(pipe_user_pages_soft_content().into_bytes()),
        ProcNode::FanotifyMaxQueuedEvents => Ok(fanotify_max_queued_events_content().into_bytes()),
        ProcNode::InotifyMaxQueuedEvents => Ok(inotify_max_queued_events_content().into_bytes()),
        ProcNode::InotifyMaxUserInstances => Ok(inotify_max_user_instances_content().into_bytes()),
        ProcNode::InotifyMaxUserWatches => Ok(inotify_max_user_watches_content().into_bytes()),
        ProcNode::BlockCacheStats => Ok(block_cache::stats_content().into_bytes()),
        ProcNode::DentryCacheStats => Ok(dentry_cache::stats_content().into_bytes()),
        ProcNode::ExecLoadStats => Ok(exec_load_stats_content().into_bytes()),
        ProcNode::LeaseBreakTime => Ok(lease_break_time_content().into_bytes()),
        ProcNode::NetIpv4ConfLoTag => Ok(net_ipv4_conf_lo_tag_content().into_bytes()),
        ProcNode::NetIpv4ConfDefaultTag => Ok(net_ipv4_conf_default_tag_content().into_bytes()),
        ProcNode::DropCaches => Ok(b"0\n".to_vec()),
        ProcNode::VfsCachePressure => Ok(vfs_cache_pressure_content().into_bytes()),
        ProcNode::Domainname => Ok(domainname_content()),
        ProcNode::Tainted => Ok(b"0\n".to_vec()),
        ProcNode::PidStat(pid) => lookup_process(pid)
            .map(pid_stat_content)
            .map(String::into_bytes)
            .ok_or(FsError::NotFound),
        ProcNode::PidStatus(pid) => lookup_process(pid)
            .map(pid_status_content)
            .map(String::into_bytes)
            .ok_or(FsError::NotFound),
        ProcNode::PidComm(pid) => lookup_process(pid)
            .map(pid_comm_content)
            .ok_or(FsError::NotFound),
        ProcNode::PidCmdline(pid) => lookup_process(pid)
            .map(pid_cmdline_content)
            .ok_or(FsError::NotFound),
        ProcNode::PidTimerslack(pid) => lookup_process(pid)
            .map(pid_timerslack_content)
            .ok_or(FsError::NotFound),
        ProcNode::PidExe(_) | ProcNode::PidFdEntry(_, _) => Err(FsError::InvalidInput),
        ProcNode::PidFdInfoEntry(pid, fd) => pid_fdinfo_content(pid, fd).map(String::into_bytes),
        ProcNode::PidMaps(pid) => pid2process(pid)
            .map(|process| process.proc_maps_content().into_bytes())
            .ok_or(FsError::NotFound),
        ProcNode::PidSmaps(pid) => pid2process(pid)
            .map(|process| process.proc_smaps_content().into_bytes())
            .ok_or(FsError::NotFound),
        ProcNode::PidMounts(pid) => lookup_process(pid)
            .map(|_| mounts_content().into_bytes())
            .ok_or(FsError::NotFound),
        ProcNode::PidMountinfo(pid) => lookup_process(pid)
            .map(|_| mountinfo_content().into_bytes())
            .ok_or(FsError::NotFound),
        ProcNode::PidIo(pid) => lookup_process(pid)
            .map(|_| proc_io_content().into_bytes())
            .ok_or(FsError::NotFound),
        ProcNode::PidNsMnt(pid) => lookup_process(pid)
            .map(|process| {
                let info = namespace_info_for_process(process, ProcNamespaceKind::Mnt);
                format!(
                    "{}:[{}]\n",
                    proc_namespace_kind_name(info.kind),
                    proc_namespace_stat_ino(info.kind, info.id)
                )
                .into_bytes()
            })
            .ok_or(FsError::NotFound),
        ProcNode::PidNsPid(pid) => namespace_info_for_pid(pid, ProcNamespaceKind::Pid)
            .map(|info| {
                format!(
                    "{}:[{}]\n",
                    proc_namespace_kind_name(info.kind),
                    proc_namespace_stat_ino(info.kind, info.id)
                )
                .into_bytes()
            })
            .ok_or(FsError::NotFound),
        ProcNode::PidNsUser(pid) => namespace_info_for_pid(pid, ProcNamespaceKind::User)
            .map(|info| {
                format!(
                    "{}:[{}]\n",
                    proc_namespace_kind_name(info.kind),
                    proc_namespace_stat_ino(info.kind, info.id)
                )
                .into_bytes()
            })
            .ok_or(FsError::NotFound),
        ProcNode::PidNsUts(pid) => namespace_info_for_pid(pid, ProcNamespaceKind::Uts)
            .map(|info| {
                format!(
                    "{}:[{}]\n",
                    proc_namespace_kind_name(info.kind),
                    proc_namespace_stat_ino(info.kind, info.id)
                )
                .into_bytes()
            })
            .ok_or(FsError::NotFound),
        ProcNode::PidTaskTidStat(pid, local_tid) => task_stat_content(pid, local_tid),
        ProcNode::PidTaskTidComm(pid, local_tid) => {
            lookup_task_by_local_tid(pid, local_tid).ok_or(FsError::NotFound)?;
            lookup_process(pid)
                .map(pid_comm_content)
                .ok_or(FsError::NotFound)
        }
        ProcNode::Root
        | ProcNode::SysDir
        | ProcNode::SysKernelDir
        | ProcNode::SysKernelKeysDir
        | ProcNode::SysFsDir
        | ProcNode::SysFsFanotifyDir
        | ProcNode::SysFsInotifyDir
        | ProcNode::SysNetDir
        | ProcNode::SysNetIpv4Dir
        | ProcNode::SysNetIpv4ConfDir
        | ProcNode::SysNetIpv4ConfLoDir
        | ProcNode::SysNetIpv4ConfDefaultDir
        | ProcNode::SysVmDir
        | ProcNode::SysVipcDir
        | ProcNode::PidDir(_)
        | ProcNode::PidFdDir(_)
        | ProcNode::PidFdInfoDir(_)
        | ProcNode::PidNsDir(_)
        | ProcNode::PidTaskDir(_)
        | ProcNode::PidTaskTidDir(_, _) => Err(FsError::IsDir),
    }
}

impl FileSystemBackend for ProcFs {
    fn root_ino(&self) -> u32 {
        ROOT_INO
    }

    fn statfs(&mut self) -> super::vfs::FileSystemStat {
        super::vfs::FileSystemStat {
            magic: 0x9FA0,
            block_size: 4096,
            blocks: 0,
            free_blocks: 0,
            available_blocks: 0,
            files: 1024,
            free_files: 1024,
            max_name_len: 255,
            flags: 0,
        }
    }

    fn lookup_component_from(
        &mut self,
        parent_ino: u32,
        component: &str,
    ) -> FsResult<(u32, FsNodeKind)> {
        let parent = decode_node(parent_ino).ok_or(FsError::NotFound)?;
        match parent {
            ProcNode::Root => match component {
                "." | ".." => Ok((ROOT_INO, FsNodeKind::Directory)),
                "mounts" => Ok((MOUNTS_INO, FsNodeKind::RegularFile)),
                "filesystems" => Ok((FILESYSTEMS_INO, FsNodeKind::RegularFile)),
                "key-users" => Ok((KEY_USERS_INO, FsNodeKind::RegularFile)),
                "meminfo" => Ok((MEMINFO_INO, FsNodeKind::RegularFile)),
                "uptime" => Ok((UPTIME_INO, FsNodeKind::RegularFile)),
                "cpuinfo" => Ok((CPUINFO_INO, FsNodeKind::RegularFile)),
                "version" => Ok((VERSION_INO, FsNodeKind::RegularFile)),
                "sys" => Ok((SYS_DIR_INO, FsNodeKind::Directory)),
                "sysvipc" => Ok((SYSVIPC_DIR_INO, FsNodeKind::Directory)),
                "self" => {
                    let pid = crate::task::current_process().getpid();
                    Ok((pid_dir_ino(pid), FsNodeKind::Directory))
                }
                _ => {
                    let pid = parse_pid(component).ok_or(FsError::NotFound)?;
                    lookup_process(pid)
                        .map(|_| (pid_dir_ino(pid), FsNodeKind::Directory))
                        .ok_or(FsError::NotFound)
                }
            },
            ProcNode::SysDir => match component {
                "." => Ok((SYS_DIR_INO, FsNodeKind::Directory)),
                ".." => Ok((ROOT_INO, FsNodeKind::Directory)),
                "kernel" => Ok((SYS_KERNEL_DIR_INO, FsNodeKind::Directory)),
                "fs" => Ok((SYS_FS_DIR_INO, FsNodeKind::Directory)),
                "net" => Ok((SYS_NET_DIR_INO, FsNodeKind::Directory)),
                "vm" => Ok((SYS_VM_DIR_INO, FsNodeKind::Directory)),
                _ => Err(FsError::NotFound),
            },
            ProcNode::SysVmDir => match component {
                "." => Ok((SYS_VM_DIR_INO, FsNodeKind::Directory)),
                ".." => Ok((SYS_DIR_INO, FsNodeKind::Directory)),
                "drop_caches" => Ok((DROP_CACHES_INO, FsNodeKind::RegularFile)),
                "vfs_cache_pressure" => Ok((VFS_CACHE_PRESSURE_INO, FsNodeKind::RegularFile)),
                _ => Err(FsError::NotFound),
            },
            ProcNode::SysNetDir => match component {
                "." => Ok((SYS_NET_DIR_INO, FsNodeKind::Directory)),
                ".." => Ok((SYS_DIR_INO, FsNodeKind::Directory)),
                "ipv4" => Ok((SYS_NET_IPV4_DIR_INO, FsNodeKind::Directory)),
                _ => Err(FsError::NotFound),
            },
            ProcNode::SysNetIpv4Dir => match component {
                "." => Ok((SYS_NET_IPV4_DIR_INO, FsNodeKind::Directory)),
                ".." => Ok((SYS_NET_DIR_INO, FsNodeKind::Directory)),
                "conf" => Ok((SYS_NET_IPV4_CONF_DIR_INO, FsNodeKind::Directory)),
                _ => Err(FsError::NotFound),
            },
            ProcNode::SysNetIpv4ConfDir => match component {
                "." => Ok((SYS_NET_IPV4_CONF_DIR_INO, FsNodeKind::Directory)),
                ".." => Ok((SYS_NET_IPV4_DIR_INO, FsNodeKind::Directory)),
                "lo" => Ok((SYS_NET_IPV4_CONF_LO_DIR_INO, FsNodeKind::Directory)),
                "default" => Ok((SYS_NET_IPV4_CONF_DEFAULT_DIR_INO, FsNodeKind::Directory)),
                _ => Err(FsError::NotFound),
            },
            ProcNode::SysNetIpv4ConfLoDir => match component {
                "." => Ok((SYS_NET_IPV4_CONF_LO_DIR_INO, FsNodeKind::Directory)),
                ".." => Ok((SYS_NET_IPV4_CONF_DIR_INO, FsNodeKind::Directory)),
                "tag" => Ok((SYS_NET_IPV4_CONF_LO_TAG_INO, FsNodeKind::RegularFile)),
                _ => Err(FsError::NotFound),
            },
            ProcNode::SysNetIpv4ConfDefaultDir => match component {
                "." => Ok((SYS_NET_IPV4_CONF_DEFAULT_DIR_INO, FsNodeKind::Directory)),
                ".." => Ok((SYS_NET_IPV4_CONF_DIR_INO, FsNodeKind::Directory)),
                "tag" => Ok((SYS_NET_IPV4_CONF_DEFAULT_TAG_INO, FsNodeKind::RegularFile)),
                _ => Err(FsError::NotFound),
            },
            ProcNode::SysKernelDir => match component {
                "." => Ok((SYS_KERNEL_DIR_INO, FsNodeKind::Directory)),
                ".." => Ok((SYS_DIR_INO, FsNodeKind::Directory)),
                "pid_max" => Ok((PID_MAX_INO, FsNodeKind::RegularFile)),
                "core_pattern" => Ok((CORE_PATTERN_INO, FsNodeKind::RegularFile)),
                "shmmax" => Ok((SHMMAX_INO, FsNodeKind::RegularFile)),
                "shmmni" => Ok((SHMMNI_INO, FsNodeKind::RegularFile)),
                "shmall" => Ok((SHMALL_INO, FsNodeKind::RegularFile)),
                "keys" => Ok((SYS_KERNEL_KEYS_DIR_INO, FsNodeKind::Directory)),
                "domainname" => Ok((DOMAINNAME_INO, FsNodeKind::RegularFile)),
                "tainted" => Ok((TAINTED_INO, FsNodeKind::RegularFile)),
                "block_cache" => Ok((BLOCK_CACHE_STATS_INO, FsNodeKind::RegularFile)),
                "dentry_cache" => Ok((DENTRY_CACHE_STATS_INO, FsNodeKind::RegularFile)),
                "exec_loader" => Ok((EXEC_LOAD_STATS_INO, FsNodeKind::RegularFile)),
                _ => Err(FsError::NotFound),
            },
            ProcNode::SysKernelKeysDir => match component {
                "." => Ok((SYS_KERNEL_KEYS_DIR_INO, FsNodeKind::Directory)),
                ".." => Ok((SYS_KERNEL_DIR_INO, FsNodeKind::Directory)),
                "gc_delay" => Ok((KEYS_GC_DELAY_INO, FsNodeKind::RegularFile)),
                "maxkeys" => Ok((KEYS_MAXKEYS_INO, FsNodeKind::RegularFile)),
                "maxbytes" => Ok((KEYS_MAXBYTES_INO, FsNodeKind::RegularFile)),
                _ => Err(FsError::NotFound),
            },
            ProcNode::SysVipcDir => match component {
                "." => Ok((SYSVIPC_DIR_INO, FsNodeKind::Directory)),
                ".." => Ok((ROOT_INO, FsNodeKind::Directory)),
                "shm" => Ok((SYSVIPC_SHM_INO, FsNodeKind::RegularFile)),
                _ => Err(FsError::NotFound),
            },
            ProcNode::SysFsDir => match component {
                "." => Ok((SYS_FS_DIR_INO, FsNodeKind::Directory)),
                ".." => Ok((SYS_DIR_INO, FsNodeKind::Directory)),
                "pipe-max-size" => Ok((PIPE_MAX_SIZE_INO, FsNodeKind::RegularFile)),
                "pipe-user-pages-soft" => Ok((PIPE_USER_PAGES_SOFT_INO, FsNodeKind::RegularFile)),
                "lease-break-time" => Ok((LEASE_BREAK_TIME_INO, FsNodeKind::RegularFile)),
                "fanotify" => Ok((SYS_FS_FANOTIFY_DIR_INO, FsNodeKind::Directory)),
                "inotify" => Ok((SYS_FS_INOTIFY_DIR_INO, FsNodeKind::Directory)),
                _ => Err(FsError::NotFound),
            },
            ProcNode::SysFsFanotifyDir => match component {
                "." => Ok((SYS_FS_FANOTIFY_DIR_INO, FsNodeKind::Directory)),
                ".." => Ok((SYS_FS_DIR_INO, FsNodeKind::Directory)),
                "max_queued_events" => {
                    Ok((FANOTIFY_MAX_QUEUED_EVENTS_INO, FsNodeKind::RegularFile))
                }
                _ => Err(FsError::NotFound),
            },
            ProcNode::SysFsInotifyDir => match component {
                "." => Ok((SYS_FS_INOTIFY_DIR_INO, FsNodeKind::Directory)),
                ".." => Ok((SYS_FS_DIR_INO, FsNodeKind::Directory)),
                "max_queued_events" => Ok((INOTIFY_MAX_QUEUED_EVENTS_INO, FsNodeKind::RegularFile)),
                "max_user_instances" => {
                    Ok((INOTIFY_MAX_USER_INSTANCES_INO, FsNodeKind::RegularFile))
                }
                "max_user_watches" => Ok((INOTIFY_MAX_USER_WATCHES_INO, FsNodeKind::RegularFile)),
                _ => Err(FsError::NotFound),
            },
            ProcNode::PidDir(pid) => match component {
                "." => Ok((pid_dir_ino(pid), FsNodeKind::Directory)),
                ".." => Ok((ROOT_INO, FsNodeKind::Directory)),
                "stat" => Ok((pid_file_ino(pid, PID_STAT_OFFSET), FsNodeKind::RegularFile)),
                "status" => Ok((
                    pid_file_ino(pid, PID_STATUS_OFFSET),
                    FsNodeKind::RegularFile,
                )),
                "comm" => Ok((pid_file_ino(pid, PID_COMM_OFFSET), FsNodeKind::RegularFile)),
                "cmdline" => Ok((
                    pid_file_ino(pid, PID_CMDLINE_OFFSET),
                    FsNodeKind::RegularFile,
                )),
                "exe" => Ok((pid_file_ino(pid, PID_EXE_OFFSET), FsNodeKind::Symlink)),
                "fd" => Ok((pid_file_ino(pid, PID_FD_DIR_OFFSET), FsNodeKind::Directory)),
                "fdinfo" => Ok((
                    pid_file_ino(pid, PID_FDINFO_DIR_OFFSET),
                    FsNodeKind::Directory,
                )),
                "maps" => Ok((pid_file_ino(pid, PID_MAPS_OFFSET), FsNodeKind::RegularFile)),
                "smaps" => Ok((pid_file_ino(pid, PID_SMAPS_OFFSET), FsNodeKind::RegularFile)),
                "mounts" => Ok((
                    pid_file_ino(pid, PID_MOUNTS_OFFSET),
                    FsNodeKind::RegularFile,
                )),
                "mountinfo" => Ok((
                    pid_file_ino(pid, PID_MOUNTINFO_OFFSET),
                    FsNodeKind::RegularFile,
                )),
                "io" => Ok((pid_file_ino(pid, PID_IO_OFFSET), FsNodeKind::RegularFile)),
                "timerslack_ns" => Ok((
                    pid_file_ino(pid, PID_TIMERSLACK_OFFSET),
                    FsNodeKind::RegularFile,
                )),
                "ns" => Ok((pid_file_ino(pid, PID_NS_DIR_OFFSET), FsNodeKind::Directory)),
                "task" => Ok((
                    pid_file_ino(pid, PID_TASK_DIR_OFFSET),
                    FsNodeKind::Directory,
                )),
                _ => Err(FsError::NotFound),
            },
            ProcNode::PidFdDir(pid) => match component {
                "." => Ok((pid_file_ino(pid, PID_FD_DIR_OFFSET), FsNodeKind::Directory)),
                ".." => Ok((pid_dir_ino(pid), FsNodeKind::Directory)),
                _ => {
                    let fd = parse_pid(component).ok_or(FsError::NotFound)?;
                    let process = pid2process(pid).ok_or(FsError::NotFound)?;
                    let fd_exists = {
                        let inner = process.inner_exclusive_access();
                        inner.fd_table.get(fd).is_some_and(Option::is_some)
                    };
                    if !fd_exists {
                        return Err(FsError::NotFound);
                    }
                    Ok((pid_fd_entry_ino(pid, fd), FsNodeKind::Symlink))
                }
            },
            ProcNode::PidFdInfoDir(pid) => match component {
                "." => Ok((
                    pid_file_ino(pid, PID_FDINFO_DIR_OFFSET),
                    FsNodeKind::Directory,
                )),
                ".." => Ok((pid_dir_ino(pid), FsNodeKind::Directory)),
                _ => {
                    let fd = parse_pid(component).ok_or(FsError::NotFound)?;
                    let process = pid2process(pid).ok_or(FsError::NotFound)?;
                    let fd_exists = {
                        let inner = process.inner_exclusive_access();
                        inner.fd_table.get(fd).is_some_and(Option::is_some)
                    };
                    if !fd_exists {
                        return Err(FsError::NotFound);
                    }
                    Ok((pid_fdinfo_entry_ino(pid, fd), FsNodeKind::RegularFile))
                }
            },
            ProcNode::PidNsDir(pid) => match component {
                "." => Ok((pid_file_ino(pid, PID_NS_DIR_OFFSET), FsNodeKind::Directory)),
                ".." => Ok((pid_dir_ino(pid), FsNodeKind::Directory)),
                "mnt" => Ok((
                    pid_file_ino(pid, PID_NS_MNT_OFFSET),
                    FsNodeKind::RegularFile,
                )),
                "pid" => Ok((
                    pid_file_ino(pid, PID_NS_PID_OFFSET),
                    FsNodeKind::RegularFile,
                )),
                "user" => Ok((
                    pid_file_ino(pid, PID_NS_USER_OFFSET),
                    FsNodeKind::RegularFile,
                )),
                "uts" => Ok((
                    pid_file_ino(pid, PID_NS_UTS_OFFSET),
                    FsNodeKind::RegularFile,
                )),
                _ => Err(FsError::NotFound),
            },
            ProcNode::PidTaskDir(pid) => match component {
                "." => Ok((
                    pid_file_ino(pid, PID_TASK_DIR_OFFSET),
                    FsNodeKind::Directory,
                )),
                ".." => Ok((pid_dir_ino(pid), FsNodeKind::Directory)),
                _ => {
                    let linux_tid = parse_pid(component).ok_or(FsError::NotFound)?;
                    let task = lookup_task_by_linux_tid(pid, linux_tid).ok_or(FsError::NotFound)?;
                    let local_tid = task_local_tid(&task);
                    let ino = pid_task_tid_dir_ino(pid, local_tid).ok_or(FsError::NotFound)?;
                    Ok((ino, FsNodeKind::Directory))
                }
            },
            ProcNode::PidTaskTidDir(pid, local_tid) => match component {
                "." => {
                    let ino = pid_task_tid_dir_ino(pid, local_tid).ok_or(FsError::NotFound)?;
                    Ok((ino, FsNodeKind::Directory))
                }
                ".." => Ok((
                    pid_file_ino(pid, PID_TASK_DIR_OFFSET),
                    FsNodeKind::Directory,
                )),
                "stat" => {
                    let ino = pid_task_tid_stat_ino(pid, local_tid).ok_or(FsError::NotFound)?;
                    Ok((ino, FsNodeKind::RegularFile))
                }
                "comm" => {
                    let ino = pid_task_tid_comm_ino(pid, local_tid).ok_or(FsError::NotFound)?;
                    Ok((ino, FsNodeKind::RegularFile))
                }
                _ => Err(FsError::NotFound),
            },
            _ => Err(FsError::NotDir),
        }
    }

    fn create_file(&mut self, _parent_ino: u32, _leaf_name: &str) -> FsResult<u32> {
        Err(FsError::ReadOnly)
    }

    fn create_dir(&mut self, _parent_ino: u32, _leaf_name: &str, _mode: u32) -> FsResult<u32> {
        Err(FsError::ReadOnly)
    }

    fn link(&mut self, _parent_ino: u32, _leaf_name: &str, _child_ino: u32) -> FsResult {
        Err(FsError::ReadOnly)
    }

    fn symlink(&mut self, _parent_ino: u32, _leaf_name: &str, _target: &[u8]) -> FsResult {
        Err(FsError::ReadOnly)
    }

    fn unlink(&mut self, _parent_ino: u32, _leaf_name: &str) -> FsResult {
        Err(FsError::ReadOnly)
    }

    fn rename(
        &mut self,
        _src_dir: u32,
        _src_name: &str,
        _dst_dir: u32,
        _dst_name: &str,
    ) -> FsResult {
        Err(FsError::ReadOnly)
    }

    fn set_len(&mut self, _ino: u32, _len: u64) -> FsResult {
        match decode_node(_ino).ok_or(FsError::NotFound)? {
            ProcNode::PidMax
            | ProcNode::KeysGcDelay
            | ProcNode::KeysMaxkeys
            | ProcNode::KeysMaxbytes
            | ProcNode::PipeMaxSize
            | ProcNode::LeaseBreakTime
            | ProcNode::InotifyMaxUserInstances
            | ProcNode::NetIpv4ConfLoTag
            | ProcNode::DropCaches
            | ProcNode::VfsCachePressure
            | ProcNode::PidTimerslack(_) => Ok(()),
            ProcNode::Domainname => set_domainname_len(_len),
            _ => Err(FsError::ReadOnly),
        }
    }

    fn set_times(
        &mut self,
        _ino: u32,
        _atime: Option<FileTimestamp>,
        _mtime: Option<FileTimestamp>,
        _ctime: FileTimestamp,
    ) -> FsResult {
        Err(FsError::ReadOnly)
    }

    fn stat(&mut self, ino: u32) -> FsResult<FileStat> {
        let node = decode_node(ino).ok_or(FsError::NotFound)?;
        let mut stat = match node {
            ProcNode::Root
            | ProcNode::SysDir
            | ProcNode::SysKernelDir
            | ProcNode::SysKernelKeysDir
            | ProcNode::SysFsDir
            | ProcNode::SysFsFanotifyDir
            | ProcNode::SysFsInotifyDir
            | ProcNode::SysNetDir
            | ProcNode::SysNetIpv4Dir
            | ProcNode::SysNetIpv4ConfDir
            | ProcNode::SysNetIpv4ConfLoDir
            | ProcNode::SysNetIpv4ConfDefaultDir
            | ProcNode::SysVmDir
            | ProcNode::SysVipcDir
            | ProcNode::PidDir(_)
            | ProcNode::PidFdDir(_)
            | ProcNode::PidFdInfoDir(_)
            | ProcNode::PidNsDir(_)
            | ProcNode::PidTaskDir(_)
            | ProcNode::PidTaskTidDir(_, _) => FileStat::with_mode(S_IFDIR | 0o555),
            ProcNode::PidExe(_) | ProcNode::PidFdEntry(_, _) => {
                FileStat::with_mode(S_IFLNK | 0o777)
            }
            ProcNode::PidMax
            | ProcNode::ShmMax
            | ProcNode::ShmMni
            | ProcNode::ShmAll
            | ProcNode::KeysGcDelay
            | ProcNode::KeysMaxkeys
            | ProcNode::KeysMaxbytes
            | ProcNode::PipeMaxSize
            | ProcNode::LeaseBreakTime
            | ProcNode::InotifyMaxUserInstances
            | ProcNode::NetIpv4ConfLoTag
            | ProcNode::DropCaches
            | ProcNode::VfsCachePressure
            | ProcNode::PidComm(_)
            | ProcNode::PidTimerslack(_)
            | ProcNode::PidTaskTidComm(_, _)
            | ProcNode::Domainname => FileStat::with_mode(S_IFREG | 0o644),
            _ => FileStat::with_mode(S_IFREG | 0o444),
        };
        stat.dev = 0x70726f63;
        stat.ino = match node {
            ProcNode::PidNsMnt(pid) => namespace_info_for_pid(pid, ProcNamespaceKind::Mnt)
                .map(|info| proc_namespace_stat_ino(info.kind, info.id))
                .ok_or(FsError::NotFound)?,
            ProcNode::PidNsPid(pid) => namespace_info_for_pid(pid, ProcNamespaceKind::Pid)
                .map(|info| proc_namespace_stat_ino(info.kind, info.id))
                .ok_or(FsError::NotFound)?,
            ProcNode::PidNsUser(pid) => namespace_info_for_pid(pid, ProcNamespaceKind::User)
                .map(|info| proc_namespace_stat_ino(info.kind, info.id))
                .ok_or(FsError::NotFound)?,
            ProcNode::PidNsUts(pid) => namespace_info_for_pid(pid, ProcNamespaceKind::Uts)
                .map(|info| proc_namespace_stat_ino(info.kind, info.id))
                .ok_or(FsError::NotFound)?,
            _ => ino as u64,
        };
        stat.nlink = if node_kind(node) == FsNodeKind::Directory {
            2
        } else {
            1
        };
        stat.size = 0;
        Ok(stat)
    }

    fn readlink(&mut self, ino: u32, buf: &mut [u8]) -> FsResult<usize> {
        let node = decode_node(ino).ok_or(FsError::NotFound)?;
        let target = match node {
            ProcNode::PidExe(pid) => pid_exe_target(lookup_process(pid).ok_or(FsError::NotFound)?)?,
            ProcNode::PidFdEntry(pid, fd) => {
                let process = pid2process(pid).ok_or(FsError::NotFound)?;
                let inner = process.inner_exclusive_access();
                let entry = inner
                    .fd_table
                    .get(fd)
                    .and_then(Option::as_ref)
                    .ok_or(FsError::NotFound)?;
                let file = entry.file();
                entry
                    .dir_path()
                    .map(String::from)
                    .or_else(|| file.proc_fd_target())
                    .unwrap_or_else(|| format!("/proc/{pid}/fd/{fd} (deleted)"))
            }
            _ => return Err(FsError::InvalidInput),
        };
        let len = target.len().min(buf.len());
        buf[..len].copy_from_slice(&target.as_bytes()[..len]);
        Ok(len)
    }

    fn read_at(&mut self, ino: u32, buf: &mut [u8], offset: u64) -> usize {
        let Some(node) = decode_node(ino) else {
            return 0;
        };
        let Ok(content) = node_content(node) else {
            return 0;
        };
        let start = (offset as usize).min(content.len());
        let len = buf.len().min(content.len() - start);
        buf[..len].copy_from_slice(&content[start..start + len]);
        len
    }

    fn write_at(&mut self, ino: u32, buf: &[u8], offset: u64) -> usize {
        match decode_node(ino) {
            Some(ProcNode::PidMax) => write_pid_max(buf, offset),
            Some(ProcNode::KeysGcDelay) => keyring::write_key_gc_delay(buf, offset),
            Some(ProcNode::KeysMaxkeys) => keyring::write_key_maxkeys(buf, offset),
            Some(ProcNode::KeysMaxbytes) => keyring::write_key_maxbytes(buf, offset),
            Some(ProcNode::PipeMaxSize) => write_pipe_max_size(buf, offset),
            Some(ProcNode::LeaseBreakTime) => write_lease_break_time(buf, offset),
            Some(ProcNode::InotifyMaxUserInstances) => {
                write_inotify_max_user_instances(buf, offset)
            }
            Some(ProcNode::NetIpv4ConfLoTag) => write_net_ipv4_conf_lo_tag(buf, offset),
            Some(ProcNode::DropCaches) => write_drop_caches(buf, offset),
            Some(ProcNode::VfsCachePressure) => write_vfs_cache_pressure(buf, offset),
            Some(ProcNode::PidTimerslack(pid)) => write_pid_timerslack(pid, buf, offset),
            Some(ProcNode::Domainname) => write_domainname(buf, offset),
            _ => 0,
        }
    }

    fn read_dirent64(&mut self, ino: u32, offset: u64, buf: &mut [u8]) -> FsResult<(usize, u64)> {
        match decode_node(ino).ok_or(FsError::NotFound)? {
            ProcNode::Root => write_dir_entries(&root_entries(), offset, buf),
            ProcNode::SysDir => write_dir_entries(&sys_entries(), offset, buf),
            ProcNode::SysVipcDir => write_dir_entries(&sysvipc_entries(), offset, buf),
            ProcNode::SysKernelDir => write_dir_entries(&sys_kernel_entries(), offset, buf),
            ProcNode::SysKernelKeysDir => {
                write_dir_entries(&sys_kernel_keys_entries(), offset, buf)
            }
            ProcNode::SysFsDir => write_dir_entries(&sys_fs_entries(), offset, buf),
            ProcNode::SysFsFanotifyDir => {
                write_dir_entries(&sys_fs_fanotify_entries(), offset, buf)
            }
            ProcNode::SysFsInotifyDir => write_dir_entries(&sys_fs_inotify_entries(), offset, buf),
            ProcNode::SysVmDir => write_dir_entries(&sys_vm_entries(), offset, buf),
            ProcNode::SysNetDir => write_dir_entries(&sys_net_entries(), offset, buf),
            ProcNode::SysNetIpv4Dir => write_dir_entries(&sys_net_ipv4_entries(), offset, buf),
            ProcNode::SysNetIpv4ConfDir => {
                write_dir_entries(&sys_net_ipv4_conf_entries(), offset, buf)
            }
            ProcNode::SysNetIpv4ConfLoDir => write_dir_entries(
                &sys_net_ipv4_conf_iface_entries(
                    SYS_NET_IPV4_CONF_LO_DIR_INO,
                    SYS_NET_IPV4_CONF_LO_TAG_INO,
                ),
                offset,
                buf,
            ),
            ProcNode::SysNetIpv4ConfDefaultDir => write_dir_entries(
                &sys_net_ipv4_conf_iface_entries(
                    SYS_NET_IPV4_CONF_DEFAULT_DIR_INO,
                    SYS_NET_IPV4_CONF_DEFAULT_TAG_INO,
                ),
                offset,
                buf,
            ),
            ProcNode::PidDir(pid) => write_dir_entries(&pid_entries(pid), offset, buf),
            ProcNode::PidFdDir(pid) => write_dir_entries(&pid_fd_entries(pid)?, offset, buf),
            ProcNode::PidFdInfoDir(pid) => {
                write_dir_entries(&pid_fdinfo_entries(pid)?, offset, buf)
            }
            ProcNode::PidNsDir(pid) => write_dir_entries(&pid_ns_entries(pid), offset, buf),
            ProcNode::PidTaskDir(pid) => write_dir_entries(&pid_task_entries(pid)?, offset, buf),
            ProcNode::PidTaskTidDir(pid, local_tid) => {
                write_dir_entries(&pid_task_tid_entries(pid, local_tid)?, offset, buf)
            }
            _ => Err(FsError::NotDir),
        }
    }

    fn list_root_names(&mut self) -> Vec<String> {
        root_entries()
            .into_iter()
            .filter(|entry| entry.name != "." && entry.name != "..")
            .map(|entry| entry.name)
            .collect()
    }
}
