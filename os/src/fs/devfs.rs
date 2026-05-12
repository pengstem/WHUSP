use super::dirent::{DT_BLK, DT_CHR, DT_DIR, LINUX_DIRENT64_ALIGN, LINUX_DIRENT64_HEADER_SIZE};
use super::status_flags::StatusFlagsCell;
use super::{
    File, FileStat, FsError, FsResult, OpenFlags, PollEvents, S_IFBLK, S_IFCHR, S_IFDIR, SeekWhence,
};
use crate::drivers::chardev::{CharDevice, UART};
use crate::mm::UserBuffer;
use crate::sync::UPIntrFreeCell;
use crate::task::{
    TaskControlBlock, block_current_task_no_schedule, current_has_unmasked_signal, schedule,
    wakeup_task,
};
use alloc::collections::VecDeque;
use alloc::string::ToString;
use alloc::sync::Arc;
use alloc::{vec, vec::Vec};
use lazy_static::lazy_static;

const DEVFS_DEV: u64 = 0x646576;
const LOOP_DEVICE_SIZE_FALLBACK: u64 = 300 * 1024 * 1024;
const PTY_BUFFER_CAPACITY: usize = 8192;
const PTY_TABLE_SIZE: usize = 64;
const PTY_INO_BASE: u64 = 0x1000;

