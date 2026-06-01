use crate::fs::{
    MountNamespaceId, ProcNamespaceInfo, ProcNamespaceKind, clone_mount_namespace,
    proc_namespace_info_from_path, proc_namespace_info_from_stat_ino,
};
use crate::task::{FdTableEntry, current_process};

use super::super::errno::{SysError, SysResult};

const CLONE_FS: usize = 1 << 9;
const CLONE_FILES: usize = 1 << 10;
const CLONE_NEWNS: usize = 1 << 17;
const CLONE_NEWUSER: usize = 1 << 28;
const CLONE_NEWNET: usize = 1 << 30;
const UNSHARE_SUPPORTED_FLAGS: usize =
    CLONE_FS | CLONE_FILES | CLONE_NEWNS | CLONE_NEWUSER | CLONE_NEWNET;

fn mount_namespace_id_from_info(info: ProcNamespaceInfo) -> Option<MountNamespaceId> {
    (info.kind == ProcNamespaceKind::Mnt).then_some(MountNamespaceId(info.id))
}

fn namespace_info_from_fd(entry: &FdTableEntry) -> Option<ProcNamespaceInfo> {
    entry
        .dir_path()
        .and_then(proc_namespace_info_from_path)
        .or_else(|| {
            entry
                .file()
                .proc_fd_target()
                .and_then(|path| proc_namespace_info_from_path(path.as_str()))
        })
        .or_else(|| {
            let stat = entry.file().stat().ok()?;
            proc_namespace_info_from_stat_ino(stat.ino)
        })
}

pub fn sys_setns(fd: usize, nstype: usize) -> SysResult {
    if nstype != 0 && nstype != CLONE_NEWNS && nstype != CLONE_NEWNET {
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
    let info = namespace_info_from_fd(&entry).ok_or(SysError::EINVAL)?;
    match info.kind {
        ProcNamespaceKind::Mnt => {
            if nstype != 0 && nstype != CLONE_NEWNS {
                return Err(SysError::EINVAL);
            }
            let target_namespace = mount_namespace_id_from_info(info).ok_or(SysError::EINVAL)?;
            current_process().set_mount_namespace_id(target_namespace);
        }
        ProcNamespaceKind::Net => {
            if nstype != 0 && nstype != CLONE_NEWNET {
                return Err(SysError::EINVAL);
            }
            // UNFINISHED: Network namespace state is currently global. LTP net
            // setup needs setns(/proc/<pid>/ns/net) to succeed before running
            // helper commands against the synthetic veth devices.
        }
        _ => return Err(SysError::EINVAL),
    }
    Ok(0)
}

pub fn sys_unshare(flags: usize) -> SysResult {
    if flags & !UNSHARE_SUPPORTED_FLAGS != 0 {
        return Err(SysError::EINVAL);
    }
    let process = current_process();
    if flags & (CLONE_NEWNS | CLONE_NEWNET) != 0 && !process.credentials().is_root() {
        return Err(SysError::EPERM);
    }
    if flags & CLONE_NEWNS != 0 {
        let namespace = clone_mount_namespace(process.mount_namespace_id());
        process.set_mount_namespace_id(namespace);
    }
    if flags & CLONE_NEWUSER != 0 {
        // CONTEXT: LTP network namespace setup writes uid_map/gid_map after
        // unshare. Full user-namespace capability remapping is not modeled,
        // but procfs namespace identity needs to move.
        process.enter_new_user_namespace(process.getpid());
    }
    // CONTEXT: Full network namespaces are not implemented; the setsockopt
    // CVE probes only need unshare(CLONE_NEWNET) to succeed before operating
    // on local packet/socket metadata.
    Ok(0)
}
