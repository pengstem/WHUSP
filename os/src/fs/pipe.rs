use super::status_flags::StatusFlagsCell;
use super::{File, FileStat, FsResult, OpenFlags, PollEvents, S_IFIFO};
use crate::config::PAGE_SIZE;
use crate::fs::pipe_max_size;
use crate::mm::UserBuffer;
use crate::sync::UPIntrFreeCell;
use alloc::collections::VecDeque;
use alloc::sync::{Arc, Weak};
use alloc::vec;
use alloc::vec::Vec;

use crate::task::{
    TaskControlBlock, block_current_task_no_schedule, current_has_unmasked_signal, current_process,
    schedule, wakeup_task,
};

pub struct Pipe {
    readable: bool,
    writable: bool,
    buffer: Arc<UPIntrFreeCell<PipeRingBuffer>>,
    status_flags: StatusFlagsCell,
}

impl Pipe {
    pub fn read_end_with_buffer(buffer: Arc<UPIntrFreeCell<PipeRingBuffer>>) -> Self {
        Self {
            readable: true,
            writable: false,
            buffer,
            status_flags: StatusFlagsCell::new(OpenFlags::RDONLY),
        }
    }
    pub fn write_end_with_buffer(buffer: Arc<UPIntrFreeCell<PipeRingBuffer>>) -> Self {
        Self {
            readable: false,
            writable: true,
            buffer,
            status_flags: StatusFlagsCell::new(OpenFlags::WRONLY),
        }
    }
}

pub(super) const PIPE_MIN_CAPACITY: usize = PAGE_SIZE;
pub(super) const PIPE_DEFAULT_CAPACITY: usize = PAGE_SIZE * 16;
pub(super) const PIPE_MAX_CAPACITY: usize = PAGE_SIZE * 256;

#[derive(Copy, Clone, PartialEq)]
enum RingBufferStatus {
    Full,
    Empty,
    Normal,
}

pub struct PipeRingBuffer {
    arr: Vec<u8>,
    head: usize,
    tail: usize,
    status: RingBufferStatus,
    read_end: Option<Weak<Pipe>>,
    write_end: Option<Weak<Pipe>>,
    read_wait_queue: VecDeque<Arc<TaskControlBlock>>,
    write_wait_queue: VecDeque<Arc<TaskControlBlock>>,
}

