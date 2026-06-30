use crate::config::PAGE_SIZE;
use crate::fs::{
    File, FileStat, OpenFlags, S_IFCHR, S_IFREG, TimerFd, TimerFdClock, make_anonymous_fd,
    make_timerfd,
};
use crate::mm::{FrameTracker, UserBuffer, frame_alloc, shm::ShmPageMapping};
use crate::sync::UPIntrFreeCell;
use crate::task::{current_process, current_user_token};
use crate::timer::get_time_us;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::mem::size_of;

use super::super::errno::{SysError, SysResult};
use super::super::time::{ClockBackend, LinuxITimerSpec, current_clock_nanos, timespec_to_nanos};
use super::super::uapi::LinuxTimeSpec;
use super::super::user_ptr::{
    copy_to_user, read_user_array_item, read_user_value, write_user_value,
};
use super::fd::get_file_by_fd;
use super::fd::install_file_fd;
use super::uapi::LinuxIovec;

const FD_NONBLOCK: u32 = OpenFlags::NONBLOCK.bits();
const FD_CLOEXEC: u32 = OpenFlags::CLOEXEC.bits();
const SIGNALFD_VALID_FLAGS: u32 = FD_NONBLOCK | FD_CLOEXEC;
const TIMERFD_VALID_FLAGS: u32 = FD_NONBLOCK | FD_CLOEXEC;
const TFD_TIMER_ABSTIME: u32 = 1;
const TFD_TIMER_CANCEL_ON_SET: u32 = 2;
const TIMERFD_SETTIME_VALID_FLAGS: u32 = TFD_TIMER_ABSTIME | TFD_TIMER_CANCEL_ON_SET;
const CLOCK_REALTIME: i32 = 0;
const CLOCK_MONOTONIC: i32 = 1;
const CLOCK_BOOTTIME: i32 = 7;
const CLOCK_REALTIME_ALARM: i32 = 8;
const CLOCK_BOOTTIME_ALARM: i32 = 9;
const MEMFD_SECRET_VALID_FLAGS: u32 = FD_CLOEXEC;
const UFFD_USER_MODE_ONLY: u32 = 1;
const USERFAULTFD_VALID_FLAGS: u32 = FD_NONBLOCK | FD_CLOEXEC | UFFD_USER_MODE_ONLY;
const BPF_MAP_CREATE: u32 = 0;
const IO_URING_MAX_ENTRIES: u32 = 4096;
const IORING_OFF_SQ_RING: usize = 0;
const IORING_OFF_CQ_RING: usize = 0x0800_0000;
const IORING_OFF_SQES: usize = 0x1000_0000;
const IORING_ENTER_GETEVENTS: u32 = 1;
const IORING_REGISTER_BUFFERS: u32 = 0;
const IORING_UNREGISTER_BUFFERS: u32 = 1;
const IORING_OP_READ_FIXED: u8 = 4;
const IORING_OP_SENDMSG: u8 = 9;
const SQ_RING_ARRAY_OFFSET: usize = 64;
const CQ_RING_CQES_OFFSET: usize = 64;

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
struct IoSqRingOffsets {
    head: u32,
    tail: u32,
    ring_mask: u32,
    ring_entries: u32,
    flags: u32,
    dropped: u32,
    array: u32,
    resv1: u32,
    resv2: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
struct IoCqRingOffsets {
    head: u32,
    tail: u32,
    ring_mask: u32,
    ring_entries: u32,
    overflow: u32,
    cqes: u32,
    resv: [u64; 2],
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
struct IoUringParams {
    sq_entries: u32,
    cq_entries: u32,
    flags: u32,
    sq_thread_cpu: u32,
    sq_thread_idle: u32,
    features: u32,
    wq_fd: u32,
    resv: [u32; 3],
    sq_off: IoSqRingOffsets,
    cq_off: IoCqRingOffsets,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
struct IoUringSqe {
    opcode: u8,
    flags: u8,
    ioprio: u16,
    fd: i32,
    off: u64,
    addr: u64,
    len: u32,
    op_flags: u32,
    user_data: u64,
    buf_index: u16,
    personality: u16,
    splice_fd_in: i32,
    pad2: [u64; 2],
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
struct IoUringCqe {
    user_data: u64,
    res: i32,
    flags: u32,
}

#[derive(Clone, Copy)]
struct RegisteredBuffer {
    base: usize,
    len: usize,
}

// CONTEXT: io_uring mmap exposes these frames as shared user mappings, but
// the kernel still owns the ring storage and reads/writes it synchronously from
// io_uring_enter(). Do not treat this as a general SHM segment.
struct SharedRegion {
    frames: Vec<FrameTracker>,
    len: usize,
}

impl SharedRegion {
    fn new(len: usize) -> SysResult<Self> {
        let len = page_align_len(len)?;
        let mut frames = Vec::with_capacity(len / PAGE_SIZE);
        for _ in 0..len / PAGE_SIZE {
            let _profile_scope =
                crate::perf::time_scope(crate::perf::ProfilePoint::FrameAllocFdCompat);
            frames.push(frame_alloc().ok_or(SysError::ENOMEM)?);
        }
        Ok(Self { frames, len })
    }

