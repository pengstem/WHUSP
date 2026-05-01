mod devfs;
mod dirent;
mod ext4;
mod fat;
mod inode;
mod mount;
mod path;
mod pipe;
mod procfs;
mod status_flags;
mod stdio;
mod tmpfs;
mod vfs;

use crate::mm::UserBuffer;
use bitflags::bitflags;

const DEFAULT_BLOCK_SIZE: u32 = 4096;

pub const S_IFIFO: u32 = 0o010000;
pub const S_IFCHR: u32 = 0o020000;
pub const S_IFDIR: u32 = 0o040000;
pub const S_IFREG: u32 = 0o100000;
pub const S_IFLNK: u32 = 0o120000;

bitflags! {
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct PollEvents: u16 {
        const POLLIN = 0x0001;
        const POLLPRI = 0x0002;
        const POLLOUT = 0x0004;
        const POLLERR = 0x0008;
        const POLLHUP = 0x0010;
        const POLLNVAL = 0x0020;
        const POLLRDHUP = 0x2000;
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct FileStat {
    pub dev: u64,
    pub ino: u64,
    pub mode: u32,
    pub nlink: u32,
    pub uid: u32,
    pub gid: u32,
    pub rdev: u64,
    pub size: u64,
    pub blksize: u32,
    pub blocks: u64,
    pub atime_sec: u64,
    pub atime_nsec: u32,
    pub mtime_sec: u64,
    pub mtime_nsec: u32,
    pub ctime_sec: u64,
    pub ctime_nsec: u32,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct FileTimestamp {
    pub sec: u64,
    pub nsec: u32,
}

impl FileTimestamp {
    pub fn now() -> Self {
        Self::from_nanos(crate::timer::wall_time_nanos())
    }

    pub fn from_nanos(nanos: u64) -> Self {
        Self {
            sec: nanos / 1_000_000_000,
            nsec: (nanos % 1_000_000_000) as u32,
        }
    }

    pub fn to_duration(self) -> core::time::Duration {
        core::time::Duration::new(self.sec, self.nsec)
    }
}

impl FileStat {
    pub fn with_mode(mode: u32) -> Self {
        Self {
            mode,
            nlink: 1,
            blksize: DEFAULT_BLOCK_SIZE,
            ..Self::default()
        }
    }
}

pub trait File: Send + Sync {
    fn readable(&self) -> bool;
    fn writable(&self) -> bool;
    fn read(&self, buf: UserBuffer) -> usize;
    fn write(&self, buf: UserBuffer) -> usize;
    fn write_append(&self, buf: UserBuffer) -> usize {
        self.write(buf)
    }
    fn poll(&self, events: PollEvents) -> PollEvents {
        let mut ready = PollEvents::empty();
        if events.intersects(PollEvents::POLLIN | PollEvents::POLLPRI | PollEvents::POLLRDHUP)
            && self.readable()
        {
            ready |= PollEvents::POLLIN;
        }
        if events.contains(PollEvents::POLLOUT) && self.writable() {
            ready |= PollEvents::POLLOUT;
        }
        ready
    }
    fn stat(&self) -> FileStat {
        FileStat::default()
    }
    fn read_at(&self, _offset: usize, _buf: &mut [u8]) -> usize {
        0
    }
    fn write_at(&self, _offset: usize, _buf: &[u8]) -> usize {
        0
    }
    fn read_dirent64(&self, _buf: UserBuffer) -> FsResult<isize> {
        Err(FsError::NotDir)
    }
    fn readlink(&self, _buf: &mut [u8]) -> FsResult<usize> {
        Err(FsError::InvalidInput)
    }
    fn set_times(
        &self,
        _atime: Option<FileTimestamp>,
        _mtime: Option<FileTimestamp>,
        _ctime: FileTimestamp,
    ) -> FsResult {
        Err(FsError::Unsupported)
    }
    fn working_dir(&self) -> Option<WorkingDir> {
        None
    }
    fn vfs_mount_id(&self) -> Option<mount::MountId> {
        None
    }
    fn status_flags(&self) -> inode::OpenFlags {
        inode::OpenFlags::empty()
    }
    fn set_status_flags(&self, _flags: inode::OpenFlags) {}
    fn is_tty(&self) -> bool {
        false
    }
    fn is_rtc(&self) -> bool {
        false
    }
    fn is_devfs_dir(&self) -> bool {
        false
    }
}

pub fn init() {
    mount::init_mounts();
}

pub fn list_apps() {
    mount::mount_status_log();
    println!("/**** APPS ****");
    for app in mount::list_root_apps() {
        println!("{}", app);
    }
    println!("**************/")
}

pub(crate) use devfs::{open_child as open_devfs_child, stat_child as stat_devfs_child};
pub use inode::OpenFlags;
pub(crate) use inode::{
    link_file_at, lookup_mount_target_dir_at, mkdir_at, rename_at, rmdir_at, symlink_at,
    unlink_file_at,
};
pub(crate) use mount::{
    MountError, MountId, mount_block_device_at, mount_fat_device_at, mount_tmpfs_at,
    statfs_for_mount, unmount_at,
};
pub(crate) use path::{WorkingDir, normalize_path};
pub use pipe::make_pipe;
pub use stdio::{Stdin, Stdout};
pub(crate) use vfs::open_file;
pub(crate) use vfs::{FileSystemStat, FsError, FsResult, lookup_dir_at, open_file_at, stat_at};

pub(self) fn align_up(value: usize, align: usize) -> usize {
    (value + align - 1) & !(align - 1)
}
