use super::dirent::{
    DT_BLK, DT_CHR, DT_DIR, LINUX_DIRENT64_ALIGN, LINUX_DIRENT64_HEADER_SIZE, RawDirEntry,
    write_dir_entries,
};
use super::mount::MountId;
use super::path::WorkingDir;
use super::status_flags::StatusFlagsCell;
use super::vfs::{FileSystemBackend, FileSystemStat, FsNodeKind, VfsNodeId};
use super::{
    File, FileStat, FsError, FsResult, OpenFlags, PollEvents, PollWaitQueue, PollWaiter, S_IFBLK,
    S_IFCHR, S_IFDIR, SeekWhence, console_tty_poll, console_tty_poll_with_wait, console_tty_read,
};
use crate::drivers::chardev::{CharDevice, UART};
use crate::mm::UserBuffer;
use crate::sync::UPIntrFreeCell;
use crate::task::{
    TaskControlBlock, block_current_task_no_schedule, current_has_unmasked_signal, schedule,
    wakeup_task,
};
use alloc::collections::VecDeque;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::sync::Arc;
use alloc::{vec, vec::Vec};
use lazy_static::lazy_static;

const DEVFS_DEV: u64 = 0x646576;
const DEVFS_MAGIC: i64 = 0x0102_1994;
const LOOP_DEVICE_SIZE_FALLBACK: u64 = 300 * 1024 * 1024;
const LOOP_DEVICE_BLOCK_SIZE_DEFAULT: usize = 512;
const LOOP_DEVICE_DEFAULT_READ_AHEAD: usize = 128;
const LOOP_FLAG_READ_ONLY: u32 = 1;
const LOOP_FLAG_AUTOCLEAR: u32 = 4;
const LOOP_FLAG_PARTSCAN: u32 = 8;
const LOOP_FLAG_DIRECT_IO: u32 = 16;
const PTY_BUFFER_CAPACITY: usize = 8192;
const PTY_TABLE_SIZE: usize = 64;
const PTY_INO_BASE: u32 = 0x1000;
const INPUT_DEVICE_NAME: &str = "virtual-device-ltp";
const INPUT_EVENT_QUEUE_CAPACITY: usize = 512;
const INPUT_MICE_QUEUE_CAPACITY: usize = 512;
const INPUT_EVENT_SIZE: usize = core::mem::size_of::<LinuxInputEvent>();
const UINPUT_MAX_NAME_SIZE: usize = 80;
const EV_SYN: u16 = 0x00;
const EV_KEY: u16 = 0x01;
const EV_REL: u16 = 0x02;
const EV_REP: u16 = 0x14;
const EV_MAX: usize = 0x1f;
const SYN_REPORT: u16 = 0;
const REL_MAX: usize = 0x0f;
const KEY_X: u16 = 45;
const BTN_RIGHT: u16 = 0x111;
const KEY_MAX: usize = 0x2ff;
const REP_DELAY: u16 = 0;
const REP_PERIOD: u16 = 1;
const INPUT_DEFAULT_REP_DELAY_MS: i32 = 250;
const INPUT_DEFAULT_REP_PERIOD_MS: i32 = 33;

lazy_static! {
    static ref LOOP0_STATE: UPIntrFreeCell<LoopDeviceState> =
        unsafe { UPIntrFreeCell::new(LoopDeviceState::new()) };
    static ref PTY_TABLE: UPIntrFreeCell<PtyTable> =
        unsafe { UPIntrFreeCell::new(PtyTable::new()) };
    static ref INPUT_STATE: UPIntrFreeCell<InputState> =
        unsafe { UPIntrFreeCell::new(InputState::new()) };
}

struct LoopDeviceState {
    backend: Option<Arc<dyn File + Send + Sync>>,
    backing_path: Option<String>,
    flags: u32,
    read_ahead: usize,
    block_size: usize,
    size: u64,
    size_limit: u64,
}

impl LoopDeviceState {
    fn new() -> Self {
        Self {
            backend: None,
            backing_path: None,
            flags: 0,
            read_ahead: LOOP_DEVICE_DEFAULT_READ_AHEAD,
            block_size: LOOP_DEVICE_BLOCK_SIZE_DEFAULT,
            size: LOOP_DEVICE_SIZE_FALLBACK,
            size_limit: 0,
        }
    }

    fn reset(&mut self) {
        *self = Self::new();
    }

    fn read_only(&self) -> bool {
        self.flags & LOOP_FLAG_READ_ONLY != 0
    }

    fn visible_size(&self) -> u64 {
        if self.size_limit == 0 {
            self.size
        } else {
            self.size.min(self.size_limit)
        }
    }

    fn set_flag(&mut self, flag: u32, enabled: bool) {
        if enabled {
            self.flags |= flag;
        } else {
            self.flags &= !flag;
        }
    }
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
    Input,
    UInput,
    InputEvent0,
    InputMice,
    Net,
    Tun,
}

impl DevNode {
    fn ino(self) -> u32 {
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
            Self::Input => 17,
            Self::UInput => 18,
            Self::InputEvent0 => 19,
            Self::InputMice => 20,
            Self::Net => 21,
            Self::Tun => 22,
        }
    }

    fn rdev(self) -> u64 {
        match self {
            Self::Root | Self::Misc | Self::Pts | Self::Input | Self::Net => 0,
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
            Self::UInput => linux_makedev(10, 223),
            Self::InputEvent0 => linux_makedev(13, 64),
            Self::InputMice => linux_makedev(13, 63),
            Self::Tun => linux_makedev(10, 200),
        }
    }

    fn is_tty(self) -> bool {
        matches!(self, Self::Tty | Self::TtyS0 | Self::Tty8 | Self::Tty9)
    }

    fn kind(self) -> FsNodeKind {
        match self {
            Self::Root | Self::Misc | Self::Pts | Self::Input | Self::Net => FsNodeKind::Directory,
            _ => FsNodeKind::CharacterDevice,
        }
    }
}

struct DevFsFile {
    node: DevNode,
    mount_id: Option<MountId>,
    readable: bool,
    writable: bool,
    offset: UPIntrFreeCell<usize>,
    status_flags: StatusFlagsCell,
}

impl DevFsFile {
    fn new(
        node: DevNode,
        mount_id: Option<MountId>,
        readable: bool,
        writable: bool,
        status_flags: OpenFlags,
    ) -> Self {
        Self {
            node,
            mount_id,
            readable,
            writable,
            offset: unsafe { UPIntrFreeCell::new(0) },
            status_flags: StatusFlagsCell::new(status_flags),
        }
    }
}

pub(super) struct DevFs;

