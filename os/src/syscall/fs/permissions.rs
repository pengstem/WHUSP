use crate::fs::{mount_is_read_only, FileStat, MountId, S_IFMT, S_IFREG};
use crate::syscall::errno::{SysError, SysResult};
use crate::task::Credentials;

use super::uapi::{F_OK, W_OK, X_OK};

#[derive(Clone, Copy)]
pub(crate) struct AccessSubject<'a> {
    uid: u32,
    gid: u32,
    groups: &'a [u32],
}

impl<'a> AccessSubject<'a> {
    pub(crate) fn from_fs_credentials(credentials: &'a Credentials) -> Self {
        Self {
            uid: credentials.fsuid,
            gid: credentials.fsgid,
            groups: &credentials.groups,
        }
    }

    pub(crate) fn from_effective_credentials(credentials: &'a Credentials) -> Self {
        Self {
            uid: credentials.euid,
            gid: credentials.egid,
            groups: &credentials.groups,
        }
    }

    pub(crate) fn from_real_credentials(credentials: &'a Credentials) -> Self {
        Self {
            uid: credentials.ruid,
            gid: credentials.rgid,
            groups: &credentials.groups,
        }
    }

    pub(crate) fn is_root(self) -> bool {
        self.uid == 0
    }

    pub(crate) fn uid(self) -> u32 {
        self.uid
    }

    fn in_group(self, gid: u32) -> bool {
        self.gid == gid || self.groups.iter().any(|group| *group == gid)
    }
}

pub(crate) fn check_access_mode(
    stat: &FileStat,
    mode: i32,
    subject: AccessSubject<'_>,
) -> SysResult<()> {
    if mode == F_OK {
        return Ok(());
    }

    if mode & W_OK != 0 && mount_is_read_only(MountId(stat.dev as usize)) {
        return Err(SysError::EROFS);
    }

    // UNFINISHED: Linux also folds capabilities, ACLs, immutable/append-only
    // inode flags, noexec mounts, and ETXTBSY into access checks. This kernel
    // models the common DAC mode-bit path and treats uid 0 as capability-like.
    if subject.is_root() {
        if mode & X_OK != 0 && stat.mode & S_IFMT == S_IFREG && stat.mode & 0o111 == 0 {
            return Err(SysError::EACCES);
        }
        return Ok(());
    }

    let requested = mode as u32 & 0o7;
    let granted = if subject.uid == stat.uid {
        (stat.mode >> 6) & 0o7
    } else if subject.in_group(stat.gid) {
        (stat.mode >> 3) & 0o7
    } else {
        stat.mode & 0o7
    };

    if granted & requested != requested {
        return Err(SysError::EACCES);
    }
    Ok(())
}

pub(crate) fn check_execute_permission(
    stat: &FileStat,
    subject: AccessSubject<'_>,
) -> SysResult<()> {
    check_access_mode(stat, X_OK, subject)
}
