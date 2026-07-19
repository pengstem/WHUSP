use super::{
    TaskControlBlock, block_current_task_no_schedule_unless_unmasked_signal,
    current_has_deliverable_signal, current_process, current_task, current_user_token, schedule,
    wakeup_front_task, wakeup_task,
};
use crate::mm::FutexSharedKey;
use crate::perf;
use crate::sync::SpinNoIrqLock;
use crate::syscall::errno::{SysError, SysResult};
use crate::syscall::time::{
    ClockBackend, current_clock_nanos, relative_timeout_deadline_ms,
    relative_timeout_deadline_ms_from_nanos, timespec_to_nanos, validate_timespec,
};
use crate::syscall::uapi::LinuxTimeSpec;
use crate::syscall::user_ptr::{
    read_user_value, read_user_value_with_mmap_fault, write_user_value,
    write_user_value_with_mmap_fault,
};
use crate::timer::{add_timer, get_time_ms};
use alloc::collections::{BTreeMap, VecDeque};
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::mem::size_of;
use core::sync::atomic::{AtomicU8, AtomicUsize, Ordering, fence};
use lazy_static::*;

const FUTEX_WAIT: u32 = 0;
const FUTEX_WAKE: u32 = 1;
const FUTEX_REQUEUE: u32 = 3;
const FUTEX_CMP_REQUEUE: u32 = 4;
const FUTEX_LOCK_PI: u32 = 6;
const FUTEX_UNLOCK_PI: u32 = 7;
const FUTEX_TRYLOCK_PI: u32 = 8;
const FUTEX_WAIT_BITSET: u32 = 9;
const FUTEX_WAKE_BITSET: u32 = 10;
const FUTEX_CMD_MASK: u32 = 0x7f;
const FUTEX_PRIVATE_FLAG: u32 = 0x80;
const FUTEX_CLOCK_REALTIME: u32 = 0x100;
const FUTEX_BITSET_MATCH_ANY: u32 = u32::MAX;
const FUTEX_WAITERS: u32 = 0x8000_0000;
const FUTEX_OWNER_DIED: u32 = 0x4000_0000;
const FUTEX_TID_MASK: u32 = !(FUTEX_WAITERS | FUTEX_OWNER_DIED);
// Bound robust-list teardown so a corrupted userspace list cannot trap the
// exiting task forever while it still owns cleanup of clear-child-tid/fd state.
const ROBUST_LIST_LIMIT: usize = 2048;
// Keep the bucket count a power of two: bucket_index() masks with
// FUTEX_BUCKET_COUNT - 1 instead of taking a modulo on every wait/wake path.
const FUTEX_BUCKET_COUNT: usize = 64;
const FUTEX_WAITER_QUEUED: u8 = 0;
const FUTEX_WAITER_WOKEN: u8 = 1;
const FUTEX_WAITER_CANCELLED: u8 = 2;