lazy_static! {
    static ref LOOP0_BACKEND: UPIntrFreeCell<Option<Arc<dyn File + Send + Sync>>> =
        unsafe { UPIntrFreeCell::new(None) };
    static ref PTY_TABLE: UPIntrFreeCell<PtyTable> =
        unsafe { UPIntrFreeCell::new(PtyTable::new()) };
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DevNode {
    Root,
    Misc,
    Pts,
    Null,
    Zero,
    Full,
    Random,
    Urandom,
    Tty,
    TtyS0,
    Tty8,
    Tty9,
    PtMx,
    Rtc,
    LoopControl,
    Loop0,
}

impl DevNode {
    fn ino(self) -> u64 {
        match self {
            Self::Root => 1,
            Self::Misc => 2,
            Self::Pts => 3,
            Self::Null => 4,
            Self::Zero => 5,
            Self::Full => 6,
            Self::Random => 7,
            Self::Urandom => 8,
            Self::Tty => 9,
            Self::TtyS0 => 10,
            Self::Tty8 => 11,
            Self::Tty9 => 12,
            Self::PtMx => 13,
            Self::Rtc => 14,
            Self::LoopControl => 15,
            Self::Loop0 => 16,
        }
    }

    fn rdev(self) -> u64 {
        match self {
            Self::Root | Self::Misc | Self::Pts => 0,
            Self::Null => linux_makedev(1, 3),
            Self::Zero => linux_makedev(1, 5),
            Self::Full => linux_makedev(1, 7),
            Self::Random => linux_makedev(1, 8),
            Self::Urandom => linux_makedev(1, 9),
            Self::Tty => linux_makedev(5, 0),
            Self::TtyS0 => linux_makedev(4, 64),
            Self::Tty8 => linux_makedev(4, 8),
            Self::Tty9 => linux_makedev(4, 9),
            Self::PtMx => linux_makedev(5, 2),
            Self::Rtc => linux_makedev(253, 0),
            Self::LoopControl => linux_makedev(10, 237),
            Self::Loop0 => linux_makedev(7, 0),
        }
    }

    fn is_tty(self) -> bool {
        matches!(self, Self::Tty | Self::TtyS0 | Self::Tty8 | Self::Tty9)
    }
}

struct DevFsFile {
    node: DevNode,
    readable: bool,
    writable: bool,
    offset: UPIntrFreeCell<usize>,
    status_flags: StatusFlagsCell,
}

impl DevFsFile {
    fn new(node: DevNode, readable: bool, writable: bool, status_flags: OpenFlags) -> Self {
        Self {
            node,
            readable,
            writable,
            offset: unsafe { UPIntrFreeCell::new(0) },
            status_flags: StatusFlagsCell::new(status_flags),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PtyEndpoint {
    Master,
    Slave,
}

struct PtyFile {
    pair: Arc<UPIntrFreeCell<PtyPair>>,
    endpoint: PtyEndpoint,
    readable: bool,
    writable: bool,
    status_flags: StatusFlagsCell,
}

impl PtyFile {
    fn new(
        pair: Arc<UPIntrFreeCell<PtyPair>>,
        endpoint: PtyEndpoint,
        readable: bool,
        writable: bool,
        status_flags: OpenFlags,
    ) -> Self {
        Self {
            pair,
            endpoint,
            readable,
            writable,
            status_flags: StatusFlagsCell::new(status_flags),
        }
    }

    fn id(&self) -> u32 {
        self.pair.exclusive_access().id
    }

    fn lock_state(&self) -> bool {
        self.pair.exclusive_access().locked
    }

    fn set_locked(&self, locked: bool) {
        self.pair.exclusive_access().locked = locked;
    }
}

struct PtyBuffer {
    data: VecDeque<u8>,
    read_wait_queue: VecDeque<Arc<TaskControlBlock>>,
    write_wait_queue: VecDeque<Arc<TaskControlBlock>>,
}

impl PtyBuffer {
    fn new() -> Self {
        Self {
            data: VecDeque::new(),
            read_wait_queue: VecDeque::new(),
            write_wait_queue: VecDeque::new(),
        }
    }

    fn available_read(&self) -> usize {
        self.data.len()
    }

    fn available_write(&self) -> usize {
        PTY_BUFFER_CAPACITY.saturating_sub(self.data.len())
    }

    fn read_byte(&mut self) -> u8 {
        self.data.pop_front().unwrap_or(0)
    }

    fn write_byte(&mut self, byte: u8) {
        if self.data.len() < PTY_BUFFER_CAPACITY {
            self.data.push_back(byte);
        }
    }

    fn sleep_reader(&mut self) -> *mut crate::task::TaskContext {
        let (task, task_cx_ptr) = block_current_task_no_schedule();
        self.read_wait_queue.push_back(task);
        task_cx_ptr
    }

    fn sleep_writer(&mut self) -> *mut crate::task::TaskContext {
        let (task, task_cx_ptr) = block_current_task_no_schedule();
        self.write_wait_queue.push_back(task);
        task_cx_ptr
    }

    fn wake_reader(&mut self) -> Option<Arc<TaskControlBlock>> {
        self.read_wait_queue.pop_front()
    }

    fn wake_writer(&mut self) -> Option<Arc<TaskControlBlock>> {
        self.write_wait_queue.pop_front()
    }

    fn wake_all_readers(&mut self) -> VecDeque<Arc<TaskControlBlock>> {
        core::mem::take(&mut self.read_wait_queue)
    }

    fn wake_all_writers(&mut self) -> VecDeque<Arc<TaskControlBlock>> {
        core::mem::take(&mut self.write_wait_queue)
    }
}

struct PtyPair {
    id: u32,
    locked: bool,
    master_open: usize,
    slave_open: usize,
    master_to_slave: PtyBuffer,
    slave_to_master: PtyBuffer,
}

impl PtyPair {
    fn new(id: usize) -> Self {
        Self {
            id: id as u32,
            locked: true,
            master_open: 1,
            slave_open: 0,
            master_to_slave: PtyBuffer::new(),
            slave_to_master: PtyBuffer::new(),
        }
    }

    fn input_buffer(&self, endpoint: PtyEndpoint) -> &PtyBuffer {
        match endpoint {
            PtyEndpoint::Master => &self.slave_to_master,
            PtyEndpoint::Slave => &self.master_to_slave,
        }
    }

    fn input_buffer_mut(&mut self, endpoint: PtyEndpoint) -> &mut PtyBuffer {
        match endpoint {
            PtyEndpoint::Master => &mut self.slave_to_master,
            PtyEndpoint::Slave => &mut self.master_to_slave,
        }
    }

    fn output_buffer(&self, endpoint: PtyEndpoint) -> &PtyBuffer {
        match endpoint {
            PtyEndpoint::Master => &self.master_to_slave,
            PtyEndpoint::Slave => &self.slave_to_master,
        }
    }

    fn output_buffer_mut(&mut self, endpoint: PtyEndpoint) -> &mut PtyBuffer {
        match endpoint {
            PtyEndpoint::Master => &mut self.master_to_slave,
            PtyEndpoint::Slave => &mut self.slave_to_master,
        }
    }

    fn peer_open(&self, endpoint: PtyEndpoint) -> bool {
        match endpoint {
            PtyEndpoint::Master => self.slave_open > 0,
            PtyEndpoint::Slave => self.master_open > 0,
        }
    }

    fn is_closed(&self) -> bool {
        self.master_open == 0 && self.slave_open == 0
    }
}

struct PtyTable {
    slots: Vec<Option<Arc<UPIntrFreeCell<PtyPair>>>>,
    next_hint: usize,
}

impl PtyTable {
    fn new() -> Self {
        Self {
            slots: vec![None; PTY_TABLE_SIZE],
            next_hint: 0,
        }
    }

    fn allocate(&mut self) -> FsResult<Arc<UPIntrFreeCell<PtyPair>>> {
        for offset in 0..PTY_TABLE_SIZE {
            let id = (self.next_hint + offset) % PTY_TABLE_SIZE;
            if self.slots[id].is_none() {
                let pair = Arc::new(unsafe { UPIntrFreeCell::new(PtyPair::new(id)) });
                self.slots[id] = Some(pair.clone());
                self.next_hint = (id + 1) % PTY_TABLE_SIZE;
                return Ok(pair);
            }
        }
        Err(FsError::NoSpace)
    }

    fn get(&self, id: usize) -> Option<Arc<UPIntrFreeCell<PtyPair>>> {
        self.slots.get(id).and_then(|slot| slot.clone())
    }

    fn active_ids(&self) -> Vec<usize> {
        self.slots
            .iter()
            .enumerate()
            .filter_map(|(id, slot)| slot.as_ref().map(|_| id))
            .collect()
    }

    fn remove_if_same(&mut self, id: usize, pair: &Arc<UPIntrFreeCell<PtyPair>>) {
        if let Some(slot) = self.slots.get_mut(id)
            && let Some(current) = slot
            && Arc::ptr_eq(current, pair)
        {
            *slot = None;
        }
    }
}

struct DevDirEntry {
    node: DevNode,
    name: &'static [u8],
    dtype: u8,
}

const ROOT_DEV_DIR_ENTRIES: [DevDirEntry; 18] = [
    DevDirEntry {
        node: DevNode::Root,
        name: b".",
        dtype: DT_DIR,
    },
    DevDirEntry {
        node: DevNode::Root,
        name: b"..",
        dtype: DT_DIR,
    },
    DevDirEntry {
        node: DevNode::Null,
        name: b"null",
        dtype: DT_CHR,
    },
    DevDirEntry {
        node: DevNode::Zero,
        name: b"zero",
        dtype: DT_CHR,
    },
    DevDirEntry {
        node: DevNode::Full,
        name: b"full",
        dtype: DT_CHR,
    },
    DevDirEntry {
        node: DevNode::Random,
        name: b"random",
        dtype: DT_CHR,
    },
    DevDirEntry {
        node: DevNode::Urandom,
        name: b"urandom",
        dtype: DT_CHR,
    },
    DevDirEntry {
        node: DevNode::Tty,
        name: b"tty",
        dtype: DT_CHR,
    },
    DevDirEntry {
        node: DevNode::TtyS0,
        name: b"ttyS0",
        dtype: DT_CHR,
    },
    DevDirEntry {
        node: DevNode::Tty8,
        name: b"tty8",
        dtype: DT_CHR,
    },
    DevDirEntry {
        node: DevNode::Tty9,
        name: b"tty9",
        dtype: DT_CHR,
    },
    DevDirEntry {
        node: DevNode::PtMx,
        name: b"ptmx",
        dtype: DT_CHR,
    },
    DevDirEntry {
        node: DevNode::Pts,
        name: b"pts",
        dtype: DT_DIR,
    },
    DevDirEntry {
        node: DevNode::Rtc,
        name: b"rtc",
        dtype: DT_CHR,
    },
    DevDirEntry {
        node: DevNode::Rtc,
        name: b"rtc0",
        dtype: DT_CHR,
    },
    DevDirEntry {
        node: DevNode::LoopControl,
        name: b"loop-control",
        dtype: DT_CHR,
    },
    DevDirEntry {
        node: DevNode::Loop0,
        name: b"loop0",
        dtype: DT_BLK,
    },
    DevDirEntry {
        node: DevNode::Misc,
        name: b"misc",
        dtype: DT_DIR,
    },
];

const MISC_DEV_DIR_ENTRIES: [DevDirEntry; 3] = [
    DevDirEntry {
        node: DevNode::Misc,
        name: b".",
        dtype: DT_DIR,
    },
    DevDirEntry {
        node: DevNode::Root,
        name: b"..",
        dtype: DT_DIR,
    },
    DevDirEntry {
        node: DevNode::Rtc,
        name: b"rtc",
        dtype: DT_CHR,
    },
];

fn linux_makedev(major: u64, minor: u64) -> u64 {
    (minor & 0xff) | ((major & 0xfff) << 8) | ((minor & !0xff) << 12) | ((major & !0xfff) << 32)
}

use super::align_up;

fn lookup_absolute(path: &str) -> Option<DevNode> {
    // UNFINISHED: This lightweight devfs is not a mountable filesystem yet.
    // Only absolute /dev paths and explicit /dev directory fds are handled.
    match path {
        "/dev" | "/dev/" => Some(DevNode::Root),
        "/dev/misc" | "/dev/misc/" => Some(DevNode::Misc),
        "/dev/pts" | "/dev/pts/" => Some(DevNode::Pts),
        "/dev/null" => Some(DevNode::Null),
        "/dev/zero" => Some(DevNode::Zero),
        "/dev/full" => Some(DevNode::Full),
        "/dev/random" => Some(DevNode::Random),
        "/dev/urandom" => Some(DevNode::Urandom),
        "/dev/tty" => Some(DevNode::Tty),
        "/dev/ttyS0" => Some(DevNode::TtyS0),
        "/dev/tty8" => Some(DevNode::Tty8),
        "/dev/tty9" => Some(DevNode::Tty9),
        "/dev/ptmx" => Some(DevNode::PtMx),
        "/dev/rtc" | "/dev/rtc0" | "/dev/misc/rtc" => Some(DevNode::Rtc),
        "/dev/loop-control" => Some(DevNode::LoopControl),
        "/dev/loop0" => Some(DevNode::Loop0),
        _ => None,
    }
}

fn parse_pts_id(path: &str) -> Option<usize> {
    if path.is_empty() || path.contains('/') {
        return None;
    }
    let id = path.parse::<usize>().ok()?;
    (id < PTY_TABLE_SIZE).then_some(id)
}

fn parse_absolute_pts_id(path: &str) -> Option<usize> {
    parse_pts_id(path.strip_prefix("/dev/pts/")?)
}

fn lookup_child(parent: DevNode, path: &str) -> Option<DevNode> {
    match parent {
        DevNode::Root => match path {
            "." | ".." => Some(DevNode::Root),
            "misc" => Some(DevNode::Misc),
            "pts" => Some(DevNode::Pts),
            "null" => Some(DevNode::Null),
            "zero" => Some(DevNode::Zero),
            "full" => Some(DevNode::Full),
            "random" => Some(DevNode::Random),
            "urandom" => Some(DevNode::Urandom),
            "tty" => Some(DevNode::Tty),
            "ttyS0" => Some(DevNode::TtyS0),
            "tty8" => Some(DevNode::Tty8),
            "tty9" => Some(DevNode::Tty9),
            "ptmx" => Some(DevNode::PtMx),
            "rtc" | "rtc0" => Some(DevNode::Rtc),
            "loop-control" => Some(DevNode::LoopControl),
            "loop0" => Some(DevNode::Loop0),
            _ => None,
        },
        DevNode::Misc => match path {
            "." => Some(DevNode::Misc),
            ".." => Some(DevNode::Root),
            "rtc" => Some(DevNode::Rtc),
            _ => None,
        },
        DevNode::Pts => match path {
            "." => Some(DevNode::Pts),
            ".." => Some(DevNode::Root),
            _ => None,
        },
        _ => None,
    }
}

fn wake_task(task: Option<Arc<TaskControlBlock>>) {
    if let Some(task) = task {
        let _ = wakeup_task(task);
    }
}

fn wake_tasks(tasks: VecDeque<Arc<TaskControlBlock>>) {
    for task in tasks {
        let _ = wakeup_task(task);
    }
}

fn pty_wait_interrupted() -> bool {
    // CONTEXT: File::read/write cannot return EINTR yet. Like pipes, a PTY
    // wait must return to the trap path when an unmasked signal is pending.
    current_has_unmasked_signal()
}

fn open_ptmx(flags: OpenFlags) -> FsResult<Arc<dyn File + Send + Sync>> {
    if flags.contains(OpenFlags::DIRECTORY) {
        return Err(FsError::NotDir);
    }
    let (readable, writable) = flags.read_write();
    let pair = PTY_TABLE.exclusive_session(|table| table.allocate())?;
    Ok(Arc::new(PtyFile::new(
        pair,
        PtyEndpoint::Master,
        readable,
        writable,
        OpenFlags::file_status_flags(flags),
    )))
}

fn open_pty_slave(id: usize, flags: OpenFlags) -> FsResult<Arc<dyn File + Send + Sync>> {
    if flags.contains(OpenFlags::CREATE | OpenFlags::EXCL) {
        return Err(FsError::AlreadyExists);
    }
    if flags.contains(OpenFlags::DIRECTORY) {
        return Err(FsError::NotDir);
    }
    let pair = PTY_TABLE
        .exclusive_session(|table| table.get(id))
        .ok_or(FsError::NotFound)?;
    {
        let mut inner = pair.exclusive_access();
        if inner.locked {
            return Err(FsError::PermissionDenied);
        }
        if inner.master_open == 0 {
            return Err(FsError::NoDeviceOrAddress);
        }
        inner.slave_open += 1;
    }
    let (readable, writable) = flags.read_write();
    Ok(Arc::new(PtyFile::new(
        pair,
        PtyEndpoint::Slave,
        readable,
        writable,
        OpenFlags::file_status_flags(flags),
    )))
}

fn open_node(node: DevNode, flags: OpenFlags) -> FsResult<Arc<dyn File + Send + Sync>> {
    if flags.contains(OpenFlags::CREATE | OpenFlags::EXCL) {
        return Err(FsError::AlreadyExists);
    }
    if node == DevNode::PtMx {
        return open_ptmx(flags);
    }
    if matches!(node, DevNode::Root | DevNode::Misc | DevNode::Pts) {
        if !flags.can_open_directory() {
            return Err(FsError::IsDir);
        }
        return Ok(Arc::new(DevFsFile::new(
            node,
            false,
            false,
            OpenFlags::file_status_flags(flags),
        )));
    }
    if flags.contains(OpenFlags::DIRECTORY) {
        return Err(FsError::NotDir);
    }
    let (readable, writable) = flags.read_write();
    Ok(Arc::new(DevFsFile::new(
        node,
        readable,
        writable,
        OpenFlags::file_status_flags(flags),
    )))
}

pub(crate) fn open(path: &str, flags: OpenFlags) -> FsResult<Option<Arc<dyn File + Send + Sync>>> {
    if let Some(id) = parse_absolute_pts_id(path) {
        return open_pty_slave(id, flags).map(Some);
    }
    lookup_absolute(path)
        .map(|node| open_node(node, flags))
        .transpose()
}

pub(crate) fn open_child(
    path: &str,
    flags: OpenFlags,
) -> FsResult<Option<Arc<dyn File + Send + Sync>>> {
    if let Some(rest) = path.strip_prefix("pts/") {
        return open_pts_child(rest, flags);
    }
    if path.contains('/') {
        return Ok(None);
    }
    lookup_child(DevNode::Root, path)
        .map(|node| open_node(node, flags))
        .transpose()
}

pub(crate) fn open_misc_child(
    path: &str,
    flags: OpenFlags,
) -> FsResult<Option<Arc<dyn File + Send + Sync>>> {
    if path.contains('/') {
        return Ok(None);
    }
    lookup_child(DevNode::Misc, path)
        .map(|node| open_node(node, flags))
        .transpose()
}

pub(crate) fn open_pts_child(
    path: &str,
    flags: OpenFlags,
) -> FsResult<Option<Arc<dyn File + Send + Sync>>> {
    if path.contains('/') {
        return Ok(None);
    }
    if let Some(node) = lookup_child(DevNode::Pts, path) {
        return open_node(node, flags).map(Some);
    }
    let Some(id) = parse_pts_id(path) else {
        return Ok(None);
    };
    open_pty_slave(id, flags).map(Some)
}

fn stat_node(node: DevNode) -> FileStat {
    let mode = match node {
        DevNode::Root | DevNode::Misc | DevNode::Pts => S_IFDIR | 0o755,
        DevNode::Loop0 => S_IFBLK | 0o666,
        DevNode::PtMx => S_IFCHR | 0o666,
        _ => S_IFCHR | 0o666,
    };
    let mut stat = FileStat::with_mode(mode);
    stat.dev = DEVFS_DEV;
    stat.ino = node.ino();
    stat.rdev = node.rdev();
    stat.nlink = if matches!(node, DevNode::Root | DevNode::Misc | DevNode::Pts) {
        2
    } else {
        1
    };
    stat.blocks = 0;
    stat
}

fn stat_pty_slave(id: usize) -> Option<FileStat> {
    PTY_TABLE.exclusive_session(|table| table.get(id))?;
    let mut stat = FileStat::with_mode(S_IFCHR | 0o620);
    stat.dev = DEVFS_DEV;
    stat.ino = PTY_INO_BASE + id as u64;
    stat.rdev = linux_makedev(136, id as u64);
    stat.nlink = 1;
    stat.blocks = 0;
    Some(stat)
}

fn loop0_backend() -> FsResult<Arc<dyn File + Send + Sync>> {
    LOOP0_BACKEND
        .exclusive_session(|backend| backend.clone())
        .ok_or(FsError::NoDeviceOrAddress)
}

fn loop0_size() -> u64 {
    loop0_backend()
        .ok()
        .and_then(|file| file.stat().ok())
        .map(|stat| stat.size)
        .unwrap_or(LOOP_DEVICE_SIZE_FALLBACK)
}

fn read_loop0_at(offset: usize, buf: &mut [u8]) -> usize {
    if loop0_backend().is_err() {
        return 0;
    }
    let size = loop0_size() as usize;
    if offset >= size {
        return 0;
    }
    let read_size = buf.len().min(size - offset);
    buf[..read_size].fill(0);
    read_size
}

fn write_loop0_at(offset: usize, buf: &[u8]) -> usize {
    if loop0_backend().is_err() {
        return 0;
    }
    let size = loop0_size() as usize;
    if offset >= size {
        return 0;
    }
    // CONTEXT: /dev/loop0 is currently a lightweight LTP scratch device.
    // mkfs output is not consumed by mount(), which routes loop sources to
    // tmpfs until the kernel has a real loop-backed block mount.
    buf.len().min(size - offset)
}

fn read_loop0(offset: &UPIntrFreeCell<usize>, mut user_buf: UserBuffer) -> usize {
    let mut current = offset.exclusive_access();
    let mut total = 0usize;
    for slice in user_buf.buffers.iter_mut() {
        let read_size = read_loop0_at(*current, slice);
        if read_size == 0 {
            break;
        }
        *current += read_size;
        total += read_size;
    }
    total
}

fn write_loop0(offset: &UPIntrFreeCell<usize>, user_buf: UserBuffer) -> usize {
    let mut current = offset.exclusive_access();
    let mut total = 0usize;
    for slice in user_buf.buffers.iter() {
        let write_size = write_loop0_at(*current, slice);
        if write_size == 0 {
            break;
        }
        *current += write_size;
        total += write_size;
        if write_size < slice.len() {
            break;
        }
    }
    total
}

fn seek_loop0(
    offset_cell: &UPIntrFreeCell<usize>,
    offset: i64,
    whence: SeekWhence,
) -> FsResult<usize> {
    let mut current = offset_cell.exclusive_access();
    let base = match whence {
        SeekWhence::Set => 0i128,
        SeekWhence::Current => *current as i128,
        SeekWhence::End => loop0_size() as i128,
    };
    let new_offset = base
        .checked_add(offset as i128)
        .ok_or(FsError::InvalidInput)?;
    if new_offset < 0 || new_offset > usize::MAX as i128 || new_offset > isize::MAX as i128 {
        return Err(FsError::InvalidInput);
    }
    *current = new_offset as usize;
    Ok(*current)
}

fn seek_null_like(offset_cell: &UPIntrFreeCell<usize>) -> usize {
    let mut current = offset_cell.exclusive_access();
    *current = 0;
    0
}

pub(crate) fn is_devfs_loop_control(file: &(dyn File + Send + Sync)) -> bool {
    file.as_any()
        .downcast_ref::<DevFsFile>()
        .map(|file| file.node == DevNode::LoopControl)
        .unwrap_or(false)
}

pub(crate) fn devfs_loop_device_id(file: &(dyn File + Send + Sync)) -> Option<usize> {
    file.as_any()
        .downcast_ref::<DevFsFile>()
        .and_then(|file| (file.node == DevNode::Loop0).then_some(0))
}

pub(crate) fn find_free_loop_device() -> FsResult<usize> {
    if LOOP0_BACKEND.exclusive_session(|backend| backend.is_none()) {
        Ok(0)
    } else {
        Err(FsError::NoDeviceOrAddress)
    }
}

pub(crate) fn loop_device_is_attached(id: usize) -> bool {
    id == 0 && LOOP0_BACKEND.exclusive_session(|backend| backend.is_some())
}

pub(crate) fn attach_loop_device(id: usize, backend: Arc<dyn File + Send + Sync>) -> FsResult {
    if id != 0 {
        return Err(FsError::NoDeviceOrAddress);
    }
    LOOP0_BACKEND.exclusive_session(|slot| *slot = Some(backend));
    Ok(())
}

pub(crate) fn detach_loop_device(id: usize) -> FsResult {
    if id != 0 {
        return Err(FsError::NoDeviceOrAddress);
    }
    LOOP0_BACKEND
        .exclusive_session(|slot| slot.take())
        .map(|_| ())
        .ok_or(FsError::NoDeviceOrAddress)
}

pub(crate) fn loop_device_size(id: usize) -> FsResult<u64> {
    if id != 0 {
        return Err(FsError::NoDeviceOrAddress);
    }
    Ok(loop0_size())
}

pub(crate) fn stat(path: &str) -> Option<FileStat> {
    if let Some(id) = parse_absolute_pts_id(path) {
        return stat_pty_slave(id);
    }
    Some(stat_node(lookup_absolute(path)?))
}

pub(crate) fn stat_child(path: &str) -> Option<FileStat> {
    if let Some(rest) = path.strip_prefix("pts/") {
        return stat_pts_child(rest);
    }
    if path.contains('/') {
        return None;
    }
    Some(stat_node(lookup_child(DevNode::Root, path)?))
}

pub(crate) fn stat_misc_child(path: &str) -> Option<FileStat> {
    if path.contains('/') {
        return None;
    }
    Some(stat_node(lookup_child(DevNode::Misc, path)?))
}

pub(crate) fn stat_pts_child(path: &str) -> Option<FileStat> {
    if path.contains('/') {
        return None;
    }
    if let Some(node) = lookup_child(DevNode::Pts, path) {
        return Some(stat_node(node));
    }
    stat_pty_slave(parse_pts_id(path)?)
}

fn read_console(user_buf: UserBuffer) -> usize {
    let want_to_read = user_buf.len();
    if want_to_read == 0 {
        return 0;
    }

    let mut buf_iter = user_buf.into_iter();
    let Some(byte_ref) = buf_iter.next() else {
        return 0;
    };
    unsafe {
        byte_ref.write_volatile(UART.read());
    }

    let mut already_read = 1usize;
    while already_read < want_to_read {
        let Some(ch) = UART.try_read() else {
            break;
        };
        let Some(byte_ref) = buf_iter.next() else {
            break;
        };
        unsafe {
            byte_ref.write_volatile(ch);
        }
        already_read += 1;
    }
    already_read
}

fn write_console(user_buf: UserBuffer) -> usize {
    let len = user_buf.len();
    for buffer in user_buf.buffers.iter() {
        for byte in buffer.iter() {
            UART.write(*byte);
        }
    }
    len
}

fn read_zero(user_buf: UserBuffer) -> usize {
    let len = user_buf.len();
    for byte_ref in user_buf {
        unsafe {
            byte_ref.write_volatile(0);
        }
    }
    len
}

fn read_random(user_buf: UserBuffer) -> usize {
    let len = user_buf.len();
    for (index, byte_ref) in user_buf.into_iter().enumerate() {
        unsafe {
            *byte_ref = (index as u8).wrapping_mul(37).wrapping_add(0xa5);
        }
    }
    len
}

fn read_pty(
    pair: &Arc<UPIntrFreeCell<PtyPair>>,
    endpoint: PtyEndpoint,
    mut user_buf: UserBuffer,
    flags: OpenFlags,
) -> usize {
    let want_to_read = user_buf.len();
    if want_to_read == 0 {
        return 0;
    }
    loop {
        let mut inner = pair.exclusive_access();
        let available = inner.input_buffer(endpoint).available_read();
        if available == 0 {
            if !inner.peer_open(endpoint) || flags.contains(OpenFlags::NONBLOCK) {
                return 0;
            }
            if pty_wait_interrupted() {
                return 0;
            }
            let task_cx_ptr = inner.input_buffer_mut(endpoint).sleep_reader();
            drop(inner);
            schedule(task_cx_ptr);
            continue;
        }

        let read_len = available.min(want_to_read);
        let mut copied = 0usize;
        for buffer in user_buf.buffers.iter_mut() {
            if copied == read_len {
                break;
            }
            let len = buffer.len().min(read_len - copied);
            for byte in &mut buffer[..len] {
                *byte = inner.input_buffer_mut(endpoint).read_byte();
            }
            copied += len;
        }
        let writer = inner.input_buffer_mut(endpoint).wake_writer();
        drop(inner);
        wake_task(writer);
        return copied;
    }
}

fn write_pty(
    pair: &Arc<UPIntrFreeCell<PtyPair>>,
    endpoint: PtyEndpoint,
    user_buf: UserBuffer,
    flags: OpenFlags,
) -> usize {
    let want_to_write = user_buf.len();
    if want_to_write == 0 {
        return 0;
    }
    let mut already_written = 0usize;
    loop {
        let mut inner = pair.exclusive_access();
        if !inner.peer_open(endpoint) {
            return already_written;
        }
        let available = inner.output_buffer(endpoint).available_write();
        if available == 0 {
            if flags.contains(OpenFlags::NONBLOCK) {
                return already_written;
            }
            if pty_wait_interrupted() {
                return already_written;
            }
            let task_cx_ptr = inner.output_buffer_mut(endpoint).sleep_writer();
            drop(inner);
            schedule(task_cx_ptr);
            continue;
        }

        let write_len = available.min(want_to_write - already_written);
        let mut skipped = 0usize;
        let mut written = 0usize;
        for buffer in user_buf.buffers.iter() {
            if skipped + buffer.len() <= already_written {
                skipped += buffer.len();
                continue;
            }
            let offset = already_written.saturating_sub(skipped);
            for &byte in &buffer[offset..] {
                if written == write_len {
                    break;
                }
                inner.output_buffer_mut(endpoint).write_byte(byte);
                written += 1;
            }
            if written == write_len {
                break;
            }
            skipped += buffer.len();
        }

        already_written += written;
        let reader = inner.output_buffer_mut(endpoint).wake_reader();
        drop(inner);
        wake_task(reader);
        if already_written == want_to_write {
            return already_written;
        }
    }
}

fn dir_entries(node: DevNode) -> Option<&'static [DevDirEntry]> {
    match node {
        DevNode::Root => Some(&ROOT_DEV_DIR_ENTRIES),
        DevNode::Misc => Some(&MISC_DEV_DIR_ENTRIES),
        _ => None,
    }
}

fn copy_dirents(
    entries: &'static [DevDirEntry],
    entries_offset: &mut usize,
    user_buf: UserBuffer,
) -> FsResult<isize> {
    let mut kernel_buf = vec![0u8; user_buf.len()];
    let mut written = 0usize;

    while *entries_offset < entries.len() {
        let entry = &entries[*entries_offset];
        let d_reclen = align_up(
            LINUX_DIRENT64_HEADER_SIZE + entry.name.len() + 1,
            LINUX_DIRENT64_ALIGN,
        );
        if d_reclen > kernel_buf.len().saturating_sub(written) {
            if written == 0 {
                return Err(FsError::InvalidInput);
            }
            break;
        }

        let next_offset = *entries_offset + 1;
        let entry_buf = &mut kernel_buf[written..written + d_reclen];
        entry_buf.fill(0);
        entry_buf[0..8].copy_from_slice(&entry.node.ino().to_ne_bytes());
        entry_buf[8..16].copy_from_slice(&(next_offset as i64).to_ne_bytes());
        entry_buf[16..18].copy_from_slice(&(d_reclen as u16).to_ne_bytes());
        entry_buf[18] = entry.dtype;
        entry_buf[LINUX_DIRENT64_HEADER_SIZE..LINUX_DIRENT64_HEADER_SIZE + entry.name.len()]
            .copy_from_slice(entry.name);

        written += d_reclen;
        *entries_offset = next_offset;
    }

    if written == 0 {
        return Ok(0);
    }
    for (idx, byte_ref) in user_buf.into_iter().take(written).enumerate() {
        unsafe {
            *byte_ref = kernel_buf[idx];
        }
    }
    Ok(written as isize)
}

struct DynamicDirEntry {
    ino: u64,
    name: alloc::vec::Vec<u8>,
    dtype: u8,
}

fn pts_dir_entries() -> alloc::vec::Vec<DynamicDirEntry> {
    let ids = PTY_TABLE.exclusive_session(|table| table.active_ids());
    let mut entries = alloc::vec::Vec::with_capacity(ids.len() + 2);
    entries.push(DynamicDirEntry {
        ino: DevNode::Pts.ino(),
        name: b".".to_vec(),
        dtype: DT_DIR,
    });
    entries.push(DynamicDirEntry {
        ino: DevNode::Root.ino(),
        name: b"..".to_vec(),
        dtype: DT_DIR,
    });
    for id in ids {
        entries.push(DynamicDirEntry {
            ino: PTY_INO_BASE + id as u64,
            name: id.to_string().into_bytes(),
            dtype: DT_CHR,
        });
    }
    entries
}

fn copy_dynamic_dirents(
    entries: &[DynamicDirEntry],
    entries_offset: &mut usize,
    user_buf: UserBuffer,
) -> FsResult<isize> {
    let mut kernel_buf = vec![0u8; user_buf.len()];
    let mut written = 0usize;

    while *entries_offset < entries.len() {
        let entry = &entries[*entries_offset];
        let d_reclen = align_up(
            LINUX_DIRENT64_HEADER_SIZE + entry.name.len() + 1,
            LINUX_DIRENT64_ALIGN,
        );
        if d_reclen > kernel_buf.len().saturating_sub(written) {
            if written == 0 {
                return Err(FsError::InvalidInput);
            }
            break;
        }

        let next_offset = *entries_offset + 1;
        let entry_buf = &mut kernel_buf[written..written + d_reclen];
        entry_buf.fill(0);
        entry_buf[0..8].copy_from_slice(&entry.ino.to_ne_bytes());
        entry_buf[8..16].copy_from_slice(&(next_offset as i64).to_ne_bytes());
        entry_buf[16..18].copy_from_slice(&(d_reclen as u16).to_ne_bytes());
        entry_buf[18] = entry.dtype;
        entry_buf[LINUX_DIRENT64_HEADER_SIZE..LINUX_DIRENT64_HEADER_SIZE + entry.name.len()]
            .copy_from_slice(entry.name.as_slice());

        written += d_reclen;
        *entries_offset = next_offset;
    }

    if written == 0 {
        return Ok(0);
    }
    for (idx, byte_ref) in user_buf.into_iter().take(written).enumerate() {
        unsafe {
            *byte_ref = kernel_buf[idx];
        }
    }
    Ok(written as isize)
}

impl File for DevFsFile {
    fn as_any(&self) -> &dyn core::any::Any {
        self
    }

    fn readable(&self) -> bool {
        self.readable
    }

    fn writable(&self) -> bool {
        self.writable
    }

    fn read(&self, user_buf: UserBuffer) -> usize {
        match self.node {
            DevNode::Root
            | DevNode::Misc
            | DevNode::Pts
            | DevNode::Null
            | DevNode::Rtc
            | DevNode::PtMx
            | DevNode::LoopControl => 0,
            DevNode::Zero | DevNode::Full => read_zero(user_buf),
            DevNode::Random | DevNode::Urandom => read_random(user_buf),
            DevNode::Tty | DevNode::TtyS0 | DevNode::Tty8 | DevNode::Tty9 => read_console(user_buf),
            DevNode::Loop0 => read_loop0(&self.offset, user_buf),
        }
    }

    fn write(&self, user_buf: UserBuffer) -> usize {
        match self.node {
            DevNode::Root
            | DevNode::Misc
            | DevNode::Pts
            | DevNode::Rtc
            | DevNode::Full
            | DevNode::PtMx
            | DevNode::LoopControl => 0,
            DevNode::Null | DevNode::Zero | DevNode::Random | DevNode::Urandom => user_buf.len(),
            DevNode::Tty | DevNode::TtyS0 | DevNode::Tty8 | DevNode::Tty9 => {
                write_console(user_buf)
            }
            DevNode::Loop0 => write_loop0(&self.offset, user_buf),
        }
    }

    fn read_at(&self, offset: usize, buf: &mut [u8]) -> usize {
        if self.node == DevNode::Loop0 {
            return read_loop0_at(offset, buf);
        }
        0
    }

    fn write_at(&self, offset: usize, buf: &[u8]) -> usize {
        if self.node == DevNode::Loop0 {
            return write_loop0_at(offset, buf);
        }
        0
    }

    fn seek(&self, offset: i64, whence: SeekWhence) -> FsResult<usize> {
        if self.node == DevNode::Loop0 {
            return seek_loop0(&self.offset, offset, whence);
        }
        if matches!(self.node, DevNode::Null | DevNode::Zero | DevNode::Full) {
            return Ok(seek_null_like(&self.offset));
        }
        Err(FsError::IllegalSeek)
    }

    fn poll(&self, events: PollEvents) -> PollEvents {
        match self.node {
            DevNode::Tty | DevNode::TtyS0 | DevNode::Tty8 | DevNode::Tty9 => {
                let mut ready = PollEvents::empty();
                if events.intersects(PollEvents::POLLIN | PollEvents::POLLPRI) && UART.has_input() {
                    ready |= PollEvents::POLLIN;
                }
                if events.contains(PollEvents::POLLOUT) && self.writable {
                    ready |= PollEvents::POLLOUT;
                }
                ready
            }
            _ => {
                let mut ready = PollEvents::empty();
                if events.intersects(PollEvents::POLLIN | PollEvents::POLLPRI) && self.readable {
                    ready |= PollEvents::POLLIN;
                }
                if events.contains(PollEvents::POLLOUT) && self.writable {
                    ready |= PollEvents::POLLOUT;
                }
                ready
            }
        }
    }

    fn stat(&self) -> FsResult<FileStat> {
        Ok(stat_node(self.node))
    }

    fn status_flags(&self) -> OpenFlags {
        self.status_flags.get()
    }

    fn set_status_flags(&self, flags: OpenFlags) {
        self.status_flags.set(flags);
    }

    fn read_dirent64(&self, user_buf: UserBuffer) -> FsResult<isize> {
        let mut offset = self.offset.exclusive_access();
        if self.node == DevNode::Pts {
            let entries = pts_dir_entries();
            return copy_dynamic_dirents(entries.as_slice(), &mut *offset, user_buf);
        }
        let entries = dir_entries(self.node).ok_or(FsError::NotDir)?;
        copy_dirents(entries, &mut *offset, user_buf)
    }

    fn is_tty(&self) -> bool {
        self.node.is_tty()
    }

    fn is_rtc(&self) -> bool {
        self.node == DevNode::Rtc
    }

    fn is_devfs_dir(&self) -> bool {
        matches!(self.node, DevNode::Root | DevNode::Misc | DevNode::Pts)
    }

    fn is_devfs_misc_dir(&self) -> bool {
        self.node == DevNode::Misc
    }

    fn is_devfs_pts_dir(&self) -> bool {
        self.node == DevNode::Pts
    }

    fn is_dev_full(&self) -> bool {
        self.node == DevNode::Full
    }
}

impl Drop for PtyFile {
    fn drop(&mut self) {
        let (readers, writers, remove) = {
            let mut pair = self.pair.exclusive_access();
            match self.endpoint {
                PtyEndpoint::Master => {
                    pair.master_open = pair.master_open.saturating_sub(1);
                    let readers = pair.master_to_slave.wake_all_readers();
                    let writers = pair.slave_to_master.wake_all_writers();
                    (readers, writers, pair.is_closed())
                }
                PtyEndpoint::Slave => {
                    pair.slave_open = pair.slave_open.saturating_sub(1);
                    let readers = pair.slave_to_master.wake_all_readers();
                    let writers = pair.master_to_slave.wake_all_writers();
                    (readers, writers, pair.is_closed())
                }
            }
        };
        wake_tasks(readers);
        wake_tasks(writers);
        if remove {
            let id = self.id() as usize;
            PTY_TABLE.exclusive_session(|table| table.remove_if_same(id, &self.pair));
        }
    }
}

impl File for PtyFile {
    fn as_any(&self) -> &dyn core::any::Any {
        self
    }

    fn readable(&self) -> bool {
        self.readable
    }

    fn writable(&self) -> bool {
        self.writable
    }

    fn read(&self, user_buf: UserBuffer) -> usize {
        read_pty(&self.pair, self.endpoint, user_buf, self.status_flags.get())
    }

    fn write(&self, user_buf: UserBuffer) -> usize {
        write_pty(&self.pair, self.endpoint, user_buf, self.status_flags.get())
    }

    fn poll(&self, events: PollEvents) -> PollEvents {
        let pair = self.pair.exclusive_access();
        let mut ready = PollEvents::empty();
        if self.readable {
            let has_data = pair.input_buffer(self.endpoint).available_read() > 0;
            let hangup = !pair.peer_open(self.endpoint);
            if events.intersects(PollEvents::POLLIN | PollEvents::POLLPRI) && (has_data || hangup) {
                ready |= PollEvents::POLLIN;
            }
            if hangup {
                ready |= PollEvents::POLLHUP;
            }
        }
        if self.writable {
            let can_write = pair.output_buffer(self.endpoint).available_write() > 0;
            let peer_closed = !pair.peer_open(self.endpoint);
            if events.contains(PollEvents::POLLOUT) && (can_write || peer_closed) {
                ready |= PollEvents::POLLOUT;
            }
            if peer_closed {
                ready |= PollEvents::POLLERR;
            }
        }
        ready
    }

    fn stat(&self) -> FsResult<FileStat> {
        Ok(match self.endpoint {
            PtyEndpoint::Master => stat_node(DevNode::PtMx),
            PtyEndpoint::Slave => stat_pty_slave(self.id() as usize).unwrap_or_else(|| {
                let mut stat = FileStat::with_mode(S_IFCHR | 0o620);
                stat.dev = DEVFS_DEV;
                stat.ino = PTY_INO_BASE + self.id() as u64;
                stat.rdev = linux_makedev(136, self.id() as u64);
                stat
            }),
        })
    }

    fn status_flags(&self) -> OpenFlags {
        self.status_flags.get()
    }

    fn set_status_flags(&self, flags: OpenFlags) {
        self.status_flags.set(flags);
    }

    fn pipe_occupied(&self) -> Option<usize> {
        Some(
            self.pair
                .exclusive_access()
                .input_buffer(self.endpoint)
                .available_read(),
        )
    }

    fn is_tty(&self) -> bool {
        true
    }
}

pub(crate) fn devfs_pty_number(file: &(dyn File + Send + Sync)) -> Option<u32> {
    file.as_any()
        .downcast_ref::<PtyFile>()
        .map(|file| file.id())
}

pub(crate) fn devfs_pty_lock_state(file: &(dyn File + Send + Sync)) -> Option<bool> {
    file.as_any()
        .downcast_ref::<PtyFile>()
        .map(|file| file.lock_state())
}

pub(crate) fn set_devfs_pty_locked(
    file: &(dyn File + Send + Sync),
    locked: bool,
) -> FsResult<bool> {
    let Some(file) = file.as_any().downcast_ref::<PtyFile>() else {
        return Ok(false);
    };
    file.set_locked(locked);
    Ok(true)
}