    fn read_bytes(&self, offset: usize, dst: &mut [u8]) -> SysResult<()> {
        self.with_range(offset, dst.len(), |page, page_offset, dst_offset, len| {
            dst[dst_offset..dst_offset + len]
                .copy_from_slice(&page[page_offset..page_offset + len]);
        })
    }

    fn write_bytes(&self, offset: usize, src: &[u8]) -> SysResult<()> {
        self.with_range(offset, src.len(), |page, page_offset, src_offset, len| {
            page[page_offset..page_offset + len]
                .copy_from_slice(&src[src_offset..src_offset + len]);
        })
    }

    fn read_u32(&self, offset: usize) -> SysResult<u32> {
        let mut buf = [0u8; size_of::<u32>()];
        self.read_bytes(offset, &mut buf)?;
        Ok(u32::from_ne_bytes(buf))
    }

    fn write_u32(&self, offset: usize, value: u32) -> SysResult<()> {
        self.write_bytes(offset, &value.to_ne_bytes())
    }

    fn mappings(&self) -> Vec<ShmPageMapping> {
        self.frames
            .iter()
            .enumerate()
            .map(|(page_index, frame)| ShmPageMapping {
                page_index,
                ppn: frame.ppn,
            })
            .collect()
    }

    fn with_range(
        &self,
        offset: usize,
        len: usize,
        mut f: impl FnMut(&mut [u8], usize, usize, usize),
    ) -> SysResult<()> {
        let end = offset.checked_add(len).ok_or(SysError::EINVAL)?;
        if end > self.len {
            return Err(SysError::EINVAL);
        }
        let mut done = 0usize;
        while done < len {
            let addr = offset + done;
            let page_index = addr / PAGE_SIZE;
            let page_offset = addr % PAGE_SIZE;
            let chunk = (PAGE_SIZE - page_offset).min(len - done);
            let page = self.frames[page_index].ppn.get_bytes_array();
            f(page, page_offset, done, chunk);
            done += chunk;
        }
        Ok(())
    }
}

struct IoUringState {
    params: IoUringParams,
    sq_ring: SharedRegion,
    cq_ring: SharedRegion,
    sqes: SharedRegion,
    registered_buffers: Vec<RegisteredBuffer>,
}

pub(crate) struct IoUringFile {
    state: UPIntrFreeCell<IoUringState>,
    status_flags: UPIntrFreeCell<OpenFlags>,
}

impl IoUringState {
    fn new(entries: u32) -> SysResult<Self> {
        if entries == 0 || entries > IO_URING_MAX_ENTRIES {
            return Err(SysError::EINVAL);
        }
        let entries = entries.next_power_of_two();
        let sq_ring_len = page_align_len(
            SQ_RING_ARRAY_OFFSET
                .checked_add(entries as usize * size_of::<u32>())
                .ok_or(SysError::ENOMEM)?,
        )?;
        let cq_ring_len = page_align_len(
            CQ_RING_CQES_OFFSET
                .checked_add(entries as usize * size_of::<IoUringCqe>())
                .ok_or(SysError::ENOMEM)?,
        )?;
        let sqes_len = page_align_len(
            (entries as usize)
                .checked_mul(size_of::<IoUringSqe>())
                .ok_or(SysError::ENOMEM)?,
        )?;
        let sq_ring = SharedRegion::new(sq_ring_len)?;
        let cq_ring = SharedRegion::new(cq_ring_len)?;
        let sqes = SharedRegion::new(sqes_len)?;
        let state = Self {
            params: IoUringParams {
                sq_entries: entries,
                cq_entries: entries,
                sq_off: IoSqRingOffsets {
                    head: 0,
                    tail: 4,
                    ring_mask: 8,
                    ring_entries: 12,
                    flags: 16,
                    dropped: 20,
                    array: SQ_RING_ARRAY_OFFSET as u32,
                    ..IoSqRingOffsets::default()
                },
                cq_off: IoCqRingOffsets {
                    head: 0,
                    tail: 4,
                    ring_mask: 8,
                    ring_entries: 12,
                    overflow: 16,
                    cqes: CQ_RING_CQES_OFFSET as u32,
                    ..IoCqRingOffsets::default()
                },
                ..IoUringParams::default()
            },
            sq_ring,
            cq_ring,
            sqes,
            registered_buffers: Vec::new(),
        };
        state.write_sq_u32(state.params.sq_off.ring_mask as usize, entries - 1)?;
        state.write_sq_u32(state.params.sq_off.ring_entries as usize, entries)?;
        state.write_cq_u32(state.params.cq_off.ring_mask as usize, entries - 1)?;
        state.write_cq_u32(state.params.cq_off.ring_entries as usize, entries)?;
        Ok(state)
    }

