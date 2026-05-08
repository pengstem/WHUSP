use crate::task::{current_process, pid2process};

use super::super::errno::{SysError, SysResult};

const CLONE_NEWNS: usize = 1 << 17;
const PID_FILE_BASE: u64 = 10_000;
const PID_FILE_STRIDE: u64 = 10;
const PID_NS_MNT_OFFSET: u64 = 6;

fn mount_namespace_pid_from_proc_path(path: &str) -> Option<usize> {
    let proc_index = path.find("/proc/")?;
    let rest = &path[proc_index + "/proc/".len()..];
    let pid_end = rest.find('/')?;
    let pid = rest[..pid_end].parse().ok()?;
    (&rest[pid_end..] == "/ns/mnt").then_some(pid)
}

fn mount_namespace_pid_from_proc_stat(entry: &crate::task::FdTableEntry) -> Option<usize> {
    let stat = entry.file().stat().ok()?;
    if stat.ino < PID_FILE_BASE {
        return None;
    }
    let rel = stat.ino - PID_FILE_BASE;
    (rel % PID_FILE_STRIDE == PID_NS_MNT_OFFSET).then_some((rel / PID_FILE_STRIDE) as usize)
}

pub fn sys_setns(fd: usize, nstype: usize) -> SysResult {
    if nstype != 0 && nstype != CLONE_NEWNS {
        return Err(SysError::EINVAL);
    }
    let entry = {
        let process = current_process();
        let inner = process.inner_exclusive_access();
        inner
            .fd_table
            .get(fd)
            .and_then(|entry| entry.as_ref())
            .cloned()
            .ok_or(SysError::EBADF)?
    };
    let target_pid = entry
        .dir_path()
        .and_then(mount_namespace_pid_from_proc_path)
        .or_else(|| mount_namespace_pid_from_proc_stat(&entry))
        .ok_or(SysError::EINVAL)?;
    let target_process = pid2process(target_pid).ok_or(SysError::ESRCH)?;
    current_process().set_mount_namespace_id(target_process.mount_namespace_id());
    Ok(0)
}
