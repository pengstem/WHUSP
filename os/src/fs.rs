mod anonfd;
mod cgroupfs;
mod console_tty;
mod dentry_cache;
mod devfs;
mod dirent;
mod eventfd;
mod ext4;
mod fat;
mod inode;
mod memfd;
mod mount;
mod mount_fd;
mod named_fifo;
mod overlayfs;
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
use alloc::{
    collections::VecDeque,
    string::String,
    sync::{Arc, Weak},
    vec::Vec,
};
use bitflags::bitflags;
use core::{
    any::Any,
    sync::atomic::{AtomicBool, AtomicUsize, Ordering},
};

pub(crate) use anonfd::make_anonymous_fd;
pub(crate) use cgroupfs::{memcg_pressure_active, reclaim_memcg_pressure_pages};
pub(crate) use console_tty::console_tty_drain_uart;
pub(crate) use console_tty::{
    LinuxTermio, LinuxTermios, LinuxTermios2, LinuxWinsize, apply_console_tty_termio,
    console_tty_available_bytes, console_tty_foreground_pgid, console_tty_poll,
    console_tty_poll_with_wait, console_tty_read, console_tty_termio, console_tty_termios,
    console_tty_termios2, console_tty_winsize, set_console_tty_foreground_pgid,
    set_console_tty_termios, set_console_tty_termios2, set_console_tty_winsize,
};
pub(crate) use eventfd::make_eventfd;
pub(crate) use mount_fd::{DetachedMountFile, FsContextFile, FsContextStateError};

const DEFAULT_BLOCK_SIZE: u32 = 4096;

pub const S_IFIFO: u32 = 0o010000;
pub const S_IFCHR: u32 = 0o020000;
pub const S_IFDIR: u32 = 0o040000;
pub const S_IFBLK: u32 = 0o060000;
pub const S_IFREG: u32 = 0o100000;
pub const S_IFLNK: u32 = 0o120000;
pub const S_IFSOCK: u32 = 0o140000;
pub const S_IFMT: u32 = 0o170000;
pub const FS_COMPR_FL: u32 = 0x0000_0004;
pub const FS_IMMUTABLE_FL: u32 = 0x0000_0010;
pub const FS_APPEND_FL: u32 = 0x0000_0020;
pub const FS_NODUMP_FL: u32 = 0x0000_0040;
pub const FS_ENCRYPT_FL: u32 = 0x0000_0800;
pub const FS_VERITY_FL: u32 = 0x0010_0000;
pub const FS_STATX_COMMON_ATTR_FLAGS: u32 = FS_IMMUTABLE_FL | FS_APPEND_FL | FS_NODUMP_FL;
pub const FS_STATX_ATTR_FLAGS: u32 =
    FS_COMPR_FL | FS_ENCRYPT_FL | FS_VERITY_FL | FS_STATX_COMMON_ATTR_FLAGS;

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

pub struct PollWaiter {
    task: Arc<crate::task::TaskControlBlock>,
    triggered: AtomicBool,
    registrations: AtomicUsize,
}

impl PollWaiter {
    pub fn new(task: Arc<crate::task::TaskControlBlock>) -> Arc<Self> {
        Arc::new(Self {
            task,
            triggered: AtomicBool::new(false),
            registrations: AtomicUsize::new(0),
        })
    }

    pub fn was_triggered(&self) -> bool {
        self.triggered.load(Ordering::Acquire)
    }

    pub fn task_matches(&self, task: &Arc<crate::task::TaskControlBlock>) -> bool {
        Arc::ptr_eq(&self.task, task)
    }

    pub fn registration_count(&self) -> usize {
        self.registrations.load(Ordering::Acquire)
    }

    fn record_registration(&self) {
        self.registrations.fetch_add(1, Ordering::Release);
    }

    pub fn wake(&self) -> bool {
        self.triggered.store(true, Ordering::Release);
        crate::task::wakeup_task(Arc::clone(&self.task))
    }

    pub fn wake_all(waiters: Vec<Arc<Self>>) {
        for waiter in waiters {
            waiter.wake();
        }
    }
}

pub struct PollWaitQueue {
    waiters: VecDeque<Weak<PollWaiter>>,
}

impl PollWaitQueue {
    pub fn new() -> Self {
        Self {
            waiters: VecDeque::new(),
        }
    }

    pub fn register(&mut self, waiter: &Arc<PollWaiter>) {
        self.waiters.retain(|waiter| waiter.strong_count() > 0);
        waiter.record_registration();
        self.waiters.push_back(Arc::downgrade(waiter));
    }

