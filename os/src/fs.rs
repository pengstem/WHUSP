mod devfs;
mod dirent;
mod ext4;
mod fat;
mod inode;
mod mount;
mod path;
mod pipe;
mod procfs;
pub(crate) mod socket;
mod staticfs;
mod status_flags;
mod stdio;
mod tmpfs;
mod vfs;

use crate::mm::{UserBuffer, page_cache::PageCacheId};
use bitflags::bitflags;
use core::any::Any;

const DEFAULT_BLOCK_SIZE: u32 = 4096;

pub const S_IFIFO: u32 = 0o010000;
pub const S_IFCHR: u32 = 0o020000;
pub const S_IFDIR: u32 = 0o040000;
pub const S_IFBLK: u32 = 0o060000;
pub const S_IFREG: u32 = 0o100000;
pub const S_IFLNK: u32 = 0o120000;
pub const S_IFSOCK: u32 = 0o140000;
pub const S_IFMT: u32 = 0o170000;

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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SeekWhence {
    Set,
    Current,
    End,
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
    fn as_any(&self) -> &dyn Any;
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
    fn stat(&self) -> FsResult<FileStat> {
        Ok(FileStat::default())
    }
    fn read_at(&self, _offset: usize, _buf: &mut [u8]) -> usize {
        0
    }
    fn write_at(&self, _offset: usize, _buf: &[u8]) -> usize {
        0
    }
    fn set_len(&self, _len: usize) -> FsResult {
        Err(FsError::Unsupported)
    }
    fn sync(&self, _data_only: bool) -> FsResult {
        Ok(())
    }
    fn seek(&self, _offset: i64, _whence: SeekWhence) -> FsResult<usize> {
        Err(FsError::IllegalSeek)
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
    fn set_mode(&self, _mode: u32) -> FsResult {
        Err(FsError::Unsupported)
    }
    fn set_owner(&self, _uid: Option<u32>, _gid: Option<u32>) -> FsResult {
        Err(FsError::Unsupported)
    }
    fn working_dir(&self) -> Option<WorkingDir> {
        None
    }
    fn vfs_mount_id(&self) -> Option<mount::MountId> {
        None
    }
    fn page_cache_id(&self) -> Option<PageCacheId> {
        None
    }
    fn status_flags(&self) -> inode::OpenFlags {
        inode::OpenFlags::empty()
    }
    fn set_status_flags(&self, _flags: inode::OpenFlags) {}
    fn pipe_capacity(&self) -> Option<usize> {
        None
    }
    fn set_pipe_capacity(&self, _capacity: usize) -> FsResult<usize> {
        Err(FsError::Unsupported)
    }
    fn pipe_occupied(&self) -> Option<usize> {
        None
    }
    fn pipe_readers_closed(&self) -> bool {
        false
    }
    fn is_tty(&self) -> bool {
        false
    }
    fn is_rtc(&self) -> bool {
        false
    }
    fn is_devfs_dir(&self) -> bool {
        false
    }
    fn is_devfs_misc_dir(&self) -> bool {
        false
    }
    fn is_pipe(&self) -> bool {
        false
    }
    fn is_dev_full(&self) -> bool {
        false
    }
    fn is_socket(&self) -> bool {
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

pub(crate) use devfs::{
    open_child as open_devfs_child, open_misc_child as open_devfs_misc_child,
    stat_child as stat_devfs_child, stat_misc_child as stat_devfs_misc_child,
};
pub use inode::OpenFlags;
pub(crate) use inode::{
    create_node_in, link_file_in, lookup_mount_target_dir_in, mkdir_in, rename_in, rmdir_in,
    symlink_in, unlink_file_in,
};
pub(crate) use mount::{
    MountError, MountId, mount_block_device_at, mount_fat_device_at, mount_is_read_only,
    mount_tmpfs_at, remount_at, statfs_for_mount, unmount_at,
};
pub(crate) use path::{PathContext, WorkingDir, normalize_path_at_root, path_inside_root};
pub(crate) use pipe::default_pipe_capacity_for_current_process;
pub use pipe::make_pipe;
pub(crate) use procfs::pipe_max_size;
pub(crate) use staticfs::{open_path as open_static_path, stat_path as stat_static_path};
pub use stdio::{Stdin, Stdout};
pub(crate) use vfs::open_file;
pub(crate) use vfs::{
    FileCreateAttrs, FileSystemStat, FsError, FsNodeKind, FsResult, chmod_in, chown_in,
    link_open_file_in, lookup_dir_in, lookup_dir_with_stat_in, open_file_in,
    open_file_in_with_attrs, open_tmpfile_in_with_attrs, stat_in, truncate_in,
};

pub(self) fn align_up(value: usize, align: usize) -> usize {
    (value + align - 1) & !(align - 1)
}
