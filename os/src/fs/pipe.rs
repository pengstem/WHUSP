use super::status_flags::StatusFlagsCell;
use super::{File, FileStat, FsError, FsResult, OpenFlags, PollEvents, S_IFIFO};
use crate::config::PAGE_SIZE;
use crate::fs::pipe_max_size;
use crate::mm::UserBuffer;
use crate::perf;
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

    pub(crate) fn read_with_status_flags(&self, mut buf: UserBuffer, _flags: OpenFlags) -> usize {
        assert!(self.readable());
        let want_to_read = buf.len();
        if want_to_read == 0 {
            return 0;
        }
        perf::record_pipe_read_call();
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
                perf::record_pipe_reader_sleep();
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
                copied += ring_buffer.read_into(&mut buffer[..len]);
            }
            perf::record_pipe_read_chunk_copy(copied);
            let writer = ring_buffer.wake_writer();
            drop(ring_buffer);
            wake_task(writer);
            return copied;
        }
    }

    pub(crate) fn write_with_status_flags(&self, buf: UserBuffer, flags: OpenFlags) -> usize {
        assert!(self.writable());
        let want_to_write = buf.len();
        if want_to_write == 0 {
            return 0;
        }
        perf::record_pipe_write_call();
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
                if flags.contains(OpenFlags::NONBLOCK) {
                    return already_write;
                }
                if pipe_wait_interrupted() {
                    return already_write;
                }
                perf::record_pipe_writer_sleep();
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
                let source = &buffer[offset..];
                let len = source.len().min(write_len - written);
                written += ring_buffer.write_from(&source[..len]);
                if written == write_len {
                    break;
                }
                skipped += buffer.len();
            }
            perf::record_pipe_write_chunk_copy(written);
            already_write += written;
            let reader = ring_buffer.wake_reader();
            drop(ring_buffer);
            wake_task(reader);
            if already_write == want_to_write {
                return want_to_write;
            }
        }
    }

    fn splice_pipe_to_pipe(&self, out: &Pipe, len: usize) -> FsResult<usize> {
        if !self.readable || !out.writable {
            return Err(FsError::InvalidInput);
        }
        if Arc::ptr_eq(&self.buffer, &out.buffer) {
            return Err(FsError::InvalidInput);
        }

        let (writer, reader, moved) = {
            let mut in_buffer = self.buffer.exclusive_access();
            let mut out_buffer = out.buffer.exclusive_access();
            let moved = in_buffer.transfer_to(&mut out_buffer, len);
            let writer = if moved > 0 {
                in_buffer.wake_writer()
            } else {
                None
            };
            let reader = if moved > 0 {
                out_buffer.wake_reader()
            } else {
                None
            };
            (writer, reader, moved)
        };
        wake_task(writer);
        wake_task(reader);
        Ok(moved)
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
    fn write_from(&mut self, src: &[u8]) -> usize {
        let len = src.len().min(self.available_write());
        if len == 0 {
            return 0;
        }
        let first_len = len.min(self.capacity() - self.tail);
        self.arr[self.tail..self.tail + first_len].copy_from_slice(&src[..first_len]);
        let second_len = len - first_len;
        if second_len > 0 {
            self.arr[..second_len].copy_from_slice(&src[first_len..len]);
        }
        self.tail = (self.tail + len) % self.capacity();
        self.status = if self.tail == self.head {
            RingBufferStatus::Full
        } else {
            RingBufferStatus::Normal
        };
        len
    }
    fn read_into(&mut self, dst: &mut [u8]) -> usize {
        let len = dst.len().min(self.available_read());
        if len == 0 {
            return 0;
        }
        let first_len = len.min(self.capacity() - self.head);
        dst[..first_len].copy_from_slice(&self.arr[self.head..self.head + first_len]);
        let second_len = len - first_len;
        if second_len > 0 {
            dst[first_len..len].copy_from_slice(&self.arr[..second_len]);
        }
        self.head = (self.head + len) % self.capacity();
        self.status = if self.head == self.tail {
            RingBufferStatus::Empty
        } else {
            RingBufferStatus::Normal
        };
        len
    }
    fn transfer_to(&mut self, out: &mut PipeRingBuffer, len: usize) -> usize {
        let mut remaining = len.min(self.available_read()).min(out.available_write());
        let mut moved = 0usize;
        while remaining > 0 {
            let read_len = self.contiguous_read_len();
            let write_len = out.contiguous_write_len();
            let chunk_len = remaining.min(read_len).min(write_len);
            if chunk_len == 0 {
                break;
            }
            out.arr[out.tail..out.tail + chunk_len]
                .copy_from_slice(&self.arr[self.head..self.head + chunk_len]);
            self.advance_head(chunk_len);
            out.advance_tail(chunk_len);
            moved += chunk_len;
            remaining -= chunk_len;
        }
        moved
    }
    fn contiguous_read_len(&self) -> usize {
        let available = self.available_read();
        if available == 0 {
            0
        } else if self.tail > self.head {
            self.tail - self.head
        } else {
            self.capacity() - self.head
        }
    }
    fn contiguous_write_len(&self) -> usize {
        let available = self.available_write();
        if available == 0 {
            0
        } else if self.tail >= self.head {
            (self.capacity() - self.tail).min(available)
        } else {
            (self.head - self.tail).min(available)
        }
    }
    fn advance_head(&mut self, len: usize) {
        self.head = (self.head + len) % self.capacity();
        self.status = if self.head == self.tail {
            RingBufferStatus::Empty
        } else {
            RingBufferStatus::Normal
        };
    }
    fn advance_tail(&mut self, len: usize) {
        self.tail = (self.tail + len) % self.capacity();
        self.status = if self.tail == self.head {
            RingBufferStatus::Full
        } else {
            RingBufferStatus::Normal
        };
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
    /// Blocks the current reader until bytes arrive or peer teardown changes.
    ///
    /// The caller must drop the ring-buffer lock before passing the returned
    /// context pointer to `schedule()`, otherwise writers cannot wake it.
    fn sleep_reader(&mut self) -> *mut crate::task::TaskContext {
        let (task, task_cx_ptr) = block_current_task_no_schedule();
        self.read_wait_queue.push_back(task);
        task_cx_ptr
    }

    /// Blocks the current writer until pipe capacity or peer teardown changes.
    ///
    /// The caller must drop the ring-buffer lock before passing the returned
    /// context pointer to `schedule()`, otherwise readers cannot wake it.
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
        // CONTEXT: Dropping the write end wakes readers so EOF becomes
        // observable; dropping the read end wakes writers so EPIPE/SIGPIPE
        // can be produced by the syscall layer. Wake after releasing the ring
        // lock because the scheduler may inspect the same pipe state.
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
    fn read(&self, buf: UserBuffer) -> usize {
        self.read_with_status_flags(buf, self.status_flags.get())
    }
    fn write(&self, buf: UserBuffer) -> usize {
        self.write_with_status_flags(buf, self.status_flags.get())
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
    fn splice_pipe_to_pipe(
        &self,
        out: &(dyn File + Send + Sync),
        len: usize,
    ) -> FsResult<Option<usize>> {
        let Some(out) = out.as_any().downcast_ref::<Pipe>() else {
            return Ok(None);
        };
        self.splice_pipe_to_pipe(out, len).map(Some)
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
    fn is_pipe(&self) -> bool {
        true
    }
}