    pub fn drain(&mut self) -> Vec<Arc<PollWaiter>> {
        let waiters = core::mem::take(&mut self.waiters);
        waiters
            .into_iter()
            .filter_map(|waiter| waiter.upgrade())
            .collect()
    }
}

impl Default for PollWaitQueue {
    fn default() -> Self {
        Self::new()
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
    pub inode_flags: u32,
    pub inode_flags_supported: u32,
    pub size: u64,
    pub blksize: u32,
    pub blocks: u64,
    pub atime_sec: u64,
    pub atime_nsec: u32,
    pub btime_sec: u64,
    pub btime_nsec: u32,
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
    Data,
    Hole,
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

/// Open file description interface used by syscall, VFS, and special fd code.
///
/// Required methods cover ordinary byte I/O. Optional methods default to the
/// Linux-visible behavior for anonymous or non-capable file types, so concrete
/// files override only the capabilities they actually support.
pub trait File: Send + Sync {
    fn as_any(&self) -> &dyn Any;
    fn readable(&self) -> bool;
    fn writable(&self) -> bool;
    fn read(&self, buf: UserBuffer) -> usize;
    fn write(&self, buf: UserBuffer) -> usize;
    /// Falls back to normal write for files without append-specific offsets.
    fn write_append(&self, buf: UserBuffer) -> usize {
        self.write(buf)
    }
    /// Reports regular-file style readiness from access mode by default.
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
    fn poll_with_wait(&self, events: PollEvents, _waiter: Option<&Arc<PollWaiter>>) -> PollEvents {
        self.poll(events)
    }
    /// Anonymous fds without filesystem metadata report an empty stat block.
    fn stat(&self) -> FsResult<FileStat> {
        Ok(FileStat::default())
    }
    /// Non-positionable files return EOF for positioned reads by default.
    fn read_at(&self, _offset: usize, _buf: &mut [u8]) -> usize {
        0
    }
    /// Non-positionable files accept no positioned write data by default.
    fn write_at(&self, _offset: usize, _buf: &[u8]) -> usize {
        0
    }
    fn set_len(&self, _len: usize) -> FsResult {
        Err(FsError::Unsupported)
    }
    fn allocate_range(&self, offset: usize, len: usize, keep_size: bool) -> FsResult {
        let end = offset.checked_add(len).ok_or(FsError::InvalidInput)?;
        let current_size = usize::try_from(self.stat()?.size).map_err(|_| FsError::InvalidInput)?;
        if !keep_size && end > current_size {
            self.check_set_len(end)?;
            self.set_len(end)?;
        }
        Ok(())
    }
    fn zero_range(&self, _offset: usize, _len: usize, _keep_size: bool) -> FsResult {
        Err(FsError::Unsupported)
    }
    fn punch_hole(&self, _offset: usize, _len: usize) -> FsResult {
        Err(FsError::Unsupported)
    }
    fn collapse_range(&self, offset: usize, len: usize) -> FsResult {
        if !self.writable() {
            return Err(FsError::PermissionDenied);
        }
        let stat = self.stat()?;
        if stat.mode & S_IFMT != S_IFREG {
            return Err(FsError::InvalidInput);
        }
        let size = usize::try_from(stat.size).map_err(|_| FsError::InvalidInput)?;
        let end = offset.checked_add(len).ok_or(FsError::InvalidInput)?;
        if end >= size {
            return Err(FsError::InvalidInput);
        }
        let new_size = size - len;
        self.check_set_len(new_size)?;

        let mut buffer = Vec::new();
        if buffer.try_reserve(64 * 1024).is_err() {
            return Err(FsError::NoSpace);
        }
        buffer.resize(64 * 1024, 0);

        let mut read_offset = end;
        let mut write_offset = offset;
        while read_offset < size {
            let want = buffer.len().min(size - read_offset);
            buffer[..want].fill(0);
            let read_len = self.read_at(read_offset, &mut buffer[..want]);
            if read_len == 0 {
                return Err(FsError::Io);
            }
            let written = self.write_at(write_offset, &buffer[..read_len]);
            if written != read_len {
                return Err(FsError::Io);
            }
            read_offset += read_len;
            write_offset += read_len;
        }

        self.set_len(new_size)
    }
    fn insert_range(&self, offset: usize, len: usize) -> FsResult {
        if !self.writable() {
            return Err(FsError::PermissionDenied);
        }
        let stat = self.stat()?;
        if stat.mode & S_IFMT != S_IFREG {
            return Err(FsError::InvalidInput);
        }
        let size = usize::try_from(stat.size).map_err(|_| FsError::InvalidInput)?;
        if offset >= size {
            return Err(FsError::InvalidInput);
        }
        let new_size = size.checked_add(len).ok_or(FsError::InvalidInput)?;
        self.check_set_len(new_size)?;
        self.punch_hole(offset, 0)?;
        self.set_len(new_size)?;

        let mut buffer = Vec::new();
        if buffer.try_reserve(64 * 1024).is_err() {
            return Err(FsError::NoSpace);
        }
        buffer.resize(64 * 1024, 0);

        let mut remaining = size - offset;
        while remaining > 0 {
            let chunk_len = buffer.len().min(remaining);
            let read_offset = offset + remaining - chunk_len;
            let write_offset = read_offset + len;
            buffer[..chunk_len].fill(0);
            let read_len = self.read_at(read_offset, &mut buffer[..chunk_len]);
            if read_len != chunk_len {
                return Err(FsError::Io);
            }
            let written = self.write_at(write_offset, &buffer[..read_len]);
            if written != read_len {
                return Err(FsError::Io);
            }
            remaining -= chunk_len;
        }

        self.punch_hole(offset, len)
    }
    /// Preflight write hooks default to success; constrained files override.
    fn check_write(&self, _len: usize, _append: bool) -> FsResult {
        Ok(())
    }
    fn write_ignores_user_buffer(&self) -> bool {
        false
    }
    fn check_read(&self, _len: usize) -> FsResult {
        Ok(())
    }
    fn check_write_at(&self, _offset: usize, _len: usize) -> FsResult {
        Ok(())
    }
    fn check_set_len(&self, _len: usize) -> FsResult {
        Ok(())
    }
    /// Only memfd-like files expose Linux file seals.
    fn seals(&self) -> FsResult<u32> {
        Err(FsError::InvalidInput)
    }
    fn add_seals(&self, _seals: u32) -> FsResult {
        Err(FsError::InvalidInput)
    }
    fn reopen_from_proc_fd(
        &self,
        _flags: inode::OpenFlags,
    ) -> FsResult<Arc<dyn File + Send + Sync>> {
        Err(FsError::Unsupported)
    }
    fn proc_fd_target(&self) -> Option<String> {
        None
    }
    /// mmap/write exclusion hooks are opt-in for regular file descriptions.
    fn inc_writable_shared_mmap(&self) {}
    fn dec_writable_shared_mmap(&self) {}
    fn blocks_shared_writable_mmap(&self) -> bool {
        false
    }
    fn blocks_file_write(&self) -> bool {
        false
    }
    /// Most in-memory/special files have no dirty backing store to flush.
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
    fn inode_flags(&self) -> FsResult<u32> {
        Err(FsError::Unsupported)
    }
    fn set_inode_flags(&self, _flags: u32) -> FsResult {
        Err(FsError::Unsupported)
    }
    fn working_dir(&self) -> Option<WorkingDir> {
        None
    }
    fn vfs_node_id(&self) -> Option<vfs::VfsNodeId> {
        None
    }
    fn vfs_parent_node_id(&self) -> Option<vfs::VfsNodeId> {
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
    fn clone_for_fanotify_event(
        &self,
        _flags: inode::OpenFlags,
    ) -> FsResult<Arc<dyn File + Send + Sync>> {
        Err(FsError::Unsupported)
    }
    fn suppresses_fanotify(&self) -> bool {
        false
    }
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
    fn splice_pipe_to_pipe(
        &self,
        _out: &(dyn File + Send + Sync),
        _len: usize,
    ) -> FsResult<Option<usize>> {
        Ok(None)
    }
    fn socket_write_peer_closed(&self) -> bool {
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
    fn is_devfs_pts_dir(&self) -> bool {
        false
    }
    fn is_devfs_input_dir(&self) -> bool {
        false
    }
    fn is_devfs_net_dir(&self) -> bool {
        false
    }
    fn is_dev_random(&self) -> bool {
        false
    }
    fn is_pipe(&self) -> bool {
        false
    }
    fn is_dev_full(&self) -> bool {
        false
    }
    fn is_eventfd(&self) -> bool {
        false
    }
    fn is_io_uring(&self) -> bool {
        false
    }
    fn is_memfd(&self) -> bool {
        false
    }
    fn is_socket(&self) -> bool {
        false
    }
    fn supports_splice_read(&self) -> bool {
        if !self.readable() {
            return false;
        }
        if self.is_pipe() || self.is_socket() {
            return true;
        }
        match self.stat() {
            Ok(stat) => stat.mode & S_IFMT == S_IFREG,
            Err(_) => false,
        }
    }
    fn supports_splice_write(&self) -> bool {
        if !self.writable() {
            return false;
        }
        if self.is_pipe() || self.is_socket() {
            return true;
        }
        match self.stat() {
            Ok(stat) => stat.mode & S_IFMT == S_IFREG,
            Err(_) => false,
        }
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
    attach_loop_device, detach_loop_device, devfs_input_event_name, devfs_input_event_set_grabbed,
    devfs_loop_device_id, devfs_pty_lock_state, devfs_pty_number, devfs_uinput_create,
    devfs_uinput_destroy, devfs_uinput_set_evbit, devfs_uinput_set_keybit, devfs_uinput_set_relbit,
    find_free_loop_device, is_devfs_input_event, is_devfs_loop_control, is_devfs_tun,
    is_devfs_uinput, loop_device_change_fd, loop_device_flags, loop_device_is_attached,
    loop_device_is_read_only, loop_device_read_ahead, loop_device_refresh_size,
    loop_device_set_block_size, loop_device_set_direct_io, loop_device_set_read_ahead,
    loop_device_set_read_only, loop_device_set_status, loop_device_size, loop_device_size_limit,
    loop_device_sysfs_content, open_child as open_devfs_child,
    open_input_child as open_devfs_input_child, open_misc_child as open_devfs_misc_child,
    open_net_child as open_devfs_net_child, open_pts_child as open_devfs_pts_child,
    set_devfs_pty_locked, stat_child as stat_devfs_child,
    stat_input_child as stat_devfs_input_child, stat_misc_child as stat_devfs_misc_child,
    stat_net_child as stat_devfs_net_child, stat_pts_child as stat_devfs_pts_child,
};
pub use inode::OpenFlags;
pub(crate) use inode::{
    create_node_in, link_file_in, lookup_existing_dir_in, lookup_mount_target_dir_in, mkdir_in,
    rename_exchange_in, rename_in, rmdir_in, symlink_in, unlink_file_in,
};
pub(crate) use memfd::make_memfd;
pub(crate) use mount::{
    MountError, MountId, MountNamespaceId, MountPropagation, ROOT_MOUNT_NAMESPACE,
    assign_pid_to_cgroup, clone_mount_namespace, mount_bind_at, mount_block_device_at,
    mount_cgroup_memory_at, mount_cgroup2_at, mount_ext_scratch_at, mount_fat_device_at,
    mount_is_noexec, mount_is_read_only, mount_nfs_compat_at, mount_overlay_compat_at,
    mount_proc_at, mount_stat_flags_from_linux_mount_flags, mount_tmpfs_at, mounted_source_at,
    move_mount_at, nfs_compat_source_path, overlay_real_node, remount_at, root_ino_for,
    set_mount_propagation_at, set_mount_stat_flags, shutdown_all_mounts, statfs_for_mount,
    sync_all_mounts, unmount_at,
};
pub(crate) use path::{PathContext, WorkingDir, normalize_path_at_root, path_inside_root};
pub(crate) use pipe::default_pipe_capacity_for_current_process;
pub use pipe::make_pipe;
pub(crate) use procfs::{
    ProcNamespaceInfo, ProcNamespaceKind, core_pattern_for_pid, note_madvise_willneed,
    proc_namespace_info_from_path, proc_namespace_info_from_stat_ino, proc_namespace_kind_name,
    proc_namespace_stat_ino,
};
pub(crate) use procfs::{note_readahead as procfs_note_readahead, pipe_max_size};
pub(crate) use staticfs::{open_path as open_static_path, stat_path as stat_static_path};
pub use stdio::{Stdin, Stdout};
pub(crate) use vfs::VfsNodeId;
pub(crate) use vfs::open_file;
pub(crate) use vfs::{
    FileCreateAttrs, FileSystemStat, FsError, FsNodeKind, FsResult, chmod_in, chown_in,
    flush_dirty_regular_files_on_mount, link_open_file_in, lookup_dir_with_stat_in,
    lookup_dir_with_stat_path_in, lookup_path_in, open_file_handle_node, open_file_in,
    open_file_in_with_attrs, open_tmpfile_in_with_attrs, regular_file_is_open_writable_in,
    regular_file_node_is_open_writable, stat_in, track_regular_file_executable, truncate_in,
    untrack_regular_file_executable,
};

fn align_up(value: usize, align: usize) -> usize {
    (value + align - 1) & !(align - 1)
}