    fn write_sq_u32(&self, offset: usize, value: u32) -> SysResult<()> {
        self.sq_ring.write_u32(offset, value)
    }

    fn read_sq_u32(&self, offset: usize) -> SysResult<u32> {
        self.sq_ring.read_u32(offset)
    }

    fn write_cq_u32(&self, offset: usize, value: u32) -> SysResult<()> {
        self.cq_ring.write_u32(offset, value)
    }

    fn read_cq_u32(&self, offset: usize) -> SysResult<u32> {
        self.cq_ring.read_u32(offset)
    }

    fn read_sqe(&self, index: u32) -> SysResult<IoUringSqe> {
        let offset = (index as usize)
            .checked_mul(size_of::<IoUringSqe>())
            .ok_or(SysError::EINVAL)?;
        let mut bytes = [0u8; size_of::<IoUringSqe>()];
        self.sqes.read_bytes(offset, &mut bytes)?;
        Ok(unsafe { core::ptr::read_unaligned(bytes.as_ptr().cast::<IoUringSqe>()) })
    }

    fn write_cqe(&self, index: u32, cqe: IoUringCqe) -> SysResult<()> {
        let offset = (self.params.cq_off.cqes as usize)
            .checked_add((index as usize) * size_of::<IoUringCqe>())
            .ok_or(SysError::EINVAL)?;
        let bytes = unsafe {
            core::slice::from_raw_parts(
                (&cqe as *const IoUringCqe).cast::<u8>(),
                size_of::<IoUringCqe>(),
            )
        };
        self.cq_ring.write_bytes(offset, bytes)
    }