#[repr(C)]
#[derive(Clone, Copy)]
struct LinuxRobustListHead {
    list_next: usize,
    futex_offset: isize,
    list_op_pending: usize,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct LinuxRobustList {
    next: usize,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum FutexKey {
    // FUTEX_PRIVATE_FLAG scopes the word to one Linux process id plus virtual
    // address. Shared futexes must use `MemorySet::futex_shared_key()` when a
    // backing object identity can be resolved.
    Private { process_id: usize, addr: usize },
    Shared(FutexSharedKey),
    SharedVirtual { addr: usize },
}

struct FutexWaiter {
    task: Arc<TaskControlBlock>,
    bitset: u32,
    bucket: AtomicUsize,
    state: AtomicU8,
}

struct FutexBucket {
    waiters: BTreeMap<FutexKey, VecDeque<Arc<FutexWaiter>>>,
    waiter_count: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FutexWaitCleanup {
    Woken,
    StillQueued,
    AlreadyUnqueued,
}

/// Sharded futex wait queues keyed by Linux futex identity.
struct FutexManager {
    buckets: Vec<SpinNoIrqLock<FutexBucket>>,
    queue_count: AtomicUsize,
    waiter_count: AtomicUsize,
}

impl FutexBucket {
    fn new() -> Self {
        Self {
            waiters: BTreeMap::new(),
            waiter_count: 0,
        }
    }
}

impl FutexManager {
    fn new() -> Self {
        let mut buckets = Vec::new();
        while buckets.len() < FUTEX_BUCKET_COUNT {
            buckets.push(SpinNoIrqLock::new(FutexBucket::new()));
        }
        Self {
            buckets,
            queue_count: AtomicUsize::new(0),
            waiter_count: AtomicUsize::new(0),
        }
    }

    fn bucket_index(key: FutexKey) -> usize {
        let hash = match key {
            FutexKey::Private { process_id, addr } => (addr >> 2) ^ process_id,
            FutexKey::Shared(FutexSharedKey::File { id, offset }) => {
                (offset >> 2) ^ id.mount_id.0.rotate_left(5) ^ (id.ino as usize)
            }
            FutexKey::Shared(FutexSharedKey::VfsNode { node, offset }) => {
                (offset >> 2) ^ node.mount_id.0.rotate_left(5) ^ (node.ino as usize)
            }
            FutexKey::Shared(FutexSharedKey::FileObject { object, offset }) => {
                (offset >> 2) ^ object.rotate_left(11)
            }
            FutexKey::Shared(FutexSharedKey::Shm { shmid, offset }) => {
                (offset >> 2) ^ shmid.rotate_left(7)
            }
            FutexKey::Shared(FutexSharedKey::AnonymousPage { ppn, offset }) => {
                (offset >> 2) ^ ppn.rotate_left(3)
            }
            FutexKey::SharedVirtual { addr } => addr >> 2,
        };
        hash & (FUTEX_BUCKET_COUNT - 1)
    }

    fn private_process_id(key: FutexKey) -> Option<usize> {
        match key {
            FutexKey::Private { process_id, .. } => Some(process_id),
            FutexKey::Shared(_) | FutexKey::SharedVirtual { .. } => None,
        }
    }

    fn lock_bucket(&self, key: FutexKey) -> crate::sync::SpinNoIrqLockGuard<'_, FutexBucket> {
        self.buckets[Self::bucket_index(key)].lock()
    }

    fn remove_waiter_for_task(&self, waiter: &Arc<FutexWaiter>) -> FutexWaitCleanup {
        loop {
            match waiter.state.load(Ordering::Acquire) {
                FUTEX_WAITER_WOKEN => {
                    perf::record_futex_cleanup(true, false, 0, 0);
                    return FutexWaitCleanup::Woken;
                }
                FUTEX_WAITER_CANCELLED => {
                    perf::record_futex_cleanup(false, true, 0, 0);
                    return FutexWaitCleanup::AlreadyUnqueued;
                }
                FUTEX_WAITER_QUEUED => {}
                _ => unreachable!("invalid futex waiter state"),
            }

            let bucket_index = waiter.bucket.load(Ordering::Acquire);
            let mut bucket = self.buckets[bucket_index].lock();
            if waiter.bucket.load(Ordering::Acquire) != bucket_index {
                continue;
            }
            let old_queue_count = bucket.waiters.len();
            let mut removed = false;
            bucket.waiters.retain(|_, queue| {
                let old_len = queue.len();
                queue.retain(|queued| !Arc::ptr_eq(queued, waiter));
                removed |= queue.len() != old_len;
                !queue.is_empty()
            });
            if removed {
                bucket.waiter_count = bucket.waiter_count.saturating_sub(1);
                self.waiter_count.fetch_sub(1, Ordering::Relaxed);
                self.queue_count.fetch_sub(
                    old_queue_count.saturating_sub(bucket.waiters.len()),
                    Ordering::Relaxed,
                );
                waiter
                    .state
                    .store(FUTEX_WAITER_CANCELLED, Ordering::Release);
                self.record_state_for_bucket(&bucket);
                perf::record_futex_cleanup(true, false, 0, 0);
                return FutexWaitCleanup::StillQueued;
            }
            drop(bucket);

            if waiter.state.load(Ordering::Acquire) == FUTEX_WAITER_QUEUED {
                // Requeue inserts into the target bucket before publishing its
                // index. A cleanup that followed the old index retries here.
                continue;
            }
        }
    }

    /// Blocks the current task and enqueues it on `key`.
    ///
    /// The returned task context pointer must be passed to `schedule()` after
    /// releasing the bucket lock; scheduling while holding it can deadlock a
    /// wake or requeue path.
    fn block_current_on(
        &self,
        bucket: &mut FutexBucket,
        key: FutexKey,
        bitset: u32,
    ) -> Option<(*mut super::TaskContext, Arc<FutexWaiter>)> {
        let bucket_index = Self::bucket_index(key);
        let (task, task_cx_ptr) = block_current_task_no_schedule_unless_unmasked_signal()?;
        let waiter = Arc::new(FutexWaiter {
            task,
            bitset,
            bucket: AtomicUsize::new(bucket_index),
            state: AtomicU8::new(FUTEX_WAITER_QUEUED),
        });
        let queue = bucket.waiters.entry(key).or_default();
        let created_queue = queue.is_empty();
        queue.push_back(Arc::clone(&waiter));
        bucket.waiter_count += 1;
        if created_queue {
            self.queue_count.fetch_add(1, Ordering::Relaxed);
        }
        self.waiter_count.fetch_add(1, Ordering::Relaxed);
        self.record_state_for_bucket(bucket);
        Some((task_cx_ptr, waiter))
    }

    fn wake(&self, key: FutexKey, limit: usize, bitset: u32) -> Vec<Arc<TaskControlBlock>> {
        // Pair a userspace store-before-FUTEX_WAKE with the waiter's second
        // value load under this bucket. This must be explicit across the U/S
        // mode boundary, especially on weakly ordered RISC-V.
        fence(Ordering::SeqCst);
        let mut bucket = self.lock_bucket(key);
        let old_queue_count = bucket.waiters.len();
        let Some(queue) = bucket.waiters.get_mut(&key) else {
            perf::record_futex_wake(false, 0);
            return Vec::new();
        };
        let old_len = queue.len();
        let mut tasks = Vec::new();
        let mut kept = VecDeque::new();
        while let Some(waiter) = queue.pop_front() {
            if waiter.state.load(Ordering::Acquire) != FUTEX_WAITER_QUEUED {
                continue;
            }
            if waiter.bitset & bitset != 0 && tasks.len() < limit {
                waiter.state.store(FUTEX_WAITER_WOKEN, Ordering::Release);
                tasks.push(Arc::clone(&waiter.task));
            } else {
                kept.push_back(waiter);
            }
        }
        *queue = kept;
        let removed_count = old_len - queue.len();
        bucket.waiter_count = bucket.waiter_count.saturating_sub(removed_count);
        self.waiter_count
            .fetch_sub(removed_count, Ordering::Relaxed);
        self.remove_empty_queue(&mut bucket, key);
        self.queue_count.fetch_sub(
            old_queue_count.saturating_sub(bucket.waiters.len()),
            Ordering::Relaxed,
        );
        self.record_state_for_bucket(&bucket);
        perf::record_futex_wake(true, tasks.len());
        tasks
    }

    fn wake_one(&self, key: FutexKey) -> (Option<Arc<TaskControlBlock>>, bool) {
        fence(Ordering::SeqCst);
        let mut bucket = self.lock_bucket(key);
        let old_queue_count = bucket.waiters.len();
        let Some(queue) = bucket.waiters.get_mut(&key) else {
            perf::record_futex_wake(false, 0);
            return (None, false);
        };
        let old_len = queue.len();
        let mut task = None;
        while let Some(waiter) = queue.pop_front() {
            if waiter.state.load(Ordering::Acquire) != FUTEX_WAITER_QUEUED {
                continue;
            }
            waiter.state.store(FUTEX_WAITER_WOKEN, Ordering::Release);
            task = Some(Arc::clone(&waiter.task));
            break;
        }
        let removed_count = old_len - queue.len();
        let has_waiters = queue
            .iter()
            .any(|waiter| waiter.state.load(Ordering::Acquire) == FUTEX_WAITER_QUEUED);
        bucket.waiter_count = bucket.waiter_count.saturating_sub(removed_count);
        self.waiter_count
            .fetch_sub(removed_count, Ordering::Relaxed);
        self.remove_empty_queue(&mut bucket, key);
        self.queue_count.fetch_sub(
            old_queue_count.saturating_sub(bucket.waiters.len()),
            Ordering::Relaxed,
        );
        self.record_state_for_bucket(&bucket);
        perf::record_futex_wake(true, usize::from(task.is_some()));
        (task, has_waiters)
    }

    fn has_waiters(&self, key: FutexKey) -> bool {
        self.lock_bucket(key)
            .waiters
            .get(&key)
            .is_some_and(|queue| {
                queue
                    .iter()
                    .any(|waiter| waiter.state.load(Ordering::Acquire) == FUTEX_WAITER_QUEUED)
            })
    }

    fn requeue(
        &self,
        source: FutexKey,
        target: FutexKey,
        wake_limit: usize,
        requeue_limit: usize,
    ) -> (Vec<Arc<TaskControlBlock>>, usize) {
        fence(Ordering::SeqCst);
        let source_bucket_index = Self::bucket_index(source);
        let target_bucket_index = Self::bucket_index(target);
        let (tasks, moved) = if source_bucket_index == target_bucket_index {
            let mut bucket = self.buckets[source_bucket_index].lock();
            let (tasks, moved) =
                self.take_requeue_source(&mut bucket, source, wake_limit, requeue_limit);
            self.insert_requeued(&mut bucket, target, target_bucket_index, &moved);
            self.record_state_for_bucket(&bucket);
            (tasks, moved)
        } else if source_bucket_index < target_bucket_index {
            let mut source_bucket = self.buckets[source_bucket_index].lock();
            let mut target_bucket = self.buckets[target_bucket_index].lock();
            let (tasks, moved) =
                self.take_requeue_source(&mut source_bucket, source, wake_limit, requeue_limit);
            self.insert_requeued(&mut target_bucket, target, target_bucket_index, &moved);
            self.record_state_for_bucket(&source_bucket);
            self.record_state_for_bucket(&target_bucket);
            (tasks, moved)
        } else {
            let mut target_bucket = self.buckets[target_bucket_index].lock();
            let mut source_bucket = self.buckets[source_bucket_index].lock();
            let (tasks, moved) =
                self.take_requeue_source(&mut source_bucket, source, wake_limit, requeue_limit);
            self.insert_requeued(&mut target_bucket, target, target_bucket_index, &moved);
            self.record_state_for_bucket(&source_bucket);
            self.record_state_for_bucket(&target_bucket);
            (tasks, moved)
        };
        let moved_len = moved.len();
        perf::record_futex_wake(true, tasks.len());
        (tasks, moved_len)
    }

    fn take_requeue_source(
        &self,
        bucket: &mut FutexBucket,
        source: FutexKey,
        wake_limit: usize,
        requeue_limit: usize,
    ) -> (Vec<Arc<TaskControlBlock>>, VecDeque<Arc<FutexWaiter>>) {
        let old_queue_count = bucket.waiters.len();
        let Some(queue) = bucket.waiters.get_mut(&source) else {
            return (Vec::new(), VecDeque::new());
        };
        let old_len = queue.len();
        let mut tasks = Vec::new();
        let mut moved = VecDeque::new();
        let mut kept = VecDeque::new();
        while let Some(waiter) = queue.pop_front() {
            if waiter.state.load(Ordering::Acquire) != FUTEX_WAITER_QUEUED {
                continue;
            }
            if tasks.len() < wake_limit {
                waiter.state.store(FUTEX_WAITER_WOKEN, Ordering::Release);
                tasks.push(Arc::clone(&waiter.task));
            } else if moved.len() < requeue_limit {
                moved.push_back(waiter);
            } else {
                kept.push_back(waiter);
            }
        }
        *queue = kept;
        let removed_count = old_len - queue.len();
        bucket.waiter_count = bucket.waiter_count.saturating_sub(removed_count);
        let permanently_removed = removed_count.saturating_sub(moved.len());
        self.waiter_count
            .fetch_sub(permanently_removed, Ordering::Relaxed);
        self.remove_empty_queue(bucket, source);
        self.queue_count.fetch_sub(
            old_queue_count.saturating_sub(bucket.waiters.len()),
            Ordering::Relaxed,
        );
        (tasks, moved)
    }

    fn insert_requeued(
        &self,
        bucket: &mut FutexBucket,
        target: FutexKey,
        target_bucket_index: usize,
        moved: &VecDeque<Arc<FutexWaiter>>,
    ) {
        if moved.is_empty() {
            return;
        }
        let queue = bucket.waiters.entry(target).or_default();
        let created_queue = queue.is_empty();
        for waiter in moved {
            queue.push_back(Arc::clone(waiter));
            // Publish the target only after the waiter is visible there. A
            // cleanup following the old index will miss and retry.
            waiter.bucket.store(target_bucket_index, Ordering::Release);
        }
        bucket.waiter_count += moved.len();
        if created_queue {
            self.queue_count.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn remove_process(&self, process_id: usize) {
        for bucket_lock in &self.buckets {
            let mut bucket = bucket_lock.lock();
            let old_queue_count = bucket.waiters.len();
            let old_waiter_count = bucket.waiter_count;
            let mut waiter_count = 0usize;
            bucket.waiters.retain(|key, queue| {
                if Self::private_process_id(*key) == Some(process_id) {
                    for waiter in queue.iter() {
                        waiter
                            .state
                            .store(FUTEX_WAITER_CANCELLED, Ordering::Release);
                    }
                    return false;
                }
                queue.retain(|waiter| {
                    let keep = waiter.state.load(Ordering::Acquire) == FUTEX_WAITER_QUEUED
                        && waiter
                            .task
                            .process
                            .upgrade()
                            .is_some_and(|process| process.getpid() != process_id);
                    if !keep {
                        waiter
                            .state
                            .store(FUTEX_WAITER_CANCELLED, Ordering::Release);
                    }
                    keep
                });
                waiter_count += queue.len();
                !queue.is_empty()
            });
            bucket.waiter_count = waiter_count;
            self.waiter_count.fetch_sub(
                old_waiter_count.saturating_sub(waiter_count),
                Ordering::Relaxed,
            );
            self.queue_count.fetch_sub(
                old_queue_count.saturating_sub(bucket.waiters.len()),
                Ordering::Relaxed,
            );
            self.record_state_for_bucket(&bucket);
        }
    }

    fn remove_empty_queue(&self, bucket: &mut FutexBucket, key: FutexKey) {
        if matches!(bucket.waiters.get(&key), Some(queue) if queue.is_empty()) {
            bucket.waiters.remove(&key);
        }
    }

    fn record_state_for_bucket(&self, bucket: &FutexBucket) {
        perf::record_futex_manager_state(
            self.queue_count.load(Ordering::Relaxed),
            self.waiter_count.load(Ordering::Relaxed),
            bucket.waiters.len(),
            bucket.waiter_count,
        );
    }
}

lazy_static! {
    static ref FUTEX_MANAGER: FutexManager = FutexManager::new();
}

pub(super) fn init() {
    lazy_static::initialize(&FUTEX_MANAGER);
}

fn futex_key(addr: usize, private: bool) -> SysResult<FutexKey> {
    let process = current_process();
    if private {
        return Ok(FutexKey::Private {
            process_id: process.getpid(),
            addr,
        });
    }
    let inner = process.inner_exclusive_access();
    Ok(inner
        .memory_set
        .futex_shared_key(addr)
        .map(FutexKey::Shared)
        .unwrap_or(FutexKey::SharedVirtual { addr }))
}

fn futex_key_for_process(addr: usize, private: bool, process_id: usize) -> FutexKey {
    if private {
        FutexKey::Private { process_id, addr }
    } else {
        // UNFINISHED: Exit-time wake helpers only receive process id plus a
        // virtual address, so they cannot reconstruct file-backed or SHM-backed
        // process-shared futex keys. Normal sys_futex wait/wake paths still use
        // `futex_key()` to resolve backing-object identity when it is available.
        FutexKey::SharedVirtual { addr }
    }
}

fn validate_futex_addr(addr: usize) -> SysResult {
    // Linux futex operations address a naturally aligned 32-bit user word.
    // Reject unaligned addresses before any user access so EINVAL is not
    // hidden behind a later EFAULT from the copy helper.
    if addr % core::mem::size_of::<u32>() != 0 {
        return Err(SysError::EINVAL);
    }
    Ok(0)
}

fn read_futex_word(addr: usize) -> SysResult<u32> {
    validate_futex_addr(addr)?;
    read_user_value_with_mmap_fault(current_user_token(), addr as *const u32)
}

fn write_futex_word(addr: usize, value: u32) -> SysResult<()> {
    validate_futex_addr(addr)?;
    write_user_value_with_mmap_fault(current_user_token(), addr as *mut u32, &value)
}

fn read_futex_word_with_token(token: usize, addr: usize) -> SysResult<u32> {
    validate_futex_addr(addr)?;
    read_user_value(token, addr as *const u32)
}

fn write_futex_word_with_current_token_no_fault(
    token: usize,
    addr: usize,
    value: u32,
) -> SysResult<()> {
    validate_futex_addr(addr)?;
    write_user_value(token, addr as *mut u32, &value)
}

fn write_futex_word_with_token(token: usize, addr: usize, value: u32) -> SysResult<()> {
    validate_futex_addr(addr)?;
    write_user_value(token, addr as *mut u32, &value)
}

fn linux_tid_to_futex_word(tid: usize) -> SysResult<u32> {
    if tid > FUTEX_TID_MASK as usize {
        return Err(SysError::EINVAL);
    }
    Ok(tid as u32)
}

fn current_linux_tid_u32() -> SysResult<u32> {
    linux_tid_to_futex_word(
        current_task()
            .expect("futex syscall must run with a current task")
            .linux_tid(),
    )
}

fn futex_timeout_absolute(
    timeout: *const LinuxTimeSpec,
    backend: ClockBackend,
) -> SysResult<Option<usize>> {
    if timeout.is_null() {
        return Ok(None);
    }
    let request = validate_timespec(read_user_value(current_user_token(), timeout)?)?;
    let deadline_nanos = timespec_to_nanos(request)?;
    let now_nanos = current_clock_nanos(backend);
    if deadline_nanos <= now_nanos {
        return Ok(Some(get_time_ms()));
    }
    Ok(Some(relative_timeout_deadline_ms_from_nanos(
        deadline_nanos - now_nanos,
    )?))
}

fn futex_timeout_expired(timeout_ms: Option<usize>) -> bool {
    matches!(timeout_ms, Some(deadline_ms) if deadline_ms <= get_time_ms())
}

fn futex_wait(
    addr: usize,
    private: bool,
    expected: u32,
    timeout_ms: Option<usize>,
    bitset: u32,
) -> SysResult {
    let key = futex_key(addr, private)?;
    let task = current_task().expect("futex wait must run with a current task");
    let token = current_user_token();

    // Fault the word in before taking its IRQ-safe hash-bucket lock. The second
    // no-fault read under that lock is the compare-and-block point.
    if read_futex_word(addr)? != expected {
        return Err(SysError::EAGAIN);
    }
    if futex_timeout_expired(timeout_ms) {
        return Err(SysError::ETIMEDOUT);
    }
    if current_has_deliverable_signal() {
        return Err(SysError::EINTR);
    }

    let (task_cx_ptr, waiter) = {
        let mut bucket = FUTEX_MANAGER.lock_bucket(key);
        // Value check and enqueue are one critical section: a wake that runs
        // after the user word changes must either see the queued waiter or the
        // waiter must return EAGAIN before sleeping.
        if read_futex_word_with_token(token, addr)? != expected {
            return Err(SysError::EAGAIN);
        }
        if futex_timeout_expired(timeout_ms) {
            return Err(SysError::ETIMEDOUT);
        }
        FUTEX_MANAGER
            .block_current_on(&mut bucket, key, bitset)
            .ok_or(SysError::EINTR)?
    };

    if let Some(deadline_ms) = timeout_ms {
        add_timer(deadline_ms, Arc::clone(&task));
    }
    schedule(task_cx_ptr);

    let cleanup = FUTEX_MANAGER.remove_waiter_for_task(&waiter);
    if futex_timeout_expired(timeout_ms) {
        if cleanup == FutexWaitCleanup::StillQueued {
            return Err(SysError::ETIMEDOUT);
        }
        if current_has_deliverable_signal() {
            return Err(SysError::EINTR);
        }
        return Ok(0);
    }
    if current_has_deliverable_signal() {
        return Err(SysError::EINTR);
    }
    if cleanup == FutexWaitCleanup::StillQueued {
        return Err(SysError::EINTR);
    }
    Ok(0)
}

fn futex_wake(addr: usize, private: bool, limit: usize, bitset: u32) -> SysResult<usize> {
    let key = futex_key(addr, private)?;
    let tasks = FUTEX_MANAGER.wake(key, limit, bitset);
    Ok(wake_futex_tasks(tasks))
}

fn futex_wake_for_process(
    addr: usize,
    private: bool,
    process_id: usize,
    limit: usize,
    bitset: u32,
) -> usize {
    let key = futex_key_for_process(addr, private, process_id);
    let tasks = FUTEX_MANAGER.wake(key, limit, bitset);
    wake_futex_tasks(tasks)
}

fn wake_futex_tasks(tasks: Vec<Arc<TaskControlBlock>>) -> usize {
    let mut woken = 0;
    for task in tasks {
        // CONTEXT: Futex wakeups are synchronization handoffs. Under
        // hackbench-style load, placing the waiter at the tail can delay
        // pthread_join/condvar completion far beyond the timed RT workload.
        if wakeup_front_task(task) {
            woken += 1;
        }
    }
    woken
}

fn futex_waiters_word(owner_tid: u32, has_waiters: bool) -> u32 {
    if has_waiters {
        owner_tid | FUTEX_WAITERS
    } else {
        owner_tid
    }
}

fn clear_pi_waiters_bit_if_idle(addr: usize, key: FutexKey) -> SysResult {
    if FUTEX_MANAGER.has_waiters(key) {
        return Ok(0);
    }
    let word = read_futex_word(addr)?;
    if word & FUTEX_WAITERS != 0 {
        write_futex_word(addr, word & !FUTEX_WAITERS)?;
    }
    Ok(0)
}

fn futex_try_lock_pi(addr: usize) -> SysResult {
    let tid = current_linux_tid_u32()?;
    if try_acquire_pi_word(addr, tid)? {
        return Ok(0);
    }
    Err(SysError::EAGAIN)
}

fn try_acquire_pi_word(addr: usize, tid: u32) -> SysResult<bool> {
    let word = read_futex_word(addr)?;
    let owner_tid = word & FUTEX_TID_MASK;
    if owner_tid == 0 {
        if word & FUTEX_WAITERS != 0 {
            return Err(SysError::EINVAL);
        }
        write_futex_word(addr, tid)?;
        return Ok(true);
    }
    if owner_tid == tid {
        return Err(SysError::EDEADLK);
    }
    Ok(false)
}

fn futex_lock_pi(addr: usize, private: bool, timeout_ms: Option<usize>) -> SysResult {
    let key = futex_key(addr, private)?;
    let tid = current_linux_tid_u32()?;
    let task = current_task().expect("PI futex lock must run with a current task");
    let token = current_user_token();

    // Resolve lazy/COW mappings before entering the IRQ-safe futex bucket.
    let _ = read_futex_word(addr)?;

    let (task_cx_ptr, waiter) = {
        let mut bucket = FUTEX_MANAGER.lock_bucket(key);
        let word = read_futex_word_with_token(token, addr)?;
        let owner_tid = word & FUTEX_TID_MASK;
        if owner_tid == 0 {
            if word & FUTEX_WAITERS != 0 {
                return Err(SysError::EINVAL);
            }
            write_futex_word_with_current_token_no_fault(token, addr, tid)?;
            return Ok(0);
        }
        if owner_tid == tid {
            return Err(SysError::EDEADLK);
        }
        if futex_timeout_expired(timeout_ms) {
            return Err(SysError::ETIMEDOUT);
        }
        if word & FUTEX_WAITERS == 0 {
            write_futex_word_with_current_token_no_fault(token, addr, word | FUTEX_WAITERS)?;
        }
        FUTEX_MANAGER
            .block_current_on(&mut bucket, key, FUTEX_BITSET_MATCH_ANY)
            .ok_or(SysError::EINTR)?
    };

    if let Some(deadline_ms) = timeout_ms {
        add_timer(deadline_ms, Arc::clone(&task));
    }
    schedule(task_cx_ptr);

    let cleanup = FUTEX_MANAGER.remove_waiter_for_task(&waiter);
    if futex_timeout_expired(timeout_ms) {
        if cleanup == FutexWaitCleanup::StillQueued {
            clear_pi_waiters_bit_if_idle(addr, key)?;
            return Err(SysError::ETIMEDOUT);
        }
        if current_has_deliverable_signal() {
            return Err(SysError::EINTR);
        }
        return Ok(0);
    }
    if current_has_deliverable_signal() {
        clear_pi_waiters_bit_if_idle(addr, key)?;
        return Err(SysError::EINTR);
    }
    if cleanup == FutexWaitCleanup::StillQueued {
        clear_pi_waiters_bit_if_idle(addr, key)?;
        return Err(SysError::EINTR);
    }
    Ok(0)
}

fn futex_unlock_pi(addr: usize, private: bool) -> SysResult {
    let key = futex_key(addr, private)?;
    let tid = current_linux_tid_u32()?;
    let word = read_futex_word(addr)?;
    if word & FUTEX_TID_MASK != tid {
        return Err(SysError::EPERM);
    }

    let (next_task, has_more_waiters) = FUTEX_MANAGER.wake_one(key);
    if let Some(task) = next_task {
        let next_tid = linux_tid_to_futex_word(task.linux_tid())?;
        // UNFINISHED: This is PI-futex ownership handoff without scheduler
        // priority boosting. It preserves the Linux futex-word policy needed by
        // musl pthread PRIO_INHERIT mutexes, but it does not implement
        // transitive priority inheritance or priority-ordered waiter selection.
        write_futex_word(addr, futex_waiters_word(next_tid, has_more_waiters))?;
        let _ = wakeup_task(task);
    } else {
        write_futex_word(addr, 0)?;
    }
    Ok(0)
}

fn robust_futex_addr(entry: usize, futex_offset: isize) -> SysResult<usize> {
    if futex_offset >= 0 {
        entry
            .checked_add(futex_offset as usize)
            .ok_or(SysError::EFAULT)
    } else {
        entry
            .checked_sub(futex_offset.checked_neg().ok_or(SysError::EFAULT)? as usize)
            .ok_or(SysError::EFAULT)
    }
}

fn handle_robust_futex_death(
    token: usize,
    process_id: usize,
    entry: usize,
    futex_offset: isize,
    tid: u32,
) -> SysResult {
    let addr = robust_futex_addr(entry, futex_offset)?;
    let word = read_futex_word_with_token(token, addr)?;
    if word & FUTEX_TID_MASK != tid {
        return Ok(0);
    }

    write_futex_word_with_token(token, addr, (word & FUTEX_WAITERS) | FUTEX_OWNER_DIED)?;
    if word & FUTEX_WAITERS != 0 {
        // CONTEXT: Linux wakes a robust futex waiter by keying the futex word;
        // this teardown path wakes both the shared-virtual fallback key and the
        // process-private key used by common pthread robust mutex paths.
        let _ = futex_wake_for_process(addr, false, process_id, 1, FUTEX_BITSET_MATCH_ANY);
        let _ = futex_wake_for_process(addr, true, process_id, 1, FUTEX_BITSET_MATCH_ANY);
    }
    Ok(0)
}

fn exit_robust_list_inner(
    head_addr: usize,
    token: usize,
    process_id: usize,
    tid: u32,
) -> SysResult {
    let head: LinuxRobustListHead =
        read_user_value(token, head_addr as *const LinuxRobustListHead)?;
    let mut entry = head.list_next;
    let mut remaining = ROBUST_LIST_LIMIT;

    while entry != 0 && entry != head_addr {
        if remaining == 0 {
            return Err(SysError::ELOOP);
        }
        remaining -= 1;
        let next = read_user_value::<LinuxRobustList>(token, entry as *const LinuxRobustList)?.next;
        if entry != head.list_op_pending {
            handle_robust_futex_death(token, process_id, entry, head.futex_offset, tid)?;
        }
        entry = next;
    }

    if head.list_op_pending != 0 {
        handle_robust_futex_death(
            token,
            process_id,
            head.list_op_pending,
            head.futex_offset,
            tid,
        )?;
    }
    Ok(0)
}

pub(crate) fn exit_robust_list(task: &Arc<TaskControlBlock>, token: usize, process_id: usize) {
    let head_addr = task.robust_list_head();
    if head_addr == 0 {
        return;
    }
    let Ok(tid) = linux_tid_to_futex_word(task.linux_tid()) else {
        return;
    };
    let _ = exit_robust_list_inner(head_addr, token, process_id, tid);
}

pub(crate) fn clear_child_tid_and_wake(token: usize, process_id: usize, addr: usize) {
    if addr == 0 {
        return;
    }
    // Called during task exit while the dying task's address space is still
    // available; after the zero store, wake joiners on the futex word.
    if write_user_value(token, addr as *mut i32, &0).is_err() {
        return;
    }
    let _ = futex_wake_for_process(addr, false, process_id, 1, FUTEX_BITSET_MATCH_ANY);
    // CONTEXT: Linux specifies FUTEX_WAKE without FUTEX_PRIVATE_FLAG for
    // clear_child_tid. This kernel keeps private and shared futex wait queues
    // separate, so also wake the process-private key used by common libc paths.
    let _ = futex_wake_for_process(addr, true, process_id, 1, FUTEX_BITSET_MATCH_ANY);
}

fn futex_requeue(
    addr: usize,
    private: bool,
    wake_limit: usize,
    requeue_limit: usize,
    addr2: usize,
    count_requeued: bool,
) -> SysResult<usize> {
    validate_futex_addr(addr2)?;
    let source = futex_key(addr, private)?;
    let target = futex_key(addr2, private)?;
    if source == target {
        return Err(SysError::EINVAL);
    }
    let (tasks, moved) = FUTEX_MANAGER.requeue(source, target, wake_limit, requeue_limit);
    let woken = wake_futex_tasks(tasks);
    Ok(if count_requeued { moved + woken } else { woken })
}

pub(crate) fn remove_process_futex_waiters(process_id: usize) {
    FUTEX_MANAGER.remove_process(process_id);
}

pub(crate) fn sys_set_robust_list(head: usize, len: usize) -> SysResult {
    if len != size_of::<LinuxRobustListHead>() {
        return Err(SysError::EINVAL);
    }
    // Robust-list heads are Linux-thread state, not process state. Exit and
    // exec cleanup read this field from the owning TaskControlBlock.
    current_task()
        .expect("set_robust_list must run with a current task")
        .set_robust_list_head(head);
    Ok(0)
}

fn robust_list_query_task(pid: isize) -> SysResult<Arc<TaskControlBlock>> {
    if pid < 0 {
        return Err(SysError::ESRCH);
    }
    if pid == 0 {
        return current_task().ok_or(SysError::ESRCH);
    }

    let pid = pid as usize;
    // UNFINISHED: Linux get_robust_list(pid) may inspect another task when
    // ptrace-style permission checks allow it. This kernel currently exposes
    // robust-list heads only inside the caller's current thread group.
    let process = current_process();
    let process_inner = process.inner_exclusive_access();
    process_inner
        .tasks
        .iter()
        .filter_map(|task| task.as_ref())
        .find(|task| task.linux_tid() == pid)
        .map(Arc::clone)
        .ok_or(SysError::ESRCH)
}

pub(crate) fn sys_get_robust_list(
    pid: isize,
    head_ptr: *mut usize,
    len_ptr: *mut usize,
) -> SysResult {
    let task = robust_list_query_task(pid)?;
    let token = current_user_token();
    write_user_value(token, head_ptr, &task.robust_list_head())?;
    write_user_value(token, len_ptr, &size_of::<LinuxRobustListHead>())?;
    Ok(0)
}

fn futex_count(raw: usize) -> SysResult<usize> {
    if raw > i32::MAX as usize {
        return Err(SysError::EINVAL);
    }
    Ok(raw)
}

fn validate_futex_clock_option(command: u32, futex_op: u32) -> SysResult<()> {
    if futex_op & FUTEX_CLOCK_REALTIME == 0 {
        return Ok(());
    }
    match command {
        FUTEX_WAIT | FUTEX_WAIT_BITSET => Ok(()),
        // UNFINISHED: FUTEX_WAIT_REQUEUE_PI and FUTEX_LOCK_PI2 also accept
        // FUTEX_CLOCK_REALTIME on Linux. This kernel currently implements only
        // the older FUTEX_LOCK_PI/FUTEX_TRYLOCK_PI/FUTEX_UNLOCK_PI subset.
        _ => Err(SysError::ENOSYS),
    }
}

pub(crate) fn sys_futex(
    uaddr: *mut u32,
    futex_op: u32,
    val: u32,
    timeout: *const LinuxTimeSpec,
    uaddr2: *mut u32,
    val3: u32,
) -> SysResult {
    let addr = uaddr as usize;
    validate_futex_addr(addr)?;
    if futex_op & !(FUTEX_CMD_MASK | FUTEX_PRIVATE_FLAG | FUTEX_CLOCK_REALTIME) != 0 {
        return Err(SysError::EINVAL);
    }

    let command = futex_op & FUTEX_CMD_MASK;
    validate_futex_clock_option(command, futex_op)?;
    if matches!(command, FUTEX_WAIT_BITSET | FUTEX_WAKE_BITSET) && val3 == 0 {
        return Err(SysError::EINVAL);
    }
    let private = futex_op & FUTEX_PRIVATE_FLAG != 0;
    let clock_backend = if futex_op & FUTEX_CLOCK_REALTIME != 0 {
        ClockBackend::Wall
    } else {
        ClockBackend::Monotonic
    };

    match command {
        FUTEX_WAIT => futex_wait(
            addr,
            private,
            val,
            relative_timeout_deadline_ms(current_user_token(), timeout)?,
            FUTEX_BITSET_MATCH_ANY,
        ),
        FUTEX_WAIT_BITSET => futex_wait(
            addr,
            private,
            val,
            futex_timeout_absolute(timeout, clock_backend)?,
            val3,
        ),
        FUTEX_WAKE => Ok(futex_wake(
            addr,
            private,
            futex_count(val as usize)?,
            FUTEX_BITSET_MATCH_ANY,
        )? as isize),
        FUTEX_WAKE_BITSET => {
            Ok(futex_wake(addr, private, futex_count(val as usize)?, val3)? as isize)
        }
        FUTEX_LOCK_PI => futex_lock_pi(
            addr,
            private,
            futex_timeout_absolute(timeout, ClockBackend::Wall)?,
        ),
        FUTEX_TRYLOCK_PI => futex_try_lock_pi(addr),
        FUTEX_UNLOCK_PI => futex_unlock_pi(addr, private),
        FUTEX_REQUEUE | FUTEX_CMP_REQUEUE => {
            let addr2 = uaddr2 as usize;
            // CONTEXT: FUTEX_REQUEUE/CMP_REQUEUE use the fourth syscall
            // register as val2, but syscall dispatch has already cast that
            // register to this pointer-typed timeout parameter.
            let requeue_limit = futex_count(timeout as usize)?;
            if command == FUTEX_CMP_REQUEUE && read_futex_word(addr)? != val3 {
                return Err(SysError::EAGAIN);
            }
            futex_requeue(
                addr,
                private,
                futex_count(val as usize)?,
                requeue_limit,
                addr2,
                command == FUTEX_CMP_REQUEUE,
            )
            .map(|count| count as isize)
        }
        // UNFINISHED: FUTEX_WAKE_OP, futex_waitv, and requeue-PI are not
        // implemented. The libctest pthread paths currently need classic
        // wait/wake/requeue plus the minimal PI mutex subset above.
        _ => Err(SysError::ENOSYS),
    }
}
