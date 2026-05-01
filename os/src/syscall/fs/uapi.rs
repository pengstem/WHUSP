use crate::fs::FileStat;

pub(super) const AT_FDCWD: isize = -100;
pub(super) const AT_REMOVEDIR: u32 = 0x200;
pub(super) const AT_SYMLINK_NOFOLLOW: i32 = 0x100;
pub(super) const AT_EACCESS: i32 = 0x200;
pub(super) const AT_NO_AUTOMOUNT: i32 = 0x800;
pub(super) const AT_EMPTY_PATH: i32 = 0x1000;
pub(super) const VALID_FSTATAT_FLAGS: i32 = AT_SYMLINK_NOFOLLOW | AT_NO_AUTOMOUNT | AT_EMPTY_PATH;
pub(super) const VALID_UTIMENSAT_FLAGS: i32 = AT_SYMLINK_NOFOLLOW | AT_EMPTY_PATH;
pub(super) const UTIME_NOW: isize = 0x3fffffff;
pub(super) const UTIME_OMIT: isize = 0x3ffffffe;
pub(super) const F_OK: i32 = 0;
pub(super) const X_OK: i32 = 1;
pub(super) const W_OK: i32 = 2;
pub(super) const R_OK: i32 = 4;
pub(super) const VALID_ACCESS_MODE: i32 = F_OK | X_OK | W_OK | R_OK;
pub(super) const VALID_FACCESSAT_FLAGS: i32 = AT_EACCESS;
pub(super) const AT_STATX_FORCE_SYNC: i32 = 0x2000;
pub(super) const AT_STATX_DONT_SYNC: i32 = 0x4000;
pub(super) const AT_STATX_SYNC_TYPE: i32 = AT_STATX_FORCE_SYNC | AT_STATX_DONT_SYNC;
pub(super) const VALID_STATX_FLAGS: i32 = VALID_FSTATAT_FLAGS | AT_STATX_SYNC_TYPE;
pub(super) const RENAME_NOREPLACE: u32 = 1 << 0;
pub(super) const RENAME_EXCHANGE: u32 = 1 << 1;
pub(super) const RENAME_WHITEOUT: u32 = 1 << 2;
pub(super) const VALID_RENAME_FLAGS: u32 = RENAME_NOREPLACE | RENAME_EXCHANGE | RENAME_WHITEOUT;
pub(super) const STATX_TYPE: u32 = 0x0001;
pub(super) const STATX_MODE: u32 = 0x0002;
pub(super) const STATX_NLINK: u32 = 0x0004;
pub(super) const STATX_UID: u32 = 0x0008;
pub(super) const STATX_GID: u32 = 0x0010;
pub(super) const STATX_ATIME: u32 = 0x0020;
pub(super) const STATX_MTIME: u32 = 0x0040;
pub(super) const STATX_CTIME: u32 = 0x0080;
pub(super) const STATX_INO: u32 = 0x0100;
pub(super) const STATX_SIZE: u32 = 0x0200;
pub(super) const STATX_BLOCKS: u32 = 0x0400;
pub(super) const STATX_BASIC_STATS: u32 = STATX_TYPE
    | STATX_MODE
    | STATX_NLINK
    | STATX_UID
    | STATX_GID
    | STATX_ATIME
    | STATX_MTIME
    | STATX_CTIME
    | STATX_INO
    | STATX_SIZE
    | STATX_BLOCKS;
pub(super) const STATX_RESERVED: u32 = 0x8000_0000;

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

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct LinuxStatfs {
    f_type: i64,
    f_bsize: i64,
    f_blocks: u64,
    f_bfree: u64,
    f_bavail: u64,
    f_files: u64,
    f_ffree: u64,
    f_fsid: [i32; 2],
    f_namelen: i64,
    f_frsize: i64,
    f_flags: i64,
    f_spare: [i64; 4],
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct LinuxStatxTimestamp {
    tv_sec: i64,
    tv_nsec: u32,
    __reserved: i32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct LinuxStatx {
    stx_mask: u32,
    stx_blksize: u32,
    stx_attributes: u64,
    stx_nlink: u32,
    stx_uid: u32,
    stx_gid: u32,
    stx_mode: u16,
    __spare0: u16,
    stx_ino: u64,
    stx_size: u64,
    stx_blocks: u64,
    stx_attributes_mask: u64,
    stx_atime: LinuxStatxTimestamp,
    stx_btime: LinuxStatxTimestamp,
    stx_ctime: LinuxStatxTimestamp,
    stx_mtime: LinuxStatxTimestamp,
    stx_rdev_major: u32,
    stx_rdev_minor: u32,
    stx_dev_major: u32,
    stx_dev_minor: u32,
    __spare2: [u64; 14],
}

fn linux_dev_major(dev: u64) -> u32 {
    (((dev >> 8) & 0xfff) | ((dev >> 32) & !0xfff)) as u32
}

fn linux_dev_minor(dev: u64) -> u32 {
    ((dev & 0xff) | ((dev >> 12) & !0xff)) as u32
}

impl LinuxStatxTimestamp {
    fn new(sec: u64, nsec: u32) -> Self {
        Self {
            tv_sec: sec as i64,
            tv_nsec: nsec,
            __reserved: 0,
        }
    }
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

impl From<crate::fs::FileSystemStat> for LinuxStatfs {
    fn from(stat: crate::fs::FileSystemStat) -> Self {
        Self {
            f_type: stat.magic,
            f_bsize: stat.block_size as i64,
            f_blocks: stat.blocks,
            f_bfree: stat.free_blocks,
            f_bavail: stat.available_blocks,
            f_files: stat.files,
            f_ffree: stat.free_files,
            f_fsid: [0; 2],
            f_namelen: stat.max_name_len as i64,
            f_frsize: stat.block_size as i64,
            f_flags: stat.flags as i64,
            f_spare: [0; 4],
        }
    }
}

impl From<FileStat> for LinuxStatx {
    fn from(stat: FileStat) -> Self {
        Self {
            stx_mask: STATX_BASIC_STATS,
            stx_blksize: stat.blksize,
            stx_attributes: 0,
            stx_nlink: stat.nlink,
            stx_uid: stat.uid,
            stx_gid: stat.gid,
            stx_mode: (stat.mode & 0xFFFF) as u16,
            __spare0: 0,
            stx_ino: stat.ino,
            stx_size: stat.size,
            stx_blocks: stat.blocks,
            stx_attributes_mask: 0,
            stx_atime: LinuxStatxTimestamp::new(stat.atime_sec, stat.atime_nsec),
            stx_btime: LinuxStatxTimestamp::default(),
            stx_ctime: LinuxStatxTimestamp::new(stat.ctime_sec, stat.ctime_nsec),
            stx_mtime: LinuxStatxTimestamp::new(stat.mtime_sec, stat.mtime_nsec),
            stx_rdev_major: linux_dev_major(stat.rdev),
            stx_rdev_minor: linux_dev_minor(stat.rdev),
            stx_dev_major: linux_dev_major(stat.dev),
            stx_dev_minor: linux_dev_minor(stat.dev),
            __spare2: [0; 14],
        }
    }
}