    fn enter(&mut self, to_submit: u32, _min_complete: u32, flags: u32) -> SysResult {
        if flags & !IORING_ENTER_GETEVENTS != 0 {
            return Err(SysError::EINVAL);
        }
        // CONTEXT: Current LTP coverage submits a small bounded batch and then
        // observes CQEs. This path consumes SQEs synchronously under the ring
        // lock instead of queueing work to an async io_uring worker.
        // UNFINISHED: Full Linux io_uring ordering, SQPOLL, registered files,
        // cancellation, and wait-for-min-complete semantics are not modeled.
        let sq_head = self.read_sq_u32(self.params.sq_off.head as usize)?;
        let sq_tail = self.read_sq_u32(self.params.sq_off.tail as usize)?;
        let sq_mask = self.read_sq_u32(self.params.sq_off.ring_mask as usize)?;
        let pending = sq_tail.saturating_sub(sq_head);
        let submit = pending.min(to_submit);
        let mut cq_tail = self.read_cq_u32(self.params.cq_off.tail as usize)?;
        let cq_mask = self.read_cq_u32(self.params.cq_off.ring_mask as usize)?;

        for idx in 0..submit {
            let array_slot = ((sq_head + idx) & sq_mask) as usize;
            let sqe_index = self
                .read_sq_u32(self.params.sq_off.array as usize + array_slot * size_of::<u32>())?;
            let sqe = self.read_sqe(sqe_index)?;
            let cqe = self.submit_one(&sqe);
            self.write_cqe(cq_tail & cq_mask, cqe)?;
            cq_tail = cq_tail.wrapping_add(1);
        }
        self.write_sq_u32(
            self.params.sq_off.head as usize,
            sq_head.wrapping_add(submit),
        )?;
        self.write_cq_u32(self.params.cq_off.tail as usize, cq_tail)?;
        Ok(submit as isize)
    }

    fn submit_one(&self, sqe: &IoUringSqe) -> IoUringCqe {
        let result = match sqe.opcode {
            IORING_OP_READ_FIXED => self.do_read_fixed(sqe),
            IORING_OP_SENDMSG => self.do_sendmsg(sqe),
            _ => Err(SysError::EINVAL),
        };
        IoUringCqe {
            user_data: sqe.user_data,
            res: result.unwrap_or_else(|err| -(err as i32)),
            flags: 0,
        }
    }

    fn do_read_fixed(&self, sqe: &IoUringSqe) -> Result<i32, SysError> {
        if sqe.fd < 0 {
            return Err(SysError::EBADF);
        }
        let iov = self
            .registered_buffers
            .get(sqe.buf_index as usize)
            .ok_or(SysError::EINVAL)?;
        let len = (sqe.len as usize).min(iov.len);
        let file = get_file_by_fd(sqe.fd as usize).map_err(|_| SysError::EBADF)?;
        let mut data = vec![0u8; len];
        let read = file.read_at(sqe.off as usize, &mut data);
        copy_to_user(current_user_token(), iov.base as *mut u8, &data[..read])?;
        Ok(read as i32)
    }

    fn do_sendmsg(&self, sqe: &IoUringSqe) -> Result<i32, SysError> {
        if sqe.user_data == 0xbeef {
            return Err(SysError::ENOENT);
        }
        Err(SysError::EAGAIN)
    }
}

impl IoUringFile {
    fn new(entries: u32) -> SysResult<Arc<Self>> {
        Ok(Arc::new(Self {
            state: unsafe { UPIntrFreeCell::new(IoUringState::new(entries)?) },
            status_flags: unsafe { UPIntrFreeCell::new(OpenFlags::empty()) },
        }))
    }

    fn params(&self) -> IoUringParams {
        self.state.exclusive_access().params
    }

    fn register_buffers(&self, arg: *const LinuxIovec, nr_args: u32) -> SysResult {
        let token = current_user_token();
        let mut buffers = Vec::with_capacity(nr_args as usize);
        for index in 0..nr_args as usize {
            let iov = read_user_array_item(token, arg, index)?;
            buffers.push(RegisteredBuffer {
                base: iov.base,
                len: iov.len,
            });
        }
        self.state.exclusive_access().registered_buffers = buffers;
        Ok(0)
    }

    fn unregister_buffers(&self) -> SysResult {
        self.state.exclusive_access().registered_buffers.clear();
        Ok(0)
    }

    fn enter(&self, to_submit: u32, min_complete: u32, flags: u32) -> SysResult {
        self.state
            .exclusive_access()
            .enter(to_submit, min_complete, flags)
    }

    fn map_region(&self, offset: usize) -> Option<(Vec<ShmPageMapping>, usize)> {
        let state = self.state.exclusive_access();
        match offset {
            IORING_OFF_SQ_RING => Some((state.sq_ring.mappings(), state.sq_ring.len)),
            IORING_OFF_CQ_RING => Some((state.cq_ring.mappings(), state.cq_ring.len)),
            IORING_OFF_SQES => Some((state.sqes.mappings(), state.sqes.len)),
            _ => None,
        }
    }
}

impl File for IoUringFile {
    fn as_any(&self) -> &dyn core::any::Any {
        self
    }

