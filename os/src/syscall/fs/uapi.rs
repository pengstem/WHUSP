use crate::fs::FileStat;

pub(super) const AT_FDCWD: isize = -100;
pub(super) const AT_REMOVEDIR: u32 = 0x200;
pub(super) const AT_SYMLINK_NOFOLLOW: i32 = 0x100;
pub(super) const AT_NO_AUTOMOUNT: i32 = 0x800;
pub(super) const AT_EMPTY_PATH: i32 = 0x1000;
pub(super) const VALID_FSTATAT_FLAGS: i32 = AT_SYMLINK_NOFOLLOW | AT_NO_AUTOMOUNT | AT_EMPTY_PATH;

pub(super) const IOV_MAX: usize = 1024;
pub(super) const PPOLL_MAX_NFDS: usize = 4096;

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct LinuxIovec {
    pub(super) base: usize,
    pub(super) len: usize,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct LinuxPollFd {
    pub(super) fd: i32,
    pub(super) events: i16,
    pub(super) revents: i16,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct LinuxTimeSpec {
    pub(in crate::syscall) tv_sec: isize,
    pub(in crate::syscall) tv_nsec: isize,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct LinuxKstat {
    st_dev: u64,
    st_ino: u64,
    st_mode: u32,
    st_nlink: u32,
    st_uid: u32,
    st_gid: u32,
    st_rdev: u64,
    __pad: u64,
    st_size: i64,
    st_blksize: u32,
    __pad2: i32,
    st_blocks: u64,
    st_atime_sec: i64,
    st_atime_nsec: i64,
    st_mtime_sec: i64,
    st_mtime_nsec: i64,
    st_ctime_sec: i64,
    st_ctime_nsec: i64,
    __unused: [u32; 2],
}

impl From<FileStat> for LinuxKstat {
    fn from(stat: FileStat) -> Self {
        Self {
            st_dev: stat.dev,
            st_ino: stat.ino,
            st_mode: stat.mode,
            st_nlink: stat.nlink,
            st_uid: stat.uid,
            st_gid: stat.gid,
            st_rdev: stat.rdev,
            __pad: 0,
            st_size: stat.size as i64,
            st_blksize: stat.blksize,
            __pad2: 0,
            st_blocks: stat.blocks,
            st_atime_sec: stat.atime_sec as i64,
            st_atime_nsec: stat.atime_nsec as i64,
            st_mtime_sec: stat.mtime_sec as i64,
            st_mtime_nsec: stat.mtime_nsec as i64,
            st_ctime_sec: stat.ctime_sec as i64,
            st_ctime_nsec: stat.ctime_nsec as i64,
            __unused: [0; 2],
        }
    }
}
