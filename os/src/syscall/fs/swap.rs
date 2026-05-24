use crate::fs::{FileStat, S_IFMT, S_IFREG, stat_in};
use crate::sync::SleepMutex;
use crate::task::{current_process, current_user_token};
use alloc::collections::BTreeSet;
use lazy_static::lazy_static;

use super::super::errno::{SysError, SysResult};
use super::super::user_ptr::{PATH_MAX, read_user_c_string};
use super::path_context_from;
use super::uapi::AT_FDCWD;

lazy_static! {
    static ref ACTIVE_SWAP_FILES: SleepMutex<BTreeSet<(u64, u64)>> =
        SleepMutex::new(BTreeSet::new());
}

fn resolve_swap_path(pathname: *const u8) -> SysResult<FileStat> {
    if pathname.is_null() {
        return Err(SysError::EFAULT);
    }
    let token = current_user_token();
    let path = read_user_c_string(token, pathname, PATH_MAX)?;
    if path.is_empty() {
        return Err(SysError::ENOENT);
    }
    let snapshot = current_process().path_snapshot();
    let stat = stat_in(
        path_context_from(&snapshot, AT_FDCWD, path.as_str())?,
        path.as_str(),
        true,
    )?;
    if stat.mode & S_IFMT != S_IFREG {
        return Err(SysError::EINVAL);
    }
    Ok(stat)
}

pub(crate) fn is_active_swap_file(stat: FileStat) -> bool {
    ACTIVE_SWAP_FILES.lock().contains(&(stat.dev, stat.ino))
}

// CONTEXT: The kernel has no paging-to-swap implementation yet. LTP setup only
// needs swapon/swapoff to mark a regular file as swap-active so later writes,
// including copy_file_range(), observe Linux's ETXTBSY protection.
pub fn sys_swapon(pathname: *const u8, _flags: u32) -> SysResult {
    let stat = resolve_swap_path(pathname)?;
    ACTIVE_SWAP_FILES.lock().insert((stat.dev, stat.ino));
    Ok(0)
}

pub fn sys_swapoff(pathname: *const u8) -> SysResult {
    let stat = resolve_swap_path(pathname)?;
    ACTIVE_SWAP_FILES.lock().remove(&(stat.dev, stat.ino));
    Ok(0)
}