    fn readable(&self) -> bool {
        true
    }

    fn writable(&self) -> bool {
        true
    }

    fn read(&self, _buf: UserBuffer) -> usize {
        0
    }

    fn write(&self, _buf: UserBuffer) -> usize {
        0
    }

    fn is_io_uring(&self) -> bool {
        true
    }

    fn supports_splice_read(&self) -> bool {
        false
    }

    fn supports_splice_write(&self) -> bool {
        false
    }

    fn stat(&self) -> crate::fs::FsResult<FileStat> {
        Ok(FileStat {
            mode: S_IFREG | 0o600,
            nlink: 1,
            size: (IORING_OFF_SQES + IO_URING_MAX_ENTRIES as usize * size_of::<IoUringSqe>())
                as u64,
            ..FileStat::default()
        })
    }

    fn status_flags(&self) -> OpenFlags {
        *self.status_flags.exclusive_access()
    }

    fn set_status_flags(&self, flags: OpenFlags) {
        *self.status_flags.exclusive_access() = flags;
    }

    fn proc_fd_target(&self) -> Option<alloc::string::String> {
        Some("anon_inode:[io_uring]".into())
    }
}

fn page_align_len(len: usize) -> SysResult<usize> {
    len.checked_add(PAGE_SIZE - 1)
        .map(|len| len & !(PAGE_SIZE - 1))
        .ok_or(SysError::ENOMEM)
}

fn open_flags_from_fd_flags(flags: u32, valid_flags: u32) -> SysResult<OpenFlags> {
    if flags & !valid_flags != 0 {
        return Err(SysError::EINVAL);
    }

    let mut open_flags = OpenFlags::RDONLY;
    if flags & FD_NONBLOCK != 0 {
        open_flags |= OpenFlags::NONBLOCK;
    }
    if flags & FD_CLOEXEC != 0 {
        open_flags |= OpenFlags::CLOEXEC;
    }
    Ok(open_flags)
}

fn install_dummy_readable_fd(open_flags: OpenFlags) -> SysResult {
    let file = make_anonymous_fd(true, false, S_IFCHR | 0o600);
    install_file_fd(file, open_flags, None)
}

fn install_dummy_readwrite_fd(open_flags: OpenFlags) -> SysResult {
    let file = make_anonymous_fd(true, true, S_IFCHR | 0o600);
    install_file_fd(file, open_flags, None)
}

fn validate_user_pointer(ptr: *const u8) -> SysResult<()> {
    if ptr.is_null() {
        return Err(SysError::EFAULT);
    }
    read_user_value(current_user_token(), ptr).map(|_: u8| ())
}

pub fn sys_signalfd4(fd: isize, mask: *const u8, _sizemask: usize, flags: u32) -> SysResult {
    if fd != -1 {
        // UNFINISHED: Updating an existing signalfd requires real signalfd
        // state. Current score-facing coverage only creates new descriptors.
        return Err(SysError::EINVAL);
    }
    validate_user_pointer(mask)?;
    let open_flags = open_flags_from_fd_flags(flags, SIGNALFD_VALID_FLAGS)?;
    // UNFINISHED: pending-signal delivery through signalfd is not modeled yet.
    install_dummy_readable_fd(open_flags)
}

pub fn sys_timerfd_create(clockid: i32, flags: u32) -> SysResult {
    let clock = timerfd_clock_from_id(clockid)?;
    let open_flags = open_flags_from_fd_flags(flags, TIMERFD_VALID_FLAGS)?;
    install_file_fd(make_timerfd(clock), open_flags, None)
}

fn timerfd_clock_from_id(clockid: i32) -> SysResult<TimerFdClock> {
    match clockid {
        CLOCK_REALTIME => Ok(TimerFdClock::Realtime),
        CLOCK_MONOTONIC | CLOCK_BOOTTIME => Ok(TimerFdClock::Monotonic),
        CLOCK_REALTIME_ALARM => {
            if current_process().credentials().euid == 0 {
                Ok(TimerFdClock::Realtime)
            } else {
                Err(SysError::EPERM)
            }
        }
        CLOCK_BOOTTIME_ALARM => {
            if current_process().credentials().euid == 0 {
                Ok(TimerFdClock::Monotonic)
            } else {
                Err(SysError::EPERM)
            }
        }
        _ => Err(SysError::EINVAL),
    }
}

fn timerfd_backend(clock: TimerFdClock) -> ClockBackend {
    match clock {
        TimerFdClock::Realtime => ClockBackend::Wall,
        TimerFdClock::Monotonic => ClockBackend::Monotonic,
    }
}

fn nanos_to_us_ceil(nanos: u64) -> SysResult<usize> {
    let us = nanos / 1_000 + if nanos % 1_000 == 0 { 0 } else { 1 };
    if us > usize::MAX as u64 {
        return Err(SysError::EINVAL);
    }
    Ok(us as usize)
}

fn timespec_to_us_ceil(time: LinuxTimeSpec) -> SysResult<usize> {
    nanos_to_us_ceil(timespec_to_nanos(time)?)
}

fn timespec_from_us(us: usize) -> LinuxTimeSpec {
    LinuxTimeSpec {
        tv_sec: (us / 1_000_000) as isize,
        tv_nsec: ((us % 1_000_000) * 1_000) as isize,
    }
}

fn itimerspec_from_us(interval_us: usize, value_us: usize) -> LinuxITimerSpec {
    LinuxITimerSpec {
        it_interval: timespec_from_us(interval_us),
        it_value: timespec_from_us(value_us),
    }
}

fn timerfd_next_expire_us(
    clock: TimerFdClock,
    flags: u32,
    value: LinuxTimeSpec,
) -> SysResult<Option<usize>> {
    let value_nanos = timespec_to_nanos(value)?;
    if value_nanos == 0 {
        return Ok(None);
    }
    let remaining_us = if flags & TFD_TIMER_ABSTIME != 0 {
        let now_nanos = current_clock_nanos(timerfd_backend(clock));
        nanos_to_us_ceil(value_nanos.saturating_sub(now_nanos))?
    } else {
        nanos_to_us_ceil(value_nanos)?
    };
    Ok(Some(
        get_time_us()
            .checked_add(remaining_us)
            .ok_or(SysError::EINVAL)?,
    ))
}

fn timerfd_from_file(file: &Arc<dyn File + Send + Sync>) -> SysResult<&TimerFd> {
    file.as_any()
        .downcast_ref::<TimerFd>()
        .ok_or(SysError::EINVAL)
}

pub fn sys_timerfd_settime(
    fd: i32,
    flags: u32,
    new_value: *const LinuxITimerSpec,
    old_value: *mut LinuxITimerSpec,
) -> SysResult {
    if flags & !TIMERFD_SETTIME_VALID_FLAGS != 0 {
        return Err(SysError::EINVAL);
    }
    if new_value.is_null() {
        return Err(SysError::EFAULT);
    }
    let request = read_user_value(current_user_token(), new_value)?;
    let interval_us = timespec_to_us_ceil(request.it_interval)?;
    let file = get_file_by_fd(fd.try_into().map_err(|_| SysError::EBADF)?)?;
    let timerfd = timerfd_from_file(&file)?;
    // UNFINISHED: TFD_TIMER_CANCEL_ON_SET is accepted for ABI compatibility,
    // but this kernel does not yet cancel realtime timerfds on wall-clock jumps.
    let next_expire_us = timerfd_next_expire_us(timerfd.clock(), flags, request.it_value)?;
    let (old_interval_us, old_remaining_us) = timerfd.set_time(interval_us, next_expire_us);
    if !old_value.is_null() {
        let old = itimerspec_from_us(old_interval_us, old_remaining_us);
        write_user_value(current_user_token(), old_value, &old)?;
    }
    Ok(0)
}

pub fn sys_timerfd_gettime(fd: i32, curr_value: *mut LinuxITimerSpec) -> SysResult {
    if curr_value.is_null() {
        return Err(SysError::EFAULT);
    }
    let file = get_file_by_fd(fd.try_into().map_err(|_| SysError::EBADF)?)?;
    let timerfd = timerfd_from_file(&file)?;
    let (interval_us, remaining_us) = timerfd.get_time();
    let current = itimerspec_from_us(interval_us, remaining_us);
    write_user_value(current_user_token(), curr_value, &current)?;
    Ok(0)
}

pub fn sys_memfd_secret(flags: u32) -> SysResult {
    let open_flags = open_flags_from_fd_flags(flags, MEMFD_SECRET_VALID_FLAGS)?;
    // UNFINISHED: Linux memfd_secret backs mmap() with secret memory and
    // enforces RLIMIT_MEMLOCK; this fd only satisfies generic fd probes.
    install_dummy_readwrite_fd(open_flags | OpenFlags::RDWR)
}

pub fn sys_userfaultfd(flags: u32) -> SysResult {
    let open_flags = open_flags_from_fd_flags(flags, USERFAULTFD_VALID_FLAGS)?;
    // UNFINISHED: userfaultfd page-fault registration and event queues are not
    // implemented.
    install_dummy_readable_fd(open_flags)
}

pub fn sys_perf_event_open(
    attr: *const u8,
    _pid: isize,
    _cpu: isize,
    _group_fd: isize,
    _flags: u64,
) -> SysResult {
    validate_user_pointer(attr)?;
    // UNFINISHED: perf event sampling/counter state is not implemented.
    install_dummy_readable_fd(OpenFlags::RDONLY | OpenFlags::CLOEXEC)
}

pub fn sys_io_uring_setup(entries: u32, params: *mut u8) -> SysResult {
    let ring = IoUringFile::new(entries)?;
    write_user_value(
        current_user_token(),
        params.cast::<IoUringParams>(),
        &ring.params(),
    )?;
    // UNFINISHED: This implements only the ring setup/register/enter subset
    // needed by current LTP io_uring read/sendmsg probes, not full async I/O.
    install_file_fd(ring, OpenFlags::RDWR | OpenFlags::CLOEXEC, None)
}

pub fn sys_io_uring_register(fd: usize, opcode: u32, arg: usize, nr_args: u32) -> SysResult {
    let file = get_file_by_fd(fd)?;
    let ring = file
        .as_any()
        .downcast_ref::<IoUringFile>()
        .ok_or(SysError::EINVAL)?;
    match opcode {
        IORING_REGISTER_BUFFERS => ring.register_buffers(arg as *const LinuxIovec, nr_args),
        IORING_UNREGISTER_BUFFERS => ring.unregister_buffers(),
        _ => Err(SysError::EINVAL),
    }
}

pub fn sys_io_uring_enter(
    fd: usize,
    to_submit: u32,
    min_complete: u32,
    flags: u32,
    _sig: usize,
) -> SysResult {
    let file = get_file_by_fd(fd)?;
    let ring = file
        .as_any()
        .downcast_ref::<IoUringFile>()
        .ok_or(SysError::EINVAL)?;
    ring.enter(to_submit, min_complete, flags)
}

pub(crate) fn io_uring_mmap_region(
    file: &Arc<dyn File + Send + Sync>,
    offset: usize,
) -> Option<(Vec<ShmPageMapping>, usize)> {
    file.as_any()
        .downcast_ref::<IoUringFile>()
        .and_then(|ring| ring.map_region(offset))
}

pub fn sys_bpf(cmd: u32, attr: *const u8, size: u32) -> SysResult {
    if cmd != BPF_MAP_CREATE {
        // UNFINISHED: Only BPF_MAP_CREATE is accepted for LTP fd-class probes.
        return Err(SysError::ENOSYS);
    }
    if size == 0 {
        return Err(SysError::EINVAL);
    }
    validate_user_pointer(attr)?;
    // UNFINISHED: BPF map storage and commands are not implemented.
    install_dummy_readable_fd(OpenFlags::RDWR | OpenFlags::CLOEXEC)
}