impl PipeRingBuffer {
    pub fn new(capacity: usize) -> Self {
        let capacity = capacity.max(PIPE_MIN_CAPACITY);
        Self {
            arr: vec![0; capacity],
            head: 0,
            tail: 0,
            status: RingBufferStatus::Empty,
            read_end: None,
            write_end: None,
            read_wait_queue: VecDeque::new(),
            write_wait_queue: VecDeque::new(),
        }
    }
    fn capacity(&self) -> usize {
        self.arr.len()
    }
    pub fn set_read_end(&mut self, read_end: &Arc<Pipe>) {
        self.read_end = Some(Arc::downgrade(read_end));
    }
    pub fn set_write_end(&mut self, write_end: &Arc<Pipe>) {
        self.write_end = Some(Arc::downgrade(write_end));
    }
    pub fn write_byte(&mut self, byte: u8) {
        self.status = RingBufferStatus::Normal;
        self.arr[self.tail] = byte;
        self.tail = (self.tail + 1) % self.capacity();
        if self.tail == self.head {
            self.status = RingBufferStatus::Full;
        }
    }
    pub fn read_byte(&mut self) -> u8 {
        self.status = RingBufferStatus::Normal;
        let c = self.arr[self.head];
        self.head = (self.head + 1) % self.capacity();
        if self.head == self.tail {
            self.status = RingBufferStatus::Empty;
        }
        c
    }
    pub fn available_read(&self) -> usize {
        if self.status == RingBufferStatus::Empty {
            0
        } else if self.tail > self.head {
            self.tail - self.head
        } else {
            self.tail + self.capacity() - self.head
        }
    }
    pub fn available_write(&self) -> usize {
        if self.status == RingBufferStatus::Full {
            0
        } else {
            self.capacity() - self.available_read()
        }
    }
    fn resize(&mut self, capacity: usize) -> FsResult<usize> {
        let capacity = capacity.max(PIPE_MIN_CAPACITY);
        let occupied = self.available_read();
        if capacity < occupied {
            return Err(super::FsError::Busy);
        }
        let old_capacity = self.capacity();
        let mut next = vec![0; capacity];
        for (index, byte) in next.iter_mut().take(occupied).enumerate() {
            *byte = self.arr[(self.head + index) % old_capacity];
        }
        self.arr = next;
        self.head = 0;
        self.tail = if occupied == capacity { 0 } else { occupied };
        self.status = if occupied == 0 {
            RingBufferStatus::Empty
        } else if occupied == capacity {
            RingBufferStatus::Full
        } else {
            RingBufferStatus::Normal
        };
        Ok(capacity)
    }
    pub fn all_write_ends_closed(&self) -> bool {
        match &self.write_end {
            Some(write_end) => write_end.upgrade().is_none(),
            None => true,
        }
    }
    pub fn all_read_ends_closed(&self) -> bool {
        match &self.read_end {
            Some(read_end) => read_end.upgrade().is_none(),
            None => true,
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

/// Return (read_end, write_end)
pub(crate) fn default_pipe_capacity_for_current_process() -> usize {
    let credentials = current_process().credentials();
    if credentials.is_root() {
        PIPE_DEFAULT_CAPACITY
    } else {
        PIPE_DEFAULT_CAPACITY.min(pipe_max_size())
    }
}

pub fn make_pipe(capacity: usize) -> (Arc<Pipe>, Arc<Pipe>) {
    let buffer = Arc::new(unsafe { UPIntrFreeCell::new(PipeRingBuffer::new(capacity)) });
    let read_end = Arc::new(Pipe::read_end_with_buffer(buffer.clone()));
    let write_end = Arc::new(Pipe::write_end_with_buffer(buffer.clone()));
    let mut inner = buffer.exclusive_access();
    inner.set_read_end(&read_end);
    inner.set_write_end(&write_end);
    (read_end, write_end)
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

fn pipe_wait_interrupted() -> bool {
    // CONTEXT: File::read/write cannot return EINTR yet, but a pipe wait must
    // return to the trap path when a signal wakes it so fatal signals can exit.
    current_has_unmasked_signal()
}

impl Drop for Pipe {
    fn drop(&mut self) {
        let (readers, writers) = {
            let mut ring_buffer = self.buffer.exclusive_access();
            let readers = if self.writable {
                ring_buffer.wake_all_readers()
            } else {
                VecDeque::new()
            };
            let writers = if self.readable {
                ring_buffer.wake_all_writers()
            } else {
                VecDeque::new()
            };
            (readers, writers)
        };
        wake_tasks(readers);
        wake_tasks(writers);
    }
}

impl File for Pipe {
    fn as_any(&self) -> &dyn core::any::Any {
        self
    }

    fn readable(&self) -> bool {
        self.readable
    }
    fn writable(&self) -> bool {
        self.writable
    }
    fn read(&self, mut buf: UserBuffer) -> usize {
        assert!(self.readable());
        let want_to_read = buf.len();
        if want_to_read == 0 {
            return 0;
        }
        loop {
            let mut ring_buffer = self.buffer.exclusive_access();
            let loop_read = ring_buffer.available_read().min(want_to_read);
            if loop_read == 0 {
                if ring_buffer.all_write_ends_closed() {
                    return 0;
                }
                if pipe_wait_interrupted() {
                    return 0;
                }
                let task_cx_ptr = ring_buffer.sleep_reader();
                drop(ring_buffer);
                schedule(task_cx_ptr);
                continue;
            }
            let mut copied = 0usize;
            for buffer in buf.buffers.iter_mut() {
                if copied == loop_read {
                    break;
                }
                let len = buffer.len().min(loop_read - copied);
                for byte in &mut buffer[..len] {
                    *byte = ring_buffer.read_byte();
                }
                copied += len;
            }
            let writer = ring_buffer.wake_writer();
            drop(ring_buffer);
            wake_task(writer);
            return copied;
        }
    }
    fn write(&self, buf: UserBuffer) -> usize {
        assert!(self.writable());
        let want_to_write = buf.len();
        if want_to_write == 0 {
            return 0;
        }
        let mut already_write = 0usize;
        loop {
            let mut ring_buffer = self.buffer.exclusive_access();
            if ring_buffer.all_read_ends_closed() {
                // CONTEXT: sys_write/sys_writev translate the initial
                // no-reader case into SIGPIPE/EPIPE. If readers disappear
                // after a partial write, Linux can report the partial count.
                return already_write;
            }
            let loop_write = ring_buffer.available_write();
            if loop_write == 0 {
                if self.status_flags.get().contains(OpenFlags::NONBLOCK) {
                    return already_write;
                }
                if pipe_wait_interrupted() {
                    return already_write;
                }
                let task_cx_ptr = ring_buffer.sleep_writer();
                drop(ring_buffer);
                schedule(task_cx_ptr);
                continue;
            }
            let write_len = loop_write.min(want_to_write - already_write);
            let mut skipped = 0usize;
            let mut written = 0usize;
            for buffer in buf.buffers.iter() {
                if skipped + buffer.len() <= already_write {
                    skipped += buffer.len();
                    continue;
                }
                let offset = already_write.saturating_sub(skipped);
                for &byte in &buffer[offset..] {
                    if written == write_len {
                        break;
                    }
                    ring_buffer.write_byte(byte);
                    written += 1;
                }
                if written == write_len {
                    break;
                }
                skipped += buffer.len();
            }
            already_write += written;
            let reader = ring_buffer.wake_reader();
            drop(ring_buffer);
            wake_task(reader);
            if already_write == want_to_write {
                return want_to_write;
            }
        }
    }
    fn stat(&self) -> FsResult<FileStat> {
        Ok(FileStat::with_mode(S_IFIFO | 0o600))
    }
    fn status_flags(&self) -> OpenFlags {
        self.status_flags.get()
    }
    fn set_status_flags(&self, flags: OpenFlags) {
        self.status_flags.set(flags);
    }
    fn pipe_capacity(&self) -> Option<usize> {
        Some(self.buffer.exclusive_access().capacity())
    }
    fn set_pipe_capacity(&self, capacity: usize) -> FsResult<usize> {
        self.buffer.exclusive_access().resize(capacity)
    }
    fn pipe_occupied(&self) -> Option<usize> {
        Some(self.buffer.exclusive_access().available_read())
    }
    fn pipe_readers_closed(&self) -> bool {
        self.writable && self.buffer.exclusive_access().all_read_ends_closed()
    }
    fn poll(&self, events: PollEvents) -> PollEvents {
        let ring_buffer = self.buffer.exclusive_access();
        let mut ready = PollEvents::empty();
        if self.readable {
            let has_data = ring_buffer.available_read() > 0;
            let hangup = ring_buffer.all_write_ends_closed();
            if events.intersects(PollEvents::POLLIN | PollEvents::POLLPRI) && (has_data || hangup) {
                ready |= PollEvents::POLLIN;
            }
            if hangup {
                ready |= PollEvents::POLLHUP;
            }
        }
        if self.writable {
            // CONTEXT: Linux reports pipe POLLOUT when a PIPE_BUF-sized write
            // can proceed without blocking; one free byte is not enough.
            let can_write = ring_buffer.available_write() >= PAGE_SIZE;
            let peer_closed = ring_buffer.all_read_ends_closed();
            if events.contains(PollEvents::POLLOUT) && (can_write || peer_closed) {
                ready |= PollEvents::POLLOUT;
            }
            if peer_closed {
                ready |= PollEvents::POLLERR;
            }
        }
        ready
    }
}
