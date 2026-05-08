use super::dirent::{DT_DIR, DT_LNK, DT_REG, RawDirEntry, write_dir_entries};
use super::mount;
use super::pipe::{PIPE_MAX_CAPACITY, PIPE_MIN_CAPACITY};
use super::vfs::{FileSystemBackend, FsError, FsNodeKind, FsResult};
use super::{FileStat, FileTimestamp, S_IFDIR, S_IFLNK, S_IFREG};
use crate::config::PAGE_SIZE;
use crate::mm::frame_stats;
use crate::sync::UPIntrFreeCell;
use crate::task::{
    ProcessProcSnapshot, TaskControlBlock, TaskStatus, list_process_snapshots, pid2process,
};
use crate::timer::{get_time_us, us_to_clock_ticks};
use alloc::format;
use alloc::string::{String, ToString};
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicUsize, Ordering};
use lazy_static::lazy_static;

const ROOT_INO: u32 = 2;
const MOUNTS_INO: u32 = 3;
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
const PID_DIR_BASE: u32 = 100;
const PID_FILE_BASE: u32 = 10_000;
const PID_FILE_STRIDE: u32 = 16;
const PID_STAT_OFFSET: u32 = 0;
const PID_STATUS_OFFSET: u32 = 1;
const PID_CMDLINE_OFFSET: u32 = 2;
const PID_FD_DIR_OFFSET: u32 = 3;
const PID_MAPS_OFFSET: u32 = 4;
const PID_NS_DIR_OFFSET: u32 = 5;
const PID_NS_MNT_OFFSET: u32 = 6;
const PID_TASK_DIR_OFFSET: u32 = 7;
const PID_FD_ENTRY_BASE: u32 = 1_000_000;
const PID_FD_ENTRY_STRIDE: u32 = 4096;
const PID_TASK_INO_TAG_MASK: u32 = 0xC000_0000;
const PID_TASK_TID_DIR_TAG: u32 = 0x8000_0000;
const PID_TASK_TID_STAT_TAG: u32 = 0xC000_0000;
const PID_TASK_PID_SHIFT: usize = 12;
const PID_TASK_TID_MASK: u32 = (1 << PID_TASK_PID_SHIFT) - 1;
const PID_TASK_MAX_PID: usize = 1 << (30 - PID_TASK_PID_SHIFT);
const PID_TASK_MAX_LOCAL_TID: usize = 1 << PID_TASK_PID_SHIFT;
const DEFAULT_PID_MAX: usize = 4_194_304;
const DEFAULT_PIPE_USER_PAGES_SOFT: usize = 1;

static PROC_PID_MAX: AtomicUsize = AtomicUsize::new(DEFAULT_PID_MAX);
static PROC_PIPE_MAX_SIZE: AtomicUsize = AtomicUsize::new(PIPE_MAX_CAPACITY);
static PROC_PIPE_USER_PAGES_SOFT: AtomicUsize = AtomicUsize::new(DEFAULT_PIPE_USER_PAGES_SOFT);

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

