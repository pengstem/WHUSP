use crate::fs::{
    MountNamespaceId, ProcNamespaceInfo, ProcNamespaceKind, proc_namespace_info_from_path,
    proc_namespace_info_from_stat_ino,
};
use crate::task::{FdTableEntry, current_process};

use super::super::errno::{SysError, SysResult};

const CLONE_NEWNS: usize = 1 << 17;

fn mount_namespace_id_from_info(info: ProcNamespaceInfo) -> Option<MountNamespaceId> {
    (info.kind == ProcNamespaceKind::Mnt).then_some(MountNamespaceId(info.id))
}

fn mount_namespace_id_from_proc_path(path: &str) -> Option<MountNamespaceId> {
    proc_namespace_info_from_path(path).and_then(mount_namespace_id_from_info)
}

fn mount_namespace_id_from_proc_stat(entry: &FdTableEntry) -> Option<MountNamespaceId> {
    let stat = entry.file().stat().ok()?;
    proc_namespace_info_from_stat_ino(stat.ino).and_then(mount_namespace_id_from_info)
}

fn mount_namespace_id_from_fd(entry: &FdTableEntry) -> Option<MountNamespaceId> {
    entry
        .dir_path()
        .and_then(mount_namespace_id_from_proc_path)
        .or_else(|| {
            entry
                .file()
                .proc_fd_target()
                .and_then(|path| mount_namespace_id_from_proc_path(path.as_str()))
        })
        .or_else(|| mount_namespace_id_from_proc_stat(entry))
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
    let target_namespace = mount_namespace_id_from_fd(&entry).ok_or(SysError::EINVAL)?;
    current_process().set_mount_namespace_id(target_namespace);
    Ok(0)
}
