use super::dirent::{DT_DIR, DT_REG, RawDirEntry, write_dir_entries};
use super::mount;
use super::vfs::{FileSystemBackend, FsError, FsNodeKind, FsResult};
use super::{FileStat, S_IFDIR, S_IFREG};
use crate::config::PAGE_SIZE;
use crate::mm::frame_stats;
use crate::task::{ProcessProcSnapshot, list_process_snapshots, pid2process};
use crate::timer::{get_time_us, us_to_clock_ticks};
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

const ROOT_INO: u32 = 2;
const MOUNTS_INO: u32 = 3;
const MEMINFO_INO: u32 = 4;
const UPTIME_INO: u32 = 5;
const PID_DIR_BASE: u32 = 100;
const PID_FILE_BASE: u32 = 10_000;
const PID_FILE_STRIDE: u32 = 10;
const PID_STAT_OFFSET: u32 = 0;
const PID_STATUS_OFFSET: u32 = 1;
const PID_CMDLINE_OFFSET: u32 = 2;

pub(super) struct ProcFs;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ProcNode {
    Root,
    Mounts,
    Meminfo,
    Uptime,
    PidDir(usize),
    PidStat(usize),
    PidStatus(usize),
    PidCmdline(usize),
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

fn lookup_process(pid: usize) -> Option<ProcessProcSnapshot> {
    pid2process(pid).map(|process| process.proc_snapshot())
}

fn parse_pid(component: &str) -> Option<usize> {
    if component.is_empty() || !component.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    component.parse().ok()
}

fn decode_node(ino: u32) -> Option<ProcNode> {
    match ino {
        ROOT_INO => Some(ProcNode::Root),
        MOUNTS_INO => Some(ProcNode::Mounts),
        MEMINFO_INO => Some(ProcNode::Meminfo),
        UPTIME_INO => Some(ProcNode::Uptime),
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
        ProcNode::Root | ProcNode::PidDir(_) => FsNodeKind::Directory,
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
    for process in list_process_snapshots() {
        entries.push(RawDirEntry {
            ino: pid_dir_ino(process.pid),
            name: process.pid.to_string(),
            dtype: DT_DIR,
        });
    }
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
    entries
}

fn mounts_content() -> String {
    let mut output = String::new();
    for mount in mount::list_mounts() {
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

fn pid_stat_content(process: ProcessProcSnapshot) -> String {
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
        process.pid,
        process.comm,
        process.state,
        process.ppid,
        process.pid,
        process.pid,
        utime,
        stime,
        cutime,
        cstime,
        process.thread_count,
    )
}

fn pid_status_content(process: ProcessProcSnapshot) -> String {
    format!(
        "Name:\t{}\n\
         State:\t{}\n\
         Pid:\t{}\n\
         PPid:\t{}\n\
         Uid:\t0\t0\t0\t0\n\
         Gid:\t0\t0\t0\t0\n\
         VmRSS:\t0 kB\n\
         Threads:\t{}\n",
        process.comm, process.state, process.pid, process.ppid, process.thread_count
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
        ProcNode::Root | ProcNode::PidDir(_) => Err(FsError::IsDir),
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
        Err(FsError::ReadOnly)
    }

    fn stat(&mut self, ino: u32) -> FsResult<FileStat> {
        let node = decode_node(ino).ok_or(FsError::NotFound)?;
        let mut stat = match node {
            ProcNode::Root | ProcNode::PidDir(_) => FileStat::with_mode(S_IFDIR | 0o555),
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

    fn readlink(&mut self, _ino: u32, _buf: &mut [u8]) -> FsResult<usize> {
        Err(FsError::InvalidInput)
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

    fn write_at(&mut self, _ino: u32, _buf: &[u8], _offset: u64) -> usize {
        0
    }

    fn read_dirent64(&mut self, ino: u32, offset: u64, buf: &mut [u8]) -> FsResult<(usize, u64)> {
        match decode_node(ino).ok_or(FsError::NotFound)? {
            ProcNode::Root => write_dir_entries(&root_entries(), offset, buf),
            ProcNode::PidDir(pid) => write_dir_entries(&pid_entries(pid), offset, buf),
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