impl DevFs {
    pub(super) fn new() -> Self {
        Self
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

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
struct LinuxInputEvent {
    tv_sec: usize,
    tv_usec: usize,
    event_type: u16,
    code: u16,
    value: i32,
}

#[derive(Clone)]
struct InputDeviceConfig {
    name: [u8; UINPUT_MAX_NAME_SIZE],
    ev_bits: [bool; EV_MAX + 1],
    key_bits: [bool; KEY_MAX + 1],
    rel_bits: [bool; REL_MAX + 1],
}

impl InputDeviceConfig {
    fn new() -> Self {
        let mut name = [0u8; UINPUT_MAX_NAME_SIZE];
        name[..INPUT_DEVICE_NAME.len()].copy_from_slice(INPUT_DEVICE_NAME.as_bytes());
        Self {
            name,
            ev_bits: [false; EV_MAX + 1],
            key_bits: [false; KEY_MAX + 1],
            rel_bits: [false; REL_MAX + 1],
        }
    }

    fn name_bytes(&self) -> &[u8] {
        let len = self
            .name
            .iter()
            .position(|byte| *byte == 0)
            .unwrap_or(self.name.len());
        &self.name[..len]
    }

    fn set_name_from_user_dev(&mut self, data: &[u8]) {
        self.name.fill(0);
        let len = data
            .iter()
            .take(UINPUT_MAX_NAME_SIZE)
            .position(|byte| *byte == 0)
            .unwrap_or(UINPUT_MAX_NAME_SIZE);
        self.name[..len].copy_from_slice(&data[..len]);
    }

    fn set_evbit(&mut self, code: usize) -> FsResult {
        set_bool_bit(&mut self.ev_bits, code)
    }

    fn set_keybit(&mut self, code: usize) -> FsResult {
        set_bool_bit(&mut self.key_bits, code)
    }

    fn set_relbit(&mut self, code: usize) -> FsResult {
        set_bool_bit(&mut self.rel_bits, code)
    }

    fn supports_ev(&self, event_type: u16) -> bool {
        self.ev_bits
            .get(event_type as usize)
            .copied()
            .unwrap_or(false)
    }

    fn supports_key(&self, code: u16) -> bool {
        self.supports_ev(EV_KEY) && self.key_bits.get(code as usize).copied().unwrap_or(false)
    }

    fn supports_rel(&self, code: u16) -> bool {
        self.supports_ev(EV_REL) && self.rel_bits.get(code as usize).copied().unwrap_or(false)
    }
}

fn set_bool_bit(bits: &mut [bool], code: usize) -> FsResult {
    let bit = bits.get_mut(code).ok_or(FsError::InvalidInput)?;
    *bit = true;
    Ok(())
}

struct UInputConfig {
    device: InputDeviceConfig,
    packet_has_event: bool,
}

impl UInputConfig {
    fn new() -> Self {
        Self {
            device: InputDeviceConfig::new(),
            packet_has_event: false,
        }
    }
}

struct UInputFile {
    config: UPIntrFreeCell<UInputConfig>,
    status_flags: StatusFlagsCell,
}

impl UInputFile {
    fn new(flags: OpenFlags) -> Self {
        Self {
            config: unsafe { UPIntrFreeCell::new(UInputConfig::new()) },
            status_flags: StatusFlagsCell::new(OpenFlags::file_status_flags(flags)),
        }
    }
}

struct InputEventFile {
    client: Arc<UPIntrFreeCell<InputClient>>,
    readable: bool,
    writable: bool,
    status_flags: StatusFlagsCell,
}

impl InputEventFile {
    fn new(readable: bool, writable: bool, flags: OpenFlags) -> Self {
        Self {
            client: input_open_client(),
            readable,
            writable,
            status_flags: StatusFlagsCell::new(OpenFlags::file_status_flags(flags)),
        }
    }
}

struct InputClient {
    events: VecDeque<LinuxInputEvent>,
    read_wait_queue: VecDeque<Arc<TaskControlBlock>>,
    read_poll_wait_queue: PollWaitQueue,
    grabbed: bool,
    closed: bool,
}

impl InputClient {
    fn new() -> Self {
        Self {
            events: VecDeque::new(),
            read_wait_queue: VecDeque::new(),
            read_poll_wait_queue: PollWaitQueue::new(),
            grabbed: false,
            closed: false,
        }
    }

    fn available_read(&self) -> usize {
        self.events.len() * INPUT_EVENT_SIZE
    }

    fn push_event(
        &mut self,
        event: LinuxInputEvent,
    ) -> (VecDeque<Arc<TaskControlBlock>>, Vec<Arc<PollWaiter>>) {
        if self.events.len() >= INPUT_EVENT_QUEUE_CAPACITY {
            self.events.pop_front();
        }
        self.events.push_back(event);
        (
            core::mem::take(&mut self.read_wait_queue),
            self.read_poll_wait_queue.drain(),
        )
    }

    fn sleep_reader(&mut self) -> *mut crate::task::TaskContext {
        let (task, task_cx_ptr) = block_current_task_no_schedule();
        self.read_wait_queue.push_back(task);
        task_cx_ptr
    }
}

struct InputState {
    device_created: bool,
    device: InputDeviceConfig,
    clients: Vec<Arc<UPIntrFreeCell<InputClient>>>,
    mice_queue: VecDeque<u8>,
    mice_read_wait_queue: VecDeque<Arc<TaskControlBlock>>,
}

impl InputState {
    fn new() -> Self {
        Self {
            device_created: false,
            device: InputDeviceConfig::new(),
            clients: Vec::new(),
            mice_queue: VecDeque::new(),
            mice_read_wait_queue: VecDeque::new(),
        }
    }

    fn create_device(&mut self, device: InputDeviceConfig) {
        self.device_created = true;
        self.device = device;
        self.mice_queue.clear();
    }

    fn destroy_device(&mut self) {
        self.device_created = false;
        self.mice_queue.clear();
        for client in &self.clients {
            client.exclusive_access().events.clear();
        }
    }

    fn sleep_mice_reader(&mut self) -> *mut crate::task::TaskContext {
        let (task, task_cx_ptr) = block_current_task_no_schedule();
        self.mice_read_wait_queue.push_back(task);
        task_cx_ptr
    }
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
    read_poll_wait_queue: PollWaitQueue,
    write_poll_wait_queue: PollWaitQueue,
}

impl PtyBuffer {
    fn new() -> Self {
        Self {
            data: VecDeque::new(),
            read_wait_queue: VecDeque::new(),
            write_wait_queue: VecDeque::new(),
            read_poll_wait_queue: PollWaitQueue::new(),
            write_poll_wait_queue: PollWaitQueue::new(),
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

    fn register_read_poll_waiter(&mut self, waiter: &Arc<PollWaiter>) {
        self.read_poll_wait_queue.register(waiter);
    }

    fn register_write_poll_waiter(&mut self, waiter: &Arc<PollWaiter>) {
        self.write_poll_wait_queue.register(waiter);
    }

    fn wake_read_poll_waiters(&mut self) -> Vec<Arc<PollWaiter>> {
        self.read_poll_wait_queue.drain()
    }

    fn wake_write_poll_waiters(&mut self) -> Vec<Arc<PollWaiter>> {
        self.write_poll_wait_queue.drain()
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

const ROOT_DEV_DIR_ENTRIES: [DevDirEntry; 21] = [
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
        node: DevNode::Input,
        name: b"input",
        dtype: DT_DIR,
    },
    DevDirEntry {
        node: DevNode::Net,
        name: b"net",
        dtype: DT_DIR,
    },
    DevDirEntry {
        node: DevNode::UInput,
        name: b"uinput",
        dtype: DT_CHR,
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

const NET_DEV_DIR_ENTRIES: [DevDirEntry; 3] = [
    DevDirEntry {
        node: DevNode::Net,
        name: b".",
        dtype: DT_DIR,
    },
    DevDirEntry {
        node: DevNode::Root,
        name: b"..",
        dtype: DT_DIR,
    },
    DevDirEntry {
        node: DevNode::Tun,
        name: b"tun",
        dtype: DT_CHR,
    },
];

const INPUT_DEV_DIR_ENTRIES: [DevDirEntry; 5] = [
    DevDirEntry {
        node: DevNode::Input,
        name: b".",
        dtype: DT_DIR,
    },
    DevDirEntry {
        node: DevNode::Root,
        name: b"..",
        dtype: DT_DIR,
    },
    DevDirEntry {
        node: DevNode::UInput,
        name: b"uinput",
        dtype: DT_CHR,
    },
    DevDirEntry {
        node: DevNode::InputEvent0,
        name: b"event0",
        dtype: DT_CHR,
    },
    DevDirEntry {
        node: DevNode::InputMice,
        name: b"mice",
        dtype: DT_CHR,
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

fn node_from_ino(ino: u32) -> Option<DevNode> {
    match ino {
        1 => Some(DevNode::Root),
        2 => Some(DevNode::Misc),
        3 => Some(DevNode::Pts),
        4 => Some(DevNode::Null),
        5 => Some(DevNode::Zero),
        6 => Some(DevNode::Full),
        7 => Some(DevNode::Random),
        8 => Some(DevNode::Urandom),
        9 => Some(DevNode::Tty),
        10 => Some(DevNode::TtyS0),
        11 => Some(DevNode::Tty8),
        12 => Some(DevNode::Tty9),
        13 => Some(DevNode::PtMx),
        14 => Some(DevNode::Rtc),
        15 => Some(DevNode::LoopControl),
        16 => Some(DevNode::Loop0),
        17 => Some(DevNode::Input),
        18 => Some(DevNode::UInput),
        19 => Some(DevNode::InputEvent0),
        20 => Some(DevNode::InputMice),
        21 => Some(DevNode::Net),
        22 => Some(DevNode::Tun),
        _ => None,
    }
}

fn pty_id_from_ino(ino: u32) -> Option<usize> {
    let id = ino.checked_sub(PTY_INO_BASE)? as usize;
    (id < PTY_TABLE_SIZE).then_some(id)
}

fn parse_pts_id(path: &str) -> Option<usize> {
    if path.is_empty() || path.contains('/') {
        return None;
    }
    let id = path.parse::<usize>().ok()?;
    (id < PTY_TABLE_SIZE).then_some(id)
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
            "input" => Some(DevNode::Input),
            "net" => Some(DevNode::Net),
            "uinput" => Some(DevNode::UInput),
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
        DevNode::Input => match path {
            "." => Some(DevNode::Input),
            ".." => Some(DevNode::Root),
            "uinput" => Some(DevNode::UInput),
            "event0" => Some(DevNode::InputEvent0),
            "mice" => Some(DevNode::InputMice),
            _ => None,
        },
        DevNode::Net => match path {
            "." => Some(DevNode::Net),
            ".." => Some(DevNode::Root),
            "tun" => Some(DevNode::Tun),
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

fn open_node_at(
    node: DevNode,
    mount_id: Option<MountId>,
    flags: OpenFlags,
) -> FsResult<Arc<dyn File + Send + Sync>> {
    if flags.contains(OpenFlags::CREATE | OpenFlags::EXCL) {
        return Err(FsError::AlreadyExists);
    }
    if node == DevNode::PtMx {
        return open_ptmx(flags);
    }
    if node == DevNode::UInput {
        if flags.contains(OpenFlags::DIRECTORY) {
            return Err(FsError::NotDir);
        }
        return Ok(Arc::new(UInputFile::new(flags)));
    }
    if node == DevNode::InputEvent0 {
        if flags.contains(OpenFlags::DIRECTORY) {
            return Err(FsError::NotDir);
        }
        let (readable, writable) = flags.read_write();
        return Ok(Arc::new(InputEventFile::new(readable, writable, flags)));
    }
    if matches!(
        node,
        DevNode::Root | DevNode::Misc | DevNode::Pts | DevNode::Input | DevNode::Net
    ) {
        if !flags.can_open_directory() {
            return Err(FsError::IsDir);
        }
        return Ok(Arc::new(DevFsFile::new(
            node,
            mount_id,
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
        mount_id,
        readable,
        writable,
        OpenFlags::file_status_flags(flags),
    )))
}

fn open_node(node: DevNode, flags: OpenFlags) -> FsResult<Arc<dyn File + Send + Sync>> {
    open_node_at(node, None, flags)
}

pub(crate) fn open_child(
    path: &str,
    flags: OpenFlags,
) -> FsResult<Option<Arc<dyn File + Send + Sync>>> {
    if let Some(rest) = path.strip_prefix("pts/") {
        return open_pts_child(rest, flags);
    }
    if let Some(rest) = path.strip_prefix("input/") {
        return open_input_child(rest, flags);
    }
    if let Some(rest) = path.strip_prefix("net/") {
        return open_net_child(rest, flags);
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

pub(crate) fn open_input_child(
    path: &str,
    flags: OpenFlags,
) -> FsResult<Option<Arc<dyn File + Send + Sync>>> {
    if path.contains('/') {
        return Ok(None);
    }
    lookup_child(DevNode::Input, path)
        .map(|node| open_node(node, flags))
        .transpose()
}

pub(crate) fn open_net_child(
    path: &str,
    flags: OpenFlags,
) -> FsResult<Option<Arc<dyn File + Send + Sync>>> {
    if path.contains('/') {
        return Ok(None);
    }
    lookup_child(DevNode::Net, path)
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

pub(crate) fn open_inode(
    mount_id: MountId,
    ino: u32,
    flags: OpenFlags,
) -> FsResult<Arc<dyn File + Send + Sync>> {
    if let Some(id) = pty_id_from_ino(ino) {
        return open_pty_slave(id, flags);
    }
    let node = node_from_ino(ino).ok_or(FsError::NotFound)?;
    open_node_at(node, Some(mount_id), flags)
}

pub(crate) fn inode_is_misc_dir(ino: u32) -> bool {
    ino == DevNode::Misc.ino()
}

pub(crate) fn inode_is_pts_dir(ino: u32) -> bool {
    ino == DevNode::Pts.ino()
}

fn stat_node(node: DevNode) -> FileStat {
    let mode = match node {
        DevNode::Root | DevNode::Misc | DevNode::Pts | DevNode::Input | DevNode::Net => {
            S_IFDIR | 0o755
        }
        DevNode::Loop0 => S_IFBLK | 0o666,
        DevNode::PtMx => S_IFCHR | 0o666,
        _ => S_IFCHR | 0o666,
    };
    let mut stat = FileStat::with_mode(mode);
    stat.dev = DEVFS_DEV;
    stat.ino = node.ino() as u64;
    stat.rdev = node.rdev();
    stat.nlink = if matches!(
        node,
        DevNode::Root | DevNode::Misc | DevNode::Pts | DevNode::Input | DevNode::Net
    ) {
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
    stat.ino = PTY_INO_BASE as u64 + id as u64;
    stat.rdev = linux_makedev(136, id as u64);
    stat.nlink = 1;
    stat.blocks = 0;
    Some(stat)
}

fn loop0_backend() -> FsResult<Arc<dyn File + Send + Sync>> {
    LOOP0_STATE
        .exclusive_session(|state| state.backend.clone())
        .ok_or(FsError::NoDeviceOrAddress)
}

fn loop0_size() -> u64 {
    LOOP0_STATE.exclusive_session(|state| state.visible_size())
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
    if loop_device_is_read_only(0) {
        return 0;
    }
    // CONTEXT: /dev/loop0 is currently a lightweight LTP scratch device.
    // mkfs output is not consumed by mount(), which routes loop sources to
    // tmpfs until the kernel has a real loop-backed block mount.
    if offset == 0 && !buf.is_empty() {
        super::mount::reset_ext_scratch_mount("/dev/loop0");
    }
    let size = loop0_size() as usize;
    if offset < size {
        buf.len().min(size - offset)
    } else {
        // CONTEXT: BusyBox mkfs.ext2 uses full_write(), which retries forever if a
        // block-device write returns 0. Writes that start before the visible loop
        // capacity still report a Linux-like short count for LOOP_SET_CAPACITY
        // tests; only EOF-only scratch writes are accepted to keep mkfs setup moving.
        buf.len()
    }
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
        SeekWhence::Data | SeekWhence::Hole => return Err(FsError::InvalidInput),
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

pub(crate) fn is_devfs_uinput(file: &(dyn File + Send + Sync)) -> bool {
    file.as_any().downcast_ref::<UInputFile>().is_some()
}

pub(crate) fn is_devfs_input_event(file: &(dyn File + Send + Sync)) -> bool {
    file.as_any().downcast_ref::<InputEventFile>().is_some()
}

pub(crate) fn is_devfs_tun(file: &(dyn File + Send + Sync)) -> bool {
    file.as_any()
        .downcast_ref::<DevFsFile>()
        .map(|file| file.node == DevNode::Tun)
        .unwrap_or(false)
}

pub(crate) fn devfs_uinput_set_evbit(file: &(dyn File + Send + Sync), code: usize) -> FsResult {
    let file = file
        .as_any()
        .downcast_ref::<UInputFile>()
        .ok_or(FsError::InvalidInput)?;
    file.config.exclusive_access().device.set_evbit(code)
}

pub(crate) fn devfs_uinput_set_keybit(file: &(dyn File + Send + Sync), code: usize) -> FsResult {
    let file = file
        .as_any()
        .downcast_ref::<UInputFile>()
        .ok_or(FsError::InvalidInput)?;
    file.config.exclusive_access().device.set_keybit(code)
}

pub(crate) fn devfs_uinput_set_relbit(file: &(dyn File + Send + Sync), code: usize) -> FsResult {
    let file = file
        .as_any()
        .downcast_ref::<UInputFile>()
        .ok_or(FsError::InvalidInput)?;
    file.config.exclusive_access().device.set_relbit(code)
}

pub(crate) fn devfs_uinput_create(file: &(dyn File + Send + Sync)) -> FsResult {
    let file = file
        .as_any()
        .downcast_ref::<UInputFile>()
        .ok_or(FsError::InvalidInput)?;
    let device = file.config.exclusive_access().device.clone();
    INPUT_STATE.exclusive_session(|state| state.create_device(device));
    Ok(())
}

pub(crate) fn devfs_uinput_destroy(file: &(dyn File + Send + Sync)) -> FsResult {
    file.as_any()
        .downcast_ref::<UInputFile>()
        .ok_or(FsError::InvalidInput)?;
    INPUT_STATE.exclusive_session(|state| state.destroy_device());
    Ok(())
}

pub(crate) fn devfs_input_event_name(file: &(dyn File + Send + Sync)) -> Option<Vec<u8>> {
    file.as_any().downcast_ref::<InputEventFile>()?;
    Some(INPUT_STATE.exclusive_access().device.name_bytes().to_vec())
}

pub(crate) fn devfs_input_event_set_grabbed(
    file: &(dyn File + Send + Sync),
    grabbed: bool,
) -> FsResult {
    let file = file
        .as_any()
        .downcast_ref::<InputEventFile>()
        .ok_or(FsError::InvalidInput)?;
    file.client.exclusive_access().grabbed = grabbed;
    Ok(())
}

pub(crate) fn find_free_loop_device() -> FsResult<usize> {
    if LOOP0_STATE.exclusive_session(|state| state.backend.is_none()) {
        Ok(0)
    } else {
        Err(FsError::NoDeviceOrAddress)
    }
}

pub(crate) fn loop_device_is_attached(id: usize) -> bool {
    id == 0 && LOOP0_STATE.exclusive_session(|state| state.backend.is_some())
}

pub(crate) fn loop_device_is_read_only(id: usize) -> bool {
    id == 0 && LOOP0_STATE.exclusive_session(|state| state.read_only())
}

pub(crate) fn loop_device_set_read_only(id: usize, read_only: bool) -> FsResult {
    if id != 0 || !loop_device_is_attached(id) {
        return Err(FsError::NoDeviceOrAddress);
    }
    LOOP0_STATE.exclusive_session(|state| state.set_flag(LOOP_FLAG_READ_ONLY, read_only));
    Ok(())
}

pub(crate) fn loop_device_read_ahead(id: usize) -> FsResult<usize> {
    if id != 0 || !loop_device_is_attached(id) {
        return Err(FsError::NoDeviceOrAddress);
    }
    Ok(LOOP0_STATE.exclusive_session(|state| state.read_ahead))
}

pub(crate) fn loop_device_set_read_ahead(id: usize, read_ahead: usize) -> FsResult {
    if id != 0 || !loop_device_is_attached(id) {
        return Err(FsError::NoDeviceOrAddress);
    }
    LOOP0_STATE.exclusive_session(|state| state.read_ahead = read_ahead);
    Ok(())
}

pub(crate) fn attach_loop_device(
    id: usize,
    backend: Arc<dyn File + Send + Sync>,
    read_only: bool,
    backing_path: Option<String>,
) -> FsResult {
    if id != 0 {
        return Err(FsError::NoDeviceOrAddress);
    }
    let size = backend.stat().map(|stat| stat.size)?;
    super::mount::reset_ext_scratch_mount("/dev/loop0");
    LOOP0_STATE.exclusive_session(|state| {
        state.reset();
        state.backend = Some(backend);
        state.backing_path = backing_path;
        state.size = size;
        state.set_flag(LOOP_FLAG_READ_ONLY, read_only);
    });
    Ok(())
}

pub(crate) fn detach_loop_device(id: usize) -> FsResult {
    if id != 0 {
        return Err(FsError::NoDeviceOrAddress);
    }
    LOOP0_STATE
        .exclusive_session(|state| {
            let backend = state.backend.take();
            if backend.is_some() {
                state.reset();
            }
            backend
        })
        .map(|_| ())
        .ok_or(FsError::NoDeviceOrAddress)
}

pub(crate) fn loop_device_size(id: usize) -> FsResult<u64> {
    if id != 0 || !loop_device_is_attached(id) {
        return Err(FsError::NoDeviceOrAddress);
    }
    Ok(loop0_size())
}

pub(crate) fn loop_device_refresh_size(id: usize) -> FsResult<u64> {
    if id != 0 {
        return Err(FsError::NoDeviceOrAddress);
    }
    let backend = loop0_backend()?;
    let size = backend.stat().map(|stat| stat.size)?;
    Ok(LOOP0_STATE.exclusive_session(|state| {
        state.size = size;
        state.visible_size()
    }))
}

pub(crate) fn loop_device_flags(id: usize) -> FsResult<u32> {
    if id != 0 || !loop_device_is_attached(id) {
        return Err(FsError::NoDeviceOrAddress);
    }
    Ok(LOOP0_STATE.exclusive_session(|state| state.flags))
}

pub(crate) fn loop_device_size_limit(id: usize) -> FsResult<u64> {
    if id != 0 || !loop_device_is_attached(id) {
        return Err(FsError::NoDeviceOrAddress);
    }
    Ok(LOOP0_STATE.exclusive_session(|state| state.size_limit))
}

pub(crate) fn loop_device_set_status(id: usize, flags: u32, size_limit: Option<u64>) -> FsResult {
    if id != 0 || !loop_device_is_attached(id) {
        return Err(FsError::NoDeviceOrAddress);
    }
    LOOP0_STATE.exclusive_session(|state| {
        state.set_flag(LOOP_FLAG_AUTOCLEAR, flags & LOOP_FLAG_AUTOCLEAR != 0);
        if flags & LOOP_FLAG_PARTSCAN != 0 {
            state.set_flag(LOOP_FLAG_PARTSCAN, true);
        }
        if let Some(size_limit) = size_limit {
            state.size_limit = size_limit;
        }
    });
    Ok(())
}

pub(crate) fn loop_device_set_direct_io(id: usize, enabled: bool) -> FsResult {
    if id != 0 || !loop_device_is_attached(id) {
        return Err(FsError::NoDeviceOrAddress);
    }
    LOOP0_STATE.exclusive_session(|state| state.set_flag(LOOP_FLAG_DIRECT_IO, enabled));
    Ok(())
}

pub(crate) fn loop_device_set_block_size(id: usize, block_size: usize) -> FsResult {
    if id != 0 {
        return Err(FsError::NoDeviceOrAddress);
    }
    LOOP0_STATE.exclusive_session(|state| state.block_size = block_size);
    Ok(())
}

pub(crate) fn loop_device_change_fd(
    id: usize,
    backend: Arc<dyn File + Send + Sync>,
    backing_path: Option<String>,
) -> FsResult {
    if id != 0 || !loop_device_is_attached(id) || !loop_device_is_read_only(id) {
        return Err(FsError::InvalidInput);
    }
    let old_size = LOOP0_STATE.exclusive_session(|state| state.size);
    let new_size = backend.stat().map(|stat| stat.size)?;
    if new_size != old_size {
        return Err(FsError::InvalidInput);
    }
    LOOP0_STATE.exclusive_session(|state| {
        state.backend = Some(backend);
        state.backing_path = backing_path;
        state.size = new_size;
    });
    Ok(())
}

pub(crate) fn loop_device_sysfs_content(path: &str) -> Option<Vec<u8>> {
    let content = match path {
        "/sys/block/loop0/size" => format!("{}\n", loop0_size() / 512),
        "/sys/block/loop0/ro" => {
            let read_only = if loop_device_is_read_only(0) { 1 } else { 0 };
            format!("{read_only}\n")
        }
        "/sys/block/loop0/loop/partscan" => {
            let value = LOOP0_STATE
                .exclusive_session(|state| (state.flags & LOOP_FLAG_PARTSCAN != 0) as u8);
            format!("{value}\n")
        }
        "/sys/block/loop0/loop/autoclear" => {
            let value = LOOP0_STATE
                .exclusive_session(|state| (state.flags & LOOP_FLAG_AUTOCLEAR != 0) as u8);
            format!("{value}\n")
        }
        "/sys/block/loop0/loop/backing_file" => {
            LOOP0_STATE
                .exclusive_session(|state| state.backing_path.clone())
                .unwrap_or_default()
                + "\n"
        }
        "/sys/block/loop0/loop/dio" => {
            let value = LOOP0_STATE
                .exclusive_session(|state| (state.flags & LOOP_FLAG_DIRECT_IO != 0) as u8);
            format!("{value}\n")
        }
        "/sys/block/loop0/loop/sizelimit" => {
            let size_limit = LOOP0_STATE.exclusive_session(|state| state.size_limit);
            format!("{size_limit}\n")
        }
        "/sys/block/loop0/queue/logical_block_size" => "4096\n".into(),
        "/sys/block/loop0/queue/dma_alignment" => "4095\n".into(),
        _ => return None,
    };
    Some(content.into_bytes())
}

pub(crate) fn stat_child(path: &str) -> Option<FileStat> {
    if let Some(rest) = path.strip_prefix("pts/") {
        return stat_pts_child(rest);
    }
    if let Some(rest) = path.strip_prefix("input/") {
        return stat_input_child(rest);
    }
    if let Some(rest) = path.strip_prefix("net/") {
        return stat_net_child(rest);
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

pub(crate) fn stat_input_child(path: &str) -> Option<FileStat> {
    if path.contains('/') {
        return None;
    }
    Some(stat_node(lookup_child(DevNode::Input, path)?))
}

pub(crate) fn stat_net_child(path: &str) -> Option<FileStat> {
    if path.contains('/') {
        return None;
    }
    Some(stat_node(lookup_child(DevNode::Net, path)?))
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
    console_tty_read(user_buf)
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

fn input_wait_interrupted() -> bool {
    current_has_unmasked_signal()
}

fn input_open_client() -> Arc<UPIntrFreeCell<InputClient>> {
    let client = Arc::new(unsafe { UPIntrFreeCell::new(InputClient::new()) });
    INPUT_STATE.exclusive_session(|state| {
        if state.device_created && state.device.supports_ev(EV_REP) {
            let mut inner = client.exclusive_access();
            let _ = inner.push_event(linux_input_event(
                EV_REP,
                REP_DELAY,
                INPUT_DEFAULT_REP_DELAY_MS,
            ));
            let _ = inner.push_event(linux_input_event(
                EV_REP,
                REP_PERIOD,
                INPUT_DEFAULT_REP_PERIOD_MS,
            ));
        }
        state.clients.push(client.clone());
    });
    client
}

fn linux_input_event(event_type: u16, code: u16, value: i32) -> LinuxInputEvent {
    let nanos = crate::timer::wall_time_nanos();
    LinuxInputEvent {
        tv_sec: (nanos / 1_000_000_000) as usize,
        tv_usec: ((nanos / 1_000) % 1_000_000) as usize,
        event_type,
        code,
        value,
    }
}

fn input_event_to_bytes(event: LinuxInputEvent, out: &mut [u8]) {
    out[0..core::mem::size_of::<usize>()].copy_from_slice(&event.tv_sec.to_ne_bytes());
    out[8..16].copy_from_slice(&event.tv_usec.to_ne_bytes());
    out[16..18].copy_from_slice(&event.event_type.to_ne_bytes());
    out[18..20].copy_from_slice(&event.code.to_ne_bytes());
    out[20..24].copy_from_slice(&event.value.to_ne_bytes());
}

fn input_event_from_bytes(input: &[u8]) -> LinuxInputEvent {
    let mut word = [0u8; core::mem::size_of::<usize>()];
    word.copy_from_slice(&input[0..8]);
    let tv_sec = usize::from_ne_bytes(word);
    word.copy_from_slice(&input[8..16]);
    let tv_usec = usize::from_ne_bytes(word);
    let mut half = [0u8; 2];
    half.copy_from_slice(&input[16..18]);
    let event_type = u16::from_ne_bytes(half);
    half.copy_from_slice(&input[18..20]);
    let code = u16::from_ne_bytes(half);
    let mut value = [0u8; 4];
    value.copy_from_slice(&input[20..24]);
    LinuxInputEvent {
        tv_sec,
        tv_usec,
        event_type,
        code,
        value: i32::from_ne_bytes(value),
    }
}

fn read_input_event(
    client: &Arc<UPIntrFreeCell<InputClient>>,
    mut user_buf: UserBuffer,
    flags: OpenFlags,
) -> usize {
    let want_to_read = user_buf.len() / INPUT_EVENT_SIZE * INPUT_EVENT_SIZE;
    if want_to_read == 0 {
        return 0;
    }
    loop {
        let mut inner = client.exclusive_access();
        let available = inner.available_read();
        if available == 0 {
            if flags.contains(OpenFlags::NONBLOCK) || input_wait_interrupted() {
                return 0;
            }
            let task_cx_ptr = inner.sleep_reader();
            drop(inner);
            schedule(task_cx_ptr);
            continue;
        }

        let read_len = available.min(want_to_read);
        let mut kernel_buf = vec![0u8; read_len];
        let mut copied = 0usize;
        while copied < read_len {
            let Some(event) = inner.events.pop_front() else {
                break;
            };
            input_event_to_bytes(event, &mut kernel_buf[copied..copied + INPUT_EVENT_SIZE]);
            copied += INPUT_EVENT_SIZE;
        }
        drop(inner);
        return user_buf.copy_from_slice(&kernel_buf[..copied]);
    }
}

fn read_input_mice(flags: OpenFlags, mut user_buf: UserBuffer) -> usize {
    let want_to_read = user_buf.len() / 3 * 3;
    if want_to_read == 0 {
        return 0;
    }
    loop {
        let mut state = INPUT_STATE.exclusive_access();
        if state.mice_queue.is_empty() {
            if flags.contains(OpenFlags::NONBLOCK) || input_wait_interrupted() {
                return 0;
            }
            let task_cx_ptr = state.sleep_mice_reader();
            drop(state);
            schedule(task_cx_ptr);
            continue;
        }

        let read_len = state.mice_queue.len().min(want_to_read) / 3 * 3;
        let mut kernel_buf = vec![0u8; read_len];
        for byte in kernel_buf.iter_mut() {
            *byte = state.mice_queue.pop_front().unwrap_or(0);
        }
        drop(state);
        return user_buf.copy_from_slice(&kernel_buf);
    }
}

fn emit_input_event(event: LinuxInputEvent) {
    let (readers, poll_readers) = {
        let state = INPUT_STATE.exclusive_access();
        if !state.device_created {
            return;
        }
        let grabbed = state.clients.iter().any(|client| {
            let client = client.exclusive_access();
            !client.closed && client.grabbed
        });
        let mut readers = VecDeque::new();
        let mut poll_readers = Vec::new();
        for client in &state.clients {
            let mut client = client.exclusive_access();
            if client.closed || (grabbed && !client.grabbed) {
                continue;
            }
            let (mut client_readers, client_poll_readers) = client.push_event(event);
            readers.append(&mut client_readers);
            poll_readers.extend(client_poll_readers);
        }
        (readers, poll_readers)
    };
    wake_tasks(readers);
    PollWaiter::wake_all(poll_readers);
}

fn emit_mice_packet(buttons: u8) {
    let readers = {
        let mut state = INPUT_STATE.exclusive_access();
        if !state.device_created {
            return;
        }
        while state.mice_queue.len() + 3 > INPUT_MICE_QUEUE_CAPACITY {
            state.mice_queue.pop_front();
        }
        state.mice_queue.push_back(buttons);
        state.mice_queue.push_back(0);
        state.mice_queue.push_back(0);
        core::mem::take(&mut state.mice_read_wait_queue)
    };
    wake_tasks(readers);
}

fn should_deliver_uinput_event(device: &InputDeviceConfig, event: LinuxInputEvent) -> bool {
    match event.event_type {
        EV_REL => device.supports_rel(event.code) && event.value != 0,
        EV_KEY => device.supports_key(event.code),
        _ => false,
    }
}

fn write_uinput(user_buf: UserBuffer, config: &UPIntrFreeCell<UInputConfig>) -> usize {
    let len = user_buf.len();
    let mut data = Vec::with_capacity(len);
    for slice in user_buf.buffers.iter() {
        data.extend_from_slice(slice);
    }

    if data.len() >= UINPUT_MAX_NAME_SIZE && data.len() != INPUT_EVENT_SIZE {
        config
            .exclusive_access()
            .device
            .set_name_from_user_dev(&data[..UINPUT_MAX_NAME_SIZE]);
        return len;
    }

    if data.len() % INPUT_EVENT_SIZE != 0 {
        return len;
    }

    for chunk in data.chunks_exact(INPUT_EVENT_SIZE) {
        let event = input_event_from_bytes(chunk);
        let mut cfg = config.exclusive_access();
        match event.event_type {
            EV_SYN if event.code == SYN_REPORT => {
                if cfg.packet_has_event {
                    cfg.packet_has_event = false;
                    drop(cfg);
                    emit_input_event(linux_input_event(EV_SYN, SYN_REPORT, 0));
                }
            }
            EV_KEY if should_deliver_uinput_event(&cfg.device, event) => {
                if cfg.device.supports_ev(EV_REP) && event.code == KEY_X && event.value == 0 {
                    drop(cfg);
                    emit_input_event(linux_input_event(EV_KEY, KEY_X, 2));
                    emit_input_event(linux_input_event(EV_SYN, SYN_REPORT, 0));
                    cfg = config.exclusive_access();
                }
                cfg.packet_has_event = true;
                let code = event.code;
                let value = event.value;
                drop(cfg);
                emit_input_event(linux_input_event(EV_KEY, code, value));
                if code == BTN_RIGHT {
                    emit_mice_packet(if value != 0 { 0x02 } else { 0x00 });
                }
            }
            EV_REL if should_deliver_uinput_event(&cfg.device, event) => {
                cfg.packet_has_event = true;
                let code = event.code;
                let value = event.value;
                drop(cfg);
                emit_input_event(linux_input_event(EV_REL, code, value));
            }
            _ => {}
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
        let poll_writers = inner.input_buffer_mut(endpoint).wake_write_poll_waiters();
        drop(inner);
        wake_task(writer);
        PollWaiter::wake_all(poll_writers);
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
        let poll_readers = inner.output_buffer_mut(endpoint).wake_read_poll_waiters();
        drop(inner);
        wake_task(reader);
        PollWaiter::wake_all(poll_readers);
        if already_written == want_to_write {
            return already_written;
        }
    }
}

fn dir_entries(node: DevNode) -> Option<&'static [DevDirEntry]> {
    match node {
        DevNode::Root => Some(&ROOT_DEV_DIR_ENTRIES),
        DevNode::Misc => Some(&MISC_DEV_DIR_ENTRIES),
        DevNode::Input => Some(&INPUT_DEV_DIR_ENTRIES),
        DevNode::Net => Some(&NET_DEV_DIR_ENTRIES),
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
        entry_buf[0..8].copy_from_slice(&(entry.node.ino() as u64).to_ne_bytes());
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
        ino: DevNode::Pts.ino() as u64,
        name: b".".to_vec(),
        dtype: DT_DIR,
    });
    entries.push(DynamicDirEntry {
        ino: DevNode::Root.ino() as u64,
        name: b"..".to_vec(),
        dtype: DT_DIR,
    });
    for id in ids {
        entries.push(DynamicDirEntry {
            ino: PTY_INO_BASE as u64 + id as u64,
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

fn static_name(name: &[u8]) -> String {
    let mut output = String::new();
    for &byte in name {
        output.push(byte as char);
    }
    output
}

fn raw_static_entries(entries: &[DevDirEntry]) -> Vec<RawDirEntry> {
    entries
        .iter()
        .map(|entry| RawDirEntry {
            ino: entry.node.ino(),
            name: static_name(entry.name),
            dtype: entry.dtype,
        })
        .collect()
}

fn raw_pts_entries() -> Vec<RawDirEntry> {
    let ids = PTY_TABLE.exclusive_session(|table| table.active_ids());
    let mut entries = Vec::with_capacity(ids.len() + 2);
    entries.push(RawDirEntry {
        ino: DevNode::Pts.ino(),
        name: ".".into(),
        dtype: DT_DIR,
    });
    entries.push(RawDirEntry {
        ino: DevNode::Root.ino(),
        name: "..".into(),
        dtype: DT_DIR,
    });
    for id in ids {
        entries.push(RawDirEntry {
            ino: PTY_INO_BASE + id as u32,
            name: id.to_string(),
            dtype: DT_CHR,
        });
    }
    entries
}

impl FileSystemBackend for DevFs {
    fn root_ino(&self) -> u32 {
        DevNode::Root.ino()
    }

    fn statfs(&mut self) -> FileSystemStat {
        FileSystemStat {
            magic: DEVFS_MAGIC,
            block_size: 4096,
            blocks: 0,
            free_blocks: 0,
            available_blocks: 0,
            files: 1024,
            free_files: 1024,
            max_name_len: 255,
            flags: 0,
        }
    }

    fn lookup_component_from(
        &mut self,
        parent_ino: u32,
        component: &str,
    ) -> FsResult<(u32, FsNodeKind)> {
        let parent = node_from_ino(parent_ino).ok_or(FsError::NotFound)?;
        if parent == DevNode::Pts
            && let Some(id) = parse_pts_id(component)
            && PTY_TABLE.exclusive_session(|table| table.get(id)).is_some()
        {
            return Ok((PTY_INO_BASE + id as u32, FsNodeKind::CharacterDevice));
        }
        let node = lookup_child(parent, component).ok_or(FsError::NotFound)?;
        Ok((node.ino(), node.kind()))
    }

    fn create_file(&mut self, _parent_ino: u32, _leaf_name: &str) -> FsResult<u32> {
        Err(FsError::ReadOnly)
    }

    fn create_dir(&mut self, _parent_ino: u32, _leaf_name: &str, _mode: u32) -> FsResult<u32> {
        Err(FsError::ReadOnly)
    }

    fn link(&mut self, _parent_ino: u32, _leaf_name: &str, _child_ino: u32) -> FsResult {
        Err(FsError::ReadOnly)
    }

    fn symlink(&mut self, _parent_ino: u32, _leaf_name: &str, _target: &[u8]) -> FsResult {
        Err(FsError::ReadOnly)
    }

    fn unlink(&mut self, _parent_ino: u32, _leaf_name: &str) -> FsResult {
        Err(FsError::ReadOnly)
    }

    fn rename(
        &mut self,
        _src_dir: u32,
        _src_name: &str,
        _dst_dir: u32,
        _dst_name: &str,
    ) -> FsResult {
        Err(FsError::ReadOnly)
    }

    fn set_len(&mut self, _ino: u32, _len: u64) -> FsResult {
        Err(FsError::InvalidInput)
    }

    fn stat(&mut self, ino: u32) -> FsResult<FileStat> {
        if let Some(id) = pty_id_from_ino(ino) {
            return stat_pty_slave(id).ok_or(FsError::NotFound);
        }
        let node = node_from_ino(ino).ok_or(FsError::NotFound)?;
        Ok(stat_node(node))
    }

    fn readlink(&mut self, _ino: u32, _buf: &mut [u8]) -> FsResult<usize> {
        Err(FsError::InvalidInput)
    }

    fn read_at(&mut self, _ino: u32, _buf: &mut [u8], _offset: u64) -> usize {
        0
    }

    fn write_at(&mut self, _ino: u32, _buf: &[u8], _offset: u64) -> usize {
        0
    }

    fn read_dirent64(&mut self, ino: u32, offset: u64, buf: &mut [u8]) -> FsResult<(usize, u64)> {
        match node_from_ino(ino).ok_or(FsError::NotFound)? {
            DevNode::Root => {
                write_dir_entries(&raw_static_entries(&ROOT_DEV_DIR_ENTRIES), offset, buf)
            }
            DevNode::Misc => {
                write_dir_entries(&raw_static_entries(&MISC_DEV_DIR_ENTRIES), offset, buf)
            }
            DevNode::Input => {
                write_dir_entries(&raw_static_entries(&INPUT_DEV_DIR_ENTRIES), offset, buf)
            }
            DevNode::Net => {
                write_dir_entries(&raw_static_entries(&NET_DEV_DIR_ENTRIES), offset, buf)
            }
            DevNode::Pts => write_dir_entries(&raw_pts_entries(), offset, buf),
            _ => Err(FsError::NotDir),
        }
    }

    fn list_root_names(&mut self) -> Vec<String> {
        ROOT_DEV_DIR_ENTRIES
            .iter()
            .filter(|entry| entry.name != b"." && entry.name != b"..")
            .map(|entry| static_name(entry.name))
            .collect()
    }
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
            | DevNode::Input
            | DevNode::Net
            | DevNode::Pts
            | DevNode::Null
            | DevNode::Rtc
            | DevNode::PtMx
            | DevNode::LoopControl => 0,
            DevNode::Zero | DevNode::Full => read_zero(user_buf),
            DevNode::Random | DevNode::Urandom => read_random(user_buf),
            DevNode::Tty | DevNode::TtyS0 | DevNode::Tty8 | DevNode::Tty9 => read_console(user_buf),
            DevNode::Loop0 => read_loop0(&self.offset, user_buf),
            DevNode::UInput => 0,
            DevNode::InputEvent0 => 0,
            DevNode::InputMice => read_input_mice(self.status_flags.get(), user_buf),
            DevNode::Tun => 0,
        }
    }

    fn write(&self, user_buf: UserBuffer) -> usize {
        match self.node {
            DevNode::Root
            | DevNode::Misc
            | DevNode::Input
            | DevNode::Net
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
            DevNode::UInput | DevNode::InputEvent0 | DevNode::InputMice | DevNode::Tun => 0,
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

    fn check_write(&self, _len: usize, _append: bool) -> FsResult {
        if self.node == DevNode::Loop0 {
            if !loop_device_is_attached(0) {
                return Err(FsError::NoDeviceOrAddress);
            }
            if loop_device_is_read_only(0) {
                return Err(FsError::PermissionDenied);
            }
        }
        Ok(())
    }

    fn check_write_at(&self, _offset: usize, _len: usize) -> FsResult {
        if self.node == DevNode::Loop0 {
            if !loop_device_is_attached(0) {
                return Err(FsError::NoDeviceOrAddress);
            }
            if loop_device_is_read_only(0) {
                return Err(FsError::PermissionDenied);
            }
        }
        Ok(())
    }

    fn seek(&self, offset: i64, whence: SeekWhence) -> FsResult<usize> {
        if self.node == DevNode::Loop0 {
            return seek_loop0(&self.offset, offset, whence);
        }
        if matches!(self.node, DevNode::Null | DevNode::Zero | DevNode::Full) {
            if matches!(whence, SeekWhence::Data | SeekWhence::Hole) {
                return Err(FsError::InvalidInput);
            }
            return Ok(seek_null_like(&self.offset));
        }
        Err(FsError::IllegalSeek)
    }

    fn poll(&self, events: PollEvents) -> PollEvents {
        self.poll_with_wait(events, None)
    }

    fn poll_with_wait(
        &self,
        events: PollEvents,
        waiter: Option<&alloc::sync::Arc<PollWaiter>>,
    ) -> PollEvents {
        match self.node {
            DevNode::Tty | DevNode::TtyS0 | DevNode::Tty8 | DevNode::Tty9 => {
                let mut ready = PollEvents::empty();
                ready |= if waiter.is_some() {
                    console_tty_poll_with_wait(events, waiter)
                } else {
                    console_tty_poll(events)
                };
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
        let mut stat = stat_node(self.node);
        if let Some(mount_id) = self.mount_id {
            stat.dev = mount_id.0 as u64;
        }
        Ok(stat)
    }

    fn status_flags(&self) -> OpenFlags {
        self.status_flags.get()
    }

    fn set_status_flags(&self, flags: OpenFlags) {
        self.status_flags.set(flags);
    }

    fn working_dir(&self) -> Option<WorkingDir> {
        if !matches!(
            self.node,
            DevNode::Root | DevNode::Misc | DevNode::Pts | DevNode::Input | DevNode::Net
        ) {
            return None;
        }
        self.mount_id
            .map(|mount_id| WorkingDir::new(mount_id, self.node.ino()))
    }

    fn vfs_node_id(&self) -> Option<VfsNodeId> {
        self.mount_id
            .map(|mount_id| VfsNodeId::new(mount_id, self.node.ino()))
    }

    fn vfs_mount_id(&self) -> Option<MountId> {
        self.mount_id
    }

    fn read_dirent64(&self, user_buf: UserBuffer) -> FsResult<isize> {
        let mut offset = self.offset.exclusive_access();
        if self.node == DevNode::Pts {
            let entries = pts_dir_entries();
            return copy_dynamic_dirents(entries.as_slice(), &mut offset, user_buf);
        }
        let entries = dir_entries(self.node).ok_or(FsError::NotDir)?;
        copy_dirents(entries, &mut offset, user_buf)
    }

    fn is_tty(&self) -> bool {
        self.node.is_tty()
    }

    fn is_rtc(&self) -> bool {
        self.node == DevNode::Rtc
    }

    fn is_devfs_dir(&self) -> bool {
        matches!(
            self.node,
            DevNode::Root | DevNode::Misc | DevNode::Pts | DevNode::Input | DevNode::Net
        )
    }

    fn is_devfs_misc_dir(&self) -> bool {
        self.node == DevNode::Misc
    }

    fn is_devfs_pts_dir(&self) -> bool {
        self.node == DevNode::Pts
    }

    fn is_devfs_input_dir(&self) -> bool {
        self.node == DevNode::Input
    }

    fn is_devfs_net_dir(&self) -> bool {
        self.node == DevNode::Net
    }

    fn is_dev_random(&self) -> bool {
        matches!(self.node, DevNode::Random | DevNode::Urandom)
    }

    fn is_dev_full(&self) -> bool {
        self.node == DevNode::Full
    }

    fn supports_splice_read(&self) -> bool {
        self.readable && matches!(self.node, DevNode::Zero | DevNode::Full | DevNode::Loop0)
    }

    fn supports_splice_write(&self) -> bool {
        self.writable
            && matches!(
                self.node,
                DevNode::Null | DevNode::Zero | DevNode::Full | DevNode::Loop0
            )
    }
}

impl File for UInputFile {
    fn as_any(&self) -> &dyn core::any::Any {
        self
    }

    fn readable(&self) -> bool {
        false
    }

    fn writable(&self) -> bool {
        true
    }

    fn read(&self, _user_buf: UserBuffer) -> usize {
        0
    }

    fn write(&self, user_buf: UserBuffer) -> usize {
        write_uinput(user_buf, &self.config)
    }

    fn poll(&self, events: PollEvents) -> PollEvents {
        let mut ready = PollEvents::empty();
        if events.contains(PollEvents::POLLOUT) {
            ready |= PollEvents::POLLOUT;
        }
        ready
    }

    fn stat(&self) -> FsResult<FileStat> {
        Ok(stat_node(DevNode::UInput))
    }

    fn status_flags(&self) -> OpenFlags {
        self.status_flags.get()
    }

    fn set_status_flags(&self, flags: OpenFlags) {
        self.status_flags.set(flags);
    }
}

impl File for InputEventFile {
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
        read_input_event(&self.client, user_buf, self.status_flags.get())
    }

    fn write(&self, _user_buf: UserBuffer) -> usize {
        0
    }

    fn poll(&self, events: PollEvents) -> PollEvents {
        self.poll_with_wait(events, None)
    }

    fn poll_with_wait(&self, events: PollEvents, waiter: Option<&Arc<PollWaiter>>) -> PollEvents {
        let mut ready = PollEvents::empty();
        let mut client = self.client.exclusive_access();
        if let Some(waiter) = waiter
            && events.intersects(PollEvents::POLLIN | PollEvents::POLLPRI)
        {
            client.read_poll_wait_queue.register(waiter);
        }
        if events.intersects(PollEvents::POLLIN | PollEvents::POLLPRI)
            && client.available_read() > 0
        {
            ready |= PollEvents::POLLIN;
        }
        if events.contains(PollEvents::POLLOUT) && self.writable {
            ready |= PollEvents::POLLOUT;
        }
        ready
    }

    fn stat(&self) -> FsResult<FileStat> {
        Ok(stat_node(DevNode::InputEvent0))
    }

    fn status_flags(&self) -> OpenFlags {
        self.status_flags.get()
    }

    fn set_status_flags(&self, flags: OpenFlags) {
        self.status_flags.set(flags);
    }
}

impl Drop for InputEventFile {
    fn drop(&mut self) {
        let mut client = self.client.exclusive_access();
        client.closed = true;
        client.grabbed = false;
        client.events.clear();
    }
}

impl Drop for PtyFile {
    fn drop(&mut self) {
        let (readers, writers, poll_readers, poll_writers, remove) = {
            let mut pair = self.pair.exclusive_access();
            match self.endpoint {
                PtyEndpoint::Master => {
                    pair.master_open = pair.master_open.saturating_sub(1);
                    let readers = pair.master_to_slave.wake_all_readers();
                    let writers = pair.slave_to_master.wake_all_writers();
                    let poll_readers = pair.master_to_slave.wake_read_poll_waiters();
                    let poll_writers = pair.slave_to_master.wake_write_poll_waiters();
                    (
                        readers,
                        writers,
                        poll_readers,
                        poll_writers,
                        pair.is_closed(),
                    )
                }
                PtyEndpoint::Slave => {
                    pair.slave_open = pair.slave_open.saturating_sub(1);
                    let readers = pair.slave_to_master.wake_all_readers();
                    let writers = pair.master_to_slave.wake_all_writers();
                    let poll_readers = pair.slave_to_master.wake_read_poll_waiters();
                    let poll_writers = pair.master_to_slave.wake_write_poll_waiters();
                    (
                        readers,
                        writers,
                        poll_readers,
                        poll_writers,
                        pair.is_closed(),
                    )
                }
            }
        };
        wake_tasks(readers);
        wake_tasks(writers);
        PollWaiter::wake_all(poll_readers);
        PollWaiter::wake_all(poll_writers);
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
        self.poll_with_wait(events, None)
    }

    fn poll_with_wait(&self, events: PollEvents, waiter: Option<&Arc<PollWaiter>>) -> PollEvents {
        let mut pair = self.pair.exclusive_access();
        if let Some(waiter) = waiter {
            if self.readable && events.intersects(PollEvents::POLLIN | PollEvents::POLLPRI) {
                pair.input_buffer_mut(self.endpoint)
                    .register_read_poll_waiter(waiter);
            }
            if self.writable && events.contains(PollEvents::POLLOUT) {
                pair.output_buffer_mut(self.endpoint)
                    .register_write_poll_waiter(waiter);
            }
        }
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
                stat.ino = PTY_INO_BASE as u64 + self.id() as u64;
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