pub(super) struct ProcFs;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ProcNode {
    Root,
    Mounts,
    Meminfo,
    Uptime,
    Cpuinfo,
    SysDir,
    SysKernelDir,
    SysFsDir,
    PidMax,
    PipeMaxSize,
    PipeUserPagesSoft,
    Domainname,
    Tainted,
    PidDir(usize),
    PidStat(usize),
    PidStatus(usize),
    PidCmdline(usize),
    PidFdDir(usize),
    PidFdEntry(usize, usize),
    PidMaps(usize),
    PidNsDir(usize),
    PidNsMnt(usize),
    PidTaskDir(usize),
    PidTaskTidDir(usize, usize),
    PidTaskTidStat(usize, usize),
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

fn decode_pid_task_tid_ino(ino: u32) -> Option<ProcNode> {
    let tag = ino & PID_TASK_INO_TAG_MASK;
    if !matches!(tag, PID_TASK_TID_DIR_TAG | PID_TASK_TID_STAT_TAG) {
        return None;
    }
    let payload = ino & !PID_TASK_INO_TAG_MASK;
    let pid = (payload >> PID_TASK_PID_SHIFT) as usize;
    let local_tid = (payload & PID_TASK_TID_MASK) as usize;
    lookup_task_by_local_tid(pid, local_tid)?;
    match tag {
        PID_TASK_TID_DIR_TAG => Some(ProcNode::PidTaskTidDir(pid, local_tid)),
        PID_TASK_TID_STAT_TAG => Some(ProcNode::PidTaskTidStat(pid, local_tid)),
        _ => None,
    }
}

fn lookup_process(pid: usize) -> Option<ProcessProcSnapshot> {
    pid2process(pid).map(|process| process.proc_snapshot())
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
        MEMINFO_INO => Some(ProcNode::Meminfo),
        UPTIME_INO => Some(ProcNode::Uptime),
        CPUINFO_INO => Some(ProcNode::Cpuinfo),
        SYS_DIR_INO => Some(ProcNode::SysDir),
        SYS_KERNEL_DIR_INO => Some(ProcNode::SysKernelDir),
        PID_MAX_INO => Some(ProcNode::PidMax),
        SYS_FS_DIR_INO => Some(ProcNode::SysFsDir),
        PIPE_MAX_SIZE_INO => Some(ProcNode::PipeMaxSize),
        PIPE_USER_PAGES_SOFT_INO => Some(ProcNode::PipeUserPagesSoft),
        DOMAINNAME_INO => Some(ProcNode::Domainname),
        TAINTED_INO => Some(ProcNode::Tainted),
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
            if lookup_process(pid).is_none() {
                return None;
            }
            match offset {
                PID_STAT_OFFSET => Some(ProcNode::PidStat(pid)),
                PID_STATUS_OFFSET => Some(ProcNode::PidStatus(pid)),
                PID_CMDLINE_OFFSET => Some(ProcNode::PidCmdline(pid)),
                PID_FD_DIR_OFFSET => Some(ProcNode::PidFdDir(pid)),
                PID_MAPS_OFFSET => Some(ProcNode::PidMaps(pid)),
                PID_NS_DIR_OFFSET => Some(ProcNode::PidNsDir(pid)),
                PID_NS_MNT_OFFSET => Some(ProcNode::PidNsMnt(pid)),
                PID_TASK_DIR_OFFSET => Some(ProcNode::PidTaskDir(pid)),
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
        | ProcNode::SysFsDir
        | ProcNode::PidDir(_)
        | ProcNode::PidFdDir(_)
        | ProcNode::PidNsDir(_)
        | ProcNode::PidTaskDir(_)
        | ProcNode::PidTaskTidDir(_, _) => FsNodeKind::Directory,
        ProcNode::PidFdEntry(_, _) => FsNodeKind::Symlink,
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
        ino: SYS_DIR_INO,
        name: "sys".into(),
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
        ino: DOMAINNAME_INO,
        name: "domainname".into(),
        dtype: DT_REG,
    });
    entries.push(RawDirEntry {
        ino: TAINTED_INO,
        name: "tainted".into(),
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
        ino: pid_file_ino(pid, PID_CMDLINE_OFFSET),
        name: "cmdline".into(),
        dtype: DT_REG,
    });
    entries.push(RawDirEntry {
        ino: pid_file_ino(pid, PID_FD_DIR_OFFSET),
        name: "fd".into(),
        dtype: DT_DIR,
    });
    entries.push(RawDirEntry {
        ino: pid_file_ino(pid, PID_MAPS_OFFSET),
        name: "maps".into(),
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

fn meminfo_content() -> String {
    let (total_pages, free_pages) = frame_stats();
    let page_kb = PAGE_SIZE / 1024;
    let total_kb = total_pages * page_kb;
    let free_kb = free_pages * page_kb;
    format!(
        "MemTotal:       {total_kb:8} kB\n\
         MemFree:        {free_kb:8} kB\n\
         MemAvailable:   {free_kb:8} kB\n\
         Buffers:               0 kB\n\
         Cached:                0 kB\n\
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

fn task_status_char(status: TaskStatus) -> char {
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
    let state = task_status_char(task.inner_exclusive_access().task_status);
    Ok(proc_stat_content(process_snapshot, task.linux_tid(), state).into_bytes())
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
        process.thread_count
    )
}

fn pid_cmdline_content(process: ProcessProcSnapshot) -> Vec<u8> {
    let mut output = Vec::new();
    for arg in process.cmdline {
        output.extend_from_slice(arg.as_bytes());
        output.push(0);
    }
    output
}

fn node_content(node: ProcNode) -> FsResult<Vec<u8>> {
    match node {
        ProcNode::Mounts => Ok(mounts_content().into_bytes()),
        ProcNode::Meminfo => Ok(meminfo_content().into_bytes()),
        ProcNode::Uptime => Ok(uptime_content().into_bytes()),
        ProcNode::Cpuinfo => Ok(cpuinfo_content().into_bytes()),
        ProcNode::PidMax => Ok(pid_max_content().into_bytes()),
        ProcNode::PipeMaxSize => Ok(pipe_max_size_content().into_bytes()),
        ProcNode::PipeUserPagesSoft => Ok(pipe_user_pages_soft_content().into_bytes()),
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
        ProcNode::PidCmdline(pid) => lookup_process(pid)
            .map(pid_cmdline_content)
            .ok_or(FsError::NotFound),
        ProcNode::PidFdEntry(_, _) => Err(FsError::InvalidInput),
        ProcNode::PidMaps(pid) => pid2process(pid)
            .map(|process| process.proc_maps_content().into_bytes())
            .ok_or(FsError::NotFound),
        ProcNode::PidNsMnt(pid) => lookup_process(pid)
            .map(|process| format!("mnt:[{}]\n", process.mount_namespace_id.0).into_bytes())
            .ok_or(FsError::NotFound),
        ProcNode::PidTaskTidStat(pid, local_tid) => task_stat_content(pid, local_tid),
        ProcNode::Root
        | ProcNode::SysDir
        | ProcNode::SysKernelDir
        | ProcNode::SysFsDir
        | ProcNode::PidDir(_)
        | ProcNode::PidFdDir(_)
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
                "meminfo" => Ok((MEMINFO_INO, FsNodeKind::RegularFile)),
                "uptime" => Ok((UPTIME_INO, FsNodeKind::RegularFile)),
                "cpuinfo" => Ok((CPUINFO_INO, FsNodeKind::RegularFile)),
                "sys" => Ok((SYS_DIR_INO, FsNodeKind::Directory)),
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
                _ => Err(FsError::NotFound),
            },
            ProcNode::SysKernelDir => match component {
                "." => Ok((SYS_KERNEL_DIR_INO, FsNodeKind::Directory)),
                ".." => Ok((SYS_DIR_INO, FsNodeKind::Directory)),
                "pid_max" => Ok((PID_MAX_INO, FsNodeKind::RegularFile)),
                "domainname" => Ok((DOMAINNAME_INO, FsNodeKind::RegularFile)),
                "tainted" => Ok((TAINTED_INO, FsNodeKind::RegularFile)),
                _ => Err(FsError::NotFound),
            },
            ProcNode::SysFsDir => match component {
                "." => Ok((SYS_FS_DIR_INO, FsNodeKind::Directory)),
                ".." => Ok((SYS_DIR_INO, FsNodeKind::Directory)),
                "pipe-max-size" => Ok((PIPE_MAX_SIZE_INO, FsNodeKind::RegularFile)),
                "pipe-user-pages-soft" => Ok((PIPE_USER_PAGES_SOFT_INO, FsNodeKind::RegularFile)),
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
                "cmdline" => Ok((
                    pid_file_ino(pid, PID_CMDLINE_OFFSET),
                    FsNodeKind::RegularFile,
                )),
                "fd" => Ok((pid_file_ino(pid, PID_FD_DIR_OFFSET), FsNodeKind::Directory)),
                "maps" => Ok((pid_file_ino(pid, PID_MAPS_OFFSET), FsNodeKind::RegularFile)),
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
            ProcNode::PidNsDir(pid) => match component {
                "." => Ok((pid_file_ino(pid, PID_NS_DIR_OFFSET), FsNodeKind::Directory)),
                ".." => Ok((pid_dir_ino(pid), FsNodeKind::Directory)),
                "mnt" => Ok((
                    pid_file_ino(pid, PID_NS_MNT_OFFSET),
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
            ProcNode::PidMax | ProcNode::PipeMaxSize => Ok(()),
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
            | ProcNode::SysFsDir
            | ProcNode::PidDir(_)
            | ProcNode::PidFdDir(_)
            | ProcNode::PidNsDir(_)
            | ProcNode::PidTaskDir(_)
            | ProcNode::PidTaskTidDir(_, _) => FileStat::with_mode(S_IFDIR | 0o555),
            ProcNode::PidFdEntry(_, _) => FileStat::with_mode(S_IFLNK | 0o777),
            ProcNode::PidMax | ProcNode::PipeMaxSize | ProcNode::Domainname => {
                FileStat::with_mode(S_IFREG | 0o644)
            }
            _ => FileStat::with_mode(S_IFREG | 0o444),
        };
        stat.dev = 0x70726f63;
        stat.ino = ino as u64;
        stat.nlink = if node_kind(node) == FsNodeKind::Directory {
            2
        } else {
            1
        };
        stat.size = 0;
        Ok(stat)
    }

    fn readlink(&mut self, ino: u32, buf: &mut [u8]) -> FsResult<usize> {
        let ProcNode::PidFdEntry(pid, fd) = decode_node(ino).ok_or(FsError::NotFound)? else {
            return Err(FsError::InvalidInput);
        };
        let process = pid2process(pid).ok_or(FsError::NotFound)?;
        let target = {
            let inner = process.inner_exclusive_access();
            let entry = inner
                .fd_table
                .get(fd)
                .and_then(Option::as_ref)
                .ok_or(FsError::NotFound)?;
            entry
                .dir_path()
                .map(String::from)
                .unwrap_or_else(|| format!("/proc/{pid}/fd/{fd} (deleted)"))
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
            Some(ProcNode::PipeMaxSize) => write_pipe_max_size(buf, offset),
            Some(ProcNode::Domainname) => write_domainname(buf, offset),
            _ => 0,
        }
    }

    fn read_dirent64(&mut self, ino: u32, offset: u64, buf: &mut [u8]) -> FsResult<(usize, u64)> {
        match decode_node(ino).ok_or(FsError::NotFound)? {
            ProcNode::Root => write_dir_entries(&root_entries(), offset, buf),
            ProcNode::SysDir => write_dir_entries(&sys_entries(), offset, buf),
            ProcNode::SysKernelDir => write_dir_entries(&sys_kernel_entries(), offset, buf),
            ProcNode::SysFsDir => write_dir_entries(&sys_fs_entries(), offset, buf),
            ProcNode::PidDir(pid) => write_dir_entries(&pid_entries(pid), offset, buf),
            ProcNode::PidFdDir(pid) => write_dir_entries(&pid_fd_entries(pid)?, offset, buf),
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
