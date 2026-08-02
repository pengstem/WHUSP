use crate::fs::{File, SeekWhence};
use crate::sync::SpinNoIrqLock;
use crate::syscall::user_ptr::{read_user_value, write_user_value};
use crate::task::{
    FdTableEntry, TaskControlBlock, block_current_task_no_schedule, current_process,
    current_user_token, processes_snapshot, schedule, wakeup_task,
};
use crate::uapi::errno::{Errno, KResult};
use alloc::collections::VecDeque;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::convert::TryFrom;
use core::sync::atomic::{AtomicUsize, Ordering};
use lazy_static::lazy_static;

const F_RDLCK: i16 = 0;
const F_WRLCK: i16 = 1;
const F_UNLCK: i16 = 2;
const SEEK_SET: i16 = 0;
const SEEK_CUR: i16 = 1;
const SEEK_END: i16 = 2;
const LOCK_TO_EOF: i64 = i64::MAX;
const LOCK_SH: i32 = 1;
const LOCK_EX: i32 = 2;
const LOCK_NB: i32 = 4;
const LOCK_UN: i32 = 8;
const FLOCK_VALID_FLAGS: i32 = LOCK_SH | LOCK_EX | LOCK_NB | LOCK_UN;

#[repr(C)]
#[derive(Clone, Copy)]
pub(super) struct LinuxFlock {
    l_type: i16,
    l_whence: i16,
    l_start: i64,
    l_len: i64,
    l_pid: i32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
struct LockKey {
    dev: u64,
    ino: u64,
}

#[derive(Clone)]
enum RecordLockOwner {
    Process(usize),
    FileDescription(Arc<dyn File + Send + Sync>),
}

impl RecordLockOwner {
    fn process(pid: usize) -> Self {
        Self::Process(pid)
    }

    fn file_description(file: Arc<dyn File + Send + Sync>) -> Self {
        Self::FileDescription(file)
    }

    fn same_owner(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Process(left), Self::Process(right)) => left == right,
            (Self::FileDescription(left), Self::FileDescription(right)) => Arc::ptr_eq(left, right),
            _ => false,
        }
    }

    fn is_process(&self, pid: usize) -> bool {
        matches!(self, Self::Process(owner_pid) if *owner_pid == pid)
    }

    fn reported_pid(&self) -> i32 {
        match self {
            Self::Process(pid) => *pid as i32,
            // CONTEXT: Linux reports -1 in struct flock.l_pid for conflicts
            // held by open-file-description locks.
            Self::FileDescription(_) => -1,
        }
    }

    fn sort_key(&self) -> (u8, usize) {
        match self {
            Self::Process(pid) => (0, *pid),
            Self::FileDescription(file) => (1, Arc::as_ptr(file) as *const () as usize),
        }
    }
}

#[derive(Clone)]
struct PosixLock {
    key: LockKey,
    owner: RecordLockOwner,
    l_type: i16,
    start: i64,
    end: i64,
}

#[derive(Clone, Copy, Debug)]
struct ReleasedRange {
    key: LockKey,
    start: i64,
    end: i64,
}

struct WaitingLock {
    key: LockKey,
    owner: RecordLockOwner,
    l_type: i16,
    start: i64,
    end: i64,
    task: Arc<TaskControlBlock>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FlockMode {
    Shared,
    Exclusive,
}

struct FlockLock {
    key: LockKey,
    owner: Arc<dyn File + Send + Sync>,
    mode: FlockMode,
}

struct WaitingFlock {
    key: LockKey,
    owner: Arc<dyn File + Send + Sync>,
    task: Arc<TaskControlBlock>,
}

struct RecordLockTable {
    locks: Vec<PosixLock>,
    waiters: VecDeque<WaitingLock>,
}

struct FlockTable {
    locks: Vec<FlockLock>,
    waiters: VecDeque<WaitingFlock>,
}

lazy_static! {
    static ref RECORD_LOCK_TABLE: SpinNoIrqLock<RecordLockTable> =
        SpinNoIrqLock::new(RecordLockTable::new());
    static ref FLOCK_TABLE: SpinNoIrqLock<FlockTable> = SpinNoIrqLock::new(FlockTable::new());
}

const POSIX_LOCKS_ACTIVE: usize = 1 << 0;
const OFD_LOCKS_ACTIVE: usize = 1 << 1;
const FLOCKS_ACTIVE: usize = 1 << 2;
const RECORD_LOCKS_ACTIVE: usize = POSIX_LOCKS_ACTIVE | OFD_LOCKS_ACTIVE;

/// One load covers the overwhelmingly common case where no compatibility
/// lock class is active anywhere in the system.
static ACTIVE_LOCK_CLASSES: AtomicUsize = AtomicUsize::new(0);

fn publish_record_lock_activity(table: &RecordLockTable) {
    let mut record_classes = 0usize;
    for owner in table
        .locks
        .iter()
        .map(|lock| &lock.owner)
        .chain(table.waiters.iter().map(|waiter| &waiter.owner))
    {
        match owner {
            RecordLockOwner::Process(_) => record_classes |= POSIX_LOCKS_ACTIVE,
            RecordLockOwner::FileDescription(_) => record_classes |= OFD_LOCKS_ACTIVE,
        }
    }
    let _ = ACTIVE_LOCK_CLASSES.fetch_update(Ordering::Release, Ordering::Relaxed, |active| {
        Some((active & !RECORD_LOCKS_ACTIVE) | record_classes)
    });
}

fn publish_flock_activity(table: &FlockTable) {
    if table.locks.is_empty() && table.waiters.is_empty() {
        ACTIVE_LOCK_CLASSES.fetch_and(!FLOCKS_ACTIVE, Ordering::Release);
    } else {
        ACTIVE_LOCK_CLASSES.fetch_or(FLOCKS_ACTIVE, Ordering::Release);
    }
}

impl RecordLockTable {
    fn new() -> Self {
        Self {
            locks: Vec::new(),
            waiters: VecDeque::new(),
        }
    }

    fn set_lock(
        &mut self,
        key: LockKey,
        owner: RecordLockOwner,
        l_type: i16,
        start: i64,
        end: i64,
    ) -> KResult<Vec<Arc<TaskControlBlock>>> {
        if l_type != F_UNLCK
            && !self
                .find_conflicts(key, &owner, l_type, start, end)
                .is_empty()
        {
            return Err(Errno::EAGAIN);
        }

        let released = self.remove_owned_range(key, &owner, start, end);
        let wakeups = self.take_waiters_for_released(&released);
        if l_type != F_UNLCK {
            self.locks.push(PosixLock {
                key,
                owner,
                l_type,
                start,
                end,
            });
            self.merge_adjacent();
        }
        Ok(wakeups)
    }

    fn find_conflict(
        &self,
        key: LockKey,
        owner: &RecordLockOwner,
        requested_type: i16,
        start: i64,
        end: i64,
    ) -> Option<PosixLock> {
        self.find_conflicts(key, owner, requested_type, start, end)
            .into_iter()
            .min_by_key(|lock| (lock.start, lock.end))
    }

    fn find_conflicts(
        &self,
        key: LockKey,
        owner: &RecordLockOwner,
        requested_type: i16,
        start: i64,
        end: i64,
    ) -> Vec<PosixLock> {
        self.locks
            .iter()
            .filter(|lock| {
                lock.key == key
                    && !lock.owner.same_owner(owner)
                    && lock_conflicts(lock.l_type, requested_type)
                    && ranges_overlap(lock.start, lock.end, start, end)
            })
            .cloned()
            .collect()
    }

    fn remove_owned_range(
        &mut self,
        key: LockKey,
        owner: &RecordLockOwner,
        start: i64,
        end: i64,
    ) -> Vec<ReleasedRange> {
        let mut next = Vec::new();
        let mut released = Vec::new();
        for lock in self.locks.drain(..) {
            if lock.key != key
                || !lock.owner.same_owner(owner)
                || !ranges_overlap(lock.start, lock.end, start, end)
            {
                next.push(lock);
                continue;
            }

            released.push(ReleasedRange {
                key,
                start: lock.start.max(start),
                end: lock.end.min(end),
            });
            if lock.start < start {
                next.push(PosixLock {
                    key: lock.key,
                    owner: lock.owner.clone(),
                    l_type: lock.l_type,
                    start: lock.start,
                    end: start - 1,
                });
            }
            if end != LOCK_TO_EOF && end < lock.end {
                next.push(PosixLock {
                    key: lock.key,
                    owner: lock.owner.clone(),
                    l_type: lock.l_type,
                    start: end + 1,
                    end: lock.end,
                });
            }
        }
        self.locks = next;
        released
    }

    fn release_for_process_file(&mut self, key: LockKey, pid: usize) -> Vec<Arc<TaskControlBlock>> {
        let released = self.remove_owned_range(key, &RecordLockOwner::process(pid), 0, LOCK_TO_EOF);
        self.take_waiters_for_released(&released)
    }

    fn release_for_process(&mut self, pid: usize) -> Vec<Arc<TaskControlBlock>> {
        self.remove_waiters_for_pid(pid);
        let mut next = Vec::new();
        let mut released = Vec::new();
        for lock in self.locks.drain(..) {
            if lock.owner.is_process(pid) {
                released.push(ReleasedRange {
                    key: lock.key,
                    start: lock.start,
                    end: lock.end,
                });
            } else {
                next.push(lock);
            }
        }
        self.locks = next;
        self.take_waiters_for_released(&released)
    }

    fn release_for_file_description(
        &mut self,
        file: &Arc<dyn File + Send + Sync>,
    ) -> Vec<Arc<TaskControlBlock>> {
        self.remove_waiters_for_file_description(file);
        let mut next = Vec::new();
        let mut released = Vec::new();
        for lock in self.locks.drain(..) {
            let owned = match &lock.owner {
                RecordLockOwner::FileDescription(owner) => Arc::ptr_eq(owner, file),
                RecordLockOwner::Process(_) => false,
            };
            if owned {
                released.push(ReleasedRange {
                    key: lock.key,
                    start: lock.start,
                    end: lock.end,
                });
            } else {
                next.push(lock);
            }
        }
        self.locks = next;
        self.take_waiters_for_released(&released)
    }

    fn has_file_description_state(&self, file: &Arc<dyn File + Send + Sync>) -> bool {
        self.locks.iter().any(|lock| {
            matches!(
                &lock.owner,
                RecordLockOwner::FileDescription(owner) if Arc::ptr_eq(owner, file)
            )
        }) || self.waiters.iter().any(|waiter| {
            matches!(
                &waiter.owner,
                RecordLockOwner::FileDescription(owner) if Arc::ptr_eq(owner, file)
            )
        })
    }

    fn has_process_locks(&self, pid: usize) -> bool {
        self.locks.iter().any(|lock| lock.owner.is_process(pid))
    }

    fn enqueue_waiter(
        &mut self,
        key: LockKey,
        owner: RecordLockOwner,
        l_type: i16,
        start: i64,
        end: i64,
        task: Arc<TaskControlBlock>,
    ) {
        self.waiters.push_back(WaitingLock {
            key,
            owner,
            l_type,
            start,
            end,
            task,
        });
    }

    fn would_deadlock(&self, owner: &RecordLockOwner, conflicts: &[PosixLock]) -> bool {
        conflicts.iter().any(|conflict| {
            self.waiters.iter().any(|waiter| {
                waiter.owner.same_owner(&conflict.owner)
                    && self.locks.iter().any(|owned| {
                        owned.owner.same_owner(owner)
                            && owned.key == waiter.key
                            && lock_conflicts(owned.l_type, waiter.l_type)
                            && ranges_overlap(owned.start, owned.end, waiter.start, waiter.end)
                    })
            })
        })
    }

    fn take_waiters_for_released(
        &mut self,
        released: &[ReleasedRange],
    ) -> Vec<Arc<TaskControlBlock>> {
        if released.is_empty() {
            return Vec::new();
        }

        let mut next = VecDeque::new();
        let mut wakeups = Vec::new();
        while let Some(waiter) = self.waiters.pop_front() {
            if released.iter().any(|range| {
                waiter.key == range.key
                    && ranges_overlap(waiter.start, waiter.end, range.start, range.end)
            }) {
                wakeups.push(waiter.task);
            } else {
                next.push_back(waiter);
            }
        }
        self.waiters = next;
        wakeups
    }

    fn remove_waiters_for_pid(&mut self, pid: usize) {
        self.waiters.retain(|waiter| !waiter.owner.is_process(pid));
    }

    fn remove_waiters_for_file_description(&mut self, file: &Arc<dyn File + Send + Sync>) {
        self.waiters.retain(|waiter| match &waiter.owner {
            RecordLockOwner::FileDescription(owner) => !Arc::ptr_eq(owner, file),
            RecordLockOwner::Process(_) => true,
        });
    }

    fn merge_adjacent(&mut self) {
        self.locks.sort_by_key(|lock| {
            (
                lock.key,
                lock.owner.sort_key(),
                lock.l_type,
                lock.start,
                lock.end,
            )
        });
        let mut merged: Vec<PosixLock> = Vec::new();
        for lock in self.locks.drain(..) {
            if let Some(last) = merged.last_mut()
                && last.key == lock.key
                && last.owner.same_owner(&lock.owner)
                && last.l_type == lock.l_type
                && (last.end == LOCK_TO_EOF || lock.start <= last.end.saturating_add(1))
            {
                last.end = last.end.max(lock.end);
                continue;
            }
            merged.push(lock);
        }
        self.locks = merged;
    }
}

impl FlockTable {
    fn new() -> Self {
        Self {
            locks: Vec::new(),
            waiters: VecDeque::new(),
        }
    }

    fn set_lock(
        &mut self,
        key: LockKey,
        owner: Arc<dyn File + Send + Sync>,
        mode: FlockMode,
    ) -> KResult<Vec<Arc<TaskControlBlock>>> {
        if self.has_conflict(key, &owner, mode) {
            return Err(Errno::EAGAIN);
        }
        self.remove_owner_lock(key, &owner);
        self.locks.push(FlockLock { key, owner, mode });
        Ok(self.take_waiters_for_key(key))
    }

    fn unlock(
        &mut self,
        key: LockKey,
        owner: &Arc<dyn File + Send + Sync>,
    ) -> Vec<Arc<TaskControlBlock>> {
        self.remove_owner_lock(key, owner);
        self.take_waiters_for_key(key)
    }

    fn release_owner(&mut self, owner: &Arc<dyn File + Send + Sync>) -> Vec<Arc<TaskControlBlock>> {
        self.remove_waiters_for_owner(owner);
        let mut released_keys = Vec::new();
        self.locks.retain(|lock| {
            if Arc::ptr_eq(&lock.owner, owner) {
                if !released_keys.contains(&lock.key) {
                    released_keys.push(lock.key);
                }
                false
            } else {
                true
            }
        });
        self.take_waiters_for_keys(&released_keys)
    }

    fn has_owner_state(&self, owner: &Arc<dyn File + Send + Sync>) -> bool {
        self.locks
            .iter()
            .any(|lock| Arc::ptr_eq(&lock.owner, owner))
            || self
                .waiters
                .iter()
                .any(|waiter| Arc::ptr_eq(&waiter.owner, owner))
    }

    fn has_conflict(
        &self,
        key: LockKey,
        owner: &Arc<dyn File + Send + Sync>,
        mode: FlockMode,
    ) -> bool {
        self.locks.iter().any(|lock| {
            lock.key == key
                && !Arc::ptr_eq(&lock.owner, owner)
                && flock_modes_conflict(lock.mode, mode)
        })
    }

    fn enqueue_waiter(
        &mut self,
        key: LockKey,
        owner: Arc<dyn File + Send + Sync>,
        task: Arc<TaskControlBlock>,
    ) {
        self.waiters.push_back(WaitingFlock { key, owner, task });
    }

    fn remove_owner_lock(&mut self, key: LockKey, owner: &Arc<dyn File + Send + Sync>) {
        self.locks
            .retain(|lock| lock.key != key || !Arc::ptr_eq(&lock.owner, owner));
    }

    fn remove_waiters_for_owner(&mut self, owner: &Arc<dyn File + Send + Sync>) {
        self.waiters
            .retain(|waiter| !Arc::ptr_eq(&waiter.owner, owner));
    }

    fn take_waiters_for_key(&mut self, key: LockKey) -> Vec<Arc<TaskControlBlock>> {
        self.take_waiters_for_keys(&[key])
    }

    fn take_waiters_for_keys(&mut self, keys: &[LockKey]) -> Vec<Arc<TaskControlBlock>> {
        if keys.is_empty() {
            return Vec::new();
        }
        let mut next = VecDeque::new();
        let mut wakeups = Vec::new();
        while let Some(waiter) = self.waiters.pop_front() {
            if keys.contains(&waiter.key) {
                wakeups.push(waiter.task);
            } else {
                next.push_back(waiter);
            }
        }
        self.waiters = next;
        wakeups
    }
}

fn valid_getlk_type(l_type: i16) -> bool {
    matches!(l_type, F_RDLCK | F_WRLCK)
}

fn valid_setlk_type(l_type: i16) -> bool {
    matches!(l_type, F_RDLCK | F_WRLCK | F_UNLCK)
}

fn valid_flock_whence(l_whence: i16) -> bool {
    matches!(l_whence, SEEK_SET | SEEK_CUR | SEEK_END)
}

fn lock_conflicts(existing_type: i16, requested_type: i16) -> bool {
    existing_type == F_WRLCK || requested_type == F_WRLCK
}

fn flock_modes_conflict(existing_mode: FlockMode, requested_mode: FlockMode) -> bool {
    existing_mode == FlockMode::Exclusive || requested_mode == FlockMode::Exclusive
}

fn ranges_overlap(first_start: i64, first_end: i64, second_start: i64, second_end: i64) -> bool {
    first_start <= second_end && second_start <= first_end
}

fn lock_key(file: &Arc<dyn File + Send + Sync>) -> KResult<LockKey> {
    if let Some(node) = file.vfs_node_id() {
        return Ok(LockKey {
            dev: node.mount_id.0 as u64,
            ino: node.ino as u64,
        });
    }
    let stat = file.stat()?;
    Ok(LockKey {
        dev: stat.dev,
        ino: stat.ino,
    })
}

fn flock_range(file: &Arc<dyn File + Send + Sync>, flock: LinuxFlock) -> KResult<(i64, i64)> {
    if !valid_flock_whence(flock.l_whence) {
        return Err(Errno::EINVAL);
    }
    let base = match flock.l_whence {
        SEEK_SET => 0,
        SEEK_CUR => i64::try_from(file.seek(0, SeekWhence::Current)?).map_err(|_| Errno::EINVAL)?,
        SEEK_END => i64::try_from(file.stat()?.size).map_err(|_| Errno::EINVAL)?,
        _ => unreachable!(),
    };
    let mut start = base.checked_add(flock.l_start).ok_or(Errno::EINVAL)?;
    if start < 0 {
        return Err(Errno::EINVAL);
    }

    let end = if flock.l_len > 0 {
        let len_last = flock.l_len.checked_sub(1).ok_or(Errno::EINVAL)?;
        start.checked_add(len_last).ok_or(Errno::EINVAL)?
    } else if flock.l_len < 0 {
        let end = start.checked_sub(1).ok_or(Errno::EINVAL)?;
        start = start.checked_add(flock.l_len).ok_or(Errno::EINVAL)?;
        if start < 0 {
            return Err(Errno::EINVAL);
        }
        end
    } else {
        LOCK_TO_EOF
    };

    Ok((start, end))
}

fn flock_len(start: i64, end: i64) -> i64 {
    if end == LOCK_TO_EOF {
        0
    } else {
        end - start + 1
    }
}

fn check_lock_access(file: &Arc<dyn File + Send + Sync>, l_type: i16) -> KResult<()> {
    match l_type {
        F_RDLCK if !file.readable() => Err(Errno::EBADF),
        F_WRLCK if !file.writable() => Err(Errno::EBADF),
        _ => Ok(()),
    }
}

fn wake_waiters(waiters: Vec<Arc<TaskControlBlock>>) {
    for task in waiters {
        let _ = wakeup_task(task);
    }
}

fn parse_flock_operation(operation: i32) -> KResult<(Option<FlockMode>, bool)> {
    if operation & !FLOCK_VALID_FLAGS != 0 {
        return Err(Errno::EINVAL);
    }
    let mode_bits = operation & (LOCK_SH | LOCK_EX | LOCK_UN);
    let mode = match mode_bits {
        LOCK_SH => Some(FlockMode::Shared),
        LOCK_EX => Some(FlockMode::Exclusive),
        LOCK_UN => None,
        _ => return Err(Errno::EINVAL),
    };
    Ok((mode, operation & LOCK_NB != 0))
}

fn file_description_still_referenced(file: &Arc<dyn File + Send + Sync>) -> bool {
    processes_snapshot()
        .into_iter()
        .any(|process| process.references_file_description(file))
}

pub(super) fn flock_operation(entry: FdTableEntry, operation: i32) -> KResult {
    let owner = entry.file();
    let (mode, nonblocking) = parse_flock_operation(operation)?;
    let key = lock_key(&owner)?;
    if let Some(mode) = mode {
        // UNFINISHED: blocking flock waits are not signal-interruptible yet;
        // Linux can return EINTR when an incompatible lock wait is interrupted.
        loop {
            let mut table = FLOCK_TABLE.lock();
            match table.set_lock(key, Arc::clone(&owner), mode) {
                Ok(waiters) => {
                    publish_flock_activity(&table);
                    drop(table);
                    wake_waiters(waiters);
                    return Ok(0);
                }
                Err(Errno::EAGAIN) if nonblocking => return Err(Errno::EAGAIN),
                Err(Errno::EAGAIN) => {}
                Err(error) => return Err(error),
            }
            let (task, task_cx_ptr) = block_current_task_no_schedule();
            table.enqueue_waiter(key, Arc::clone(&owner), task);
            publish_flock_activity(&table);
            drop(table);
            schedule(task_cx_ptr);
        }
    } else {
        let mut table = FLOCK_TABLE.lock();
        let waiters = table.unlock(key, &owner);
        publish_flock_activity(&table);
        drop(table);
        wake_waiters(waiters);
        Ok(0)
    }
}

pub(super) fn fcntl_getlk(entry: FdTableEntry, lock: *mut LinuxFlock) -> KResult {
    fcntl_getlk_with_owner(
        entry,
        lock,
        RecordLockOwner::process(current_process().getpid()),
    )
}

pub(super) fn fcntl_ofd_getlk(entry: FdTableEntry, lock: *mut LinuxFlock) -> KResult {
    let owner = RecordLockOwner::file_description(entry.file());
    fcntl_getlk_with_owner(entry, lock, owner)
}

fn fcntl_getlk_with_owner(
    entry: FdTableEntry,
    lock: *mut LinuxFlock,
    owner: RecordLockOwner,
) -> KResult {
    let file = entry.file();
    let token = current_user_token();
    let mut flock = read_user_value(token, lock.cast_const())?;
    if !valid_getlk_type(flock.l_type) {
        return Err(Errno::EINVAL);
    }

    let (start, end) = flock_range(&file, flock)?;
    let key = lock_key(&file)?;
    let conflict = RECORD_LOCK_TABLE
        .lock()
        .find_conflict(key, &owner, flock.l_type, start, end);
    if let Some(conflict) = conflict {
        flock.l_type = conflict.l_type;
        flock.l_whence = SEEK_SET;
        flock.l_start = conflict.start;
        flock.l_len = flock_len(conflict.start, conflict.end);
        flock.l_pid = conflict.owner.reported_pid();
    } else {
        flock.l_type = F_UNLCK;
    }

    write_user_value(token, lock, &flock)?;
    Ok(0)
}

pub(super) fn fcntl_setlk(entry: FdTableEntry, lock: *const LinuxFlock) -> KResult {
    fcntl_setlk_with_owner(
        entry,
        lock,
        RecordLockOwner::process(current_process().getpid()),
    )
}

pub(super) fn fcntl_ofd_setlk(entry: FdTableEntry, lock: *const LinuxFlock) -> KResult {
    let token = current_user_token();
    let flock = read_user_value(token, lock)?;
    if flock.l_pid != 0 {
        return Err(Errno::EINVAL);
    }
    let owner = RecordLockOwner::file_description(entry.file());
    fcntl_setlk_with_owner(entry, lock, owner)
}

fn fcntl_setlk_with_owner(
    entry: FdTableEntry,
    lock: *const LinuxFlock,
    owner: RecordLockOwner,
) -> KResult {
    let file = entry.file();
    let token = current_user_token();
    let flock = read_user_value(token, lock)?;
    if !valid_setlk_type(flock.l_type) {
        return Err(Errno::EINVAL);
    }
    check_lock_access(&file, flock.l_type)?;

    let (start, end) = flock_range(&file, flock)?;
    let key = lock_key(&file)?;
    let mut table = RECORD_LOCK_TABLE.lock();
    let waiters = table.set_lock(key, owner.clone(), flock.l_type, start, end)?;
    match &owner {
        RecordLockOwner::Process(pid) => {
            current_process().set_has_posix_record_locks(table.has_process_locks(*pid));
        }
        RecordLockOwner::FileDescription(_) => {}
    }
    publish_record_lock_activity(&table);
    drop(table);
    wake_waiters(waiters);
    Ok(0)
}

pub(super) fn fcntl_setlkw(entry: FdTableEntry, lock: *const LinuxFlock) -> KResult {
    fcntl_setlkw_with_owner(
        entry,
        lock,
        RecordLockOwner::process(current_process().getpid()),
        true,
    )
}

pub(super) fn fcntl_ofd_setlkw(entry: FdTableEntry, lock: *const LinuxFlock) -> KResult {
    let token = current_user_token();
    let flock = read_user_value(token, lock)?;
    if flock.l_pid != 0 {
        return Err(Errno::EINVAL);
    }
    let owner = RecordLockOwner::file_description(entry.file());
    fcntl_setlkw_with_owner(entry, lock, owner, false)
}

fn fcntl_setlkw_with_owner(
    entry: FdTableEntry,
    lock: *const LinuxFlock,
    owner: RecordLockOwner,
    detect_deadlock: bool,
) -> KResult {
    let file = entry.file();
    let token = current_user_token();
    let flock = read_user_value(token, lock)?;
    if !valid_setlk_type(flock.l_type) {
        return Err(Errno::EINVAL);
    }
    check_lock_access(&file, flock.l_type)?;

    let (start, end) = flock_range(&file, flock)?;
    let key = lock_key(&file)?;
    // UNFINISHED: F_SETLKW waits are not signal-interruptible yet; Linux can
    // return EINTR when a blocked lock request is interrupted by a signal.
    loop {
        let mut table = RECORD_LOCK_TABLE.lock();
        let conflicts = if flock.l_type == F_UNLCK {
            Vec::new()
        } else {
            table.find_conflicts(key, &owner, flock.l_type, start, end)
        };
        if conflicts.is_empty() {
            let waiters = table.set_lock(key, owner.clone(), flock.l_type, start, end)?;
            match &owner {
                RecordLockOwner::Process(pid) => {
                    current_process().set_has_posix_record_locks(table.has_process_locks(*pid));
                }
                RecordLockOwner::FileDescription(_) => {}
            }
            publish_record_lock_activity(&table);
            drop(table);
            wake_waiters(waiters);
            return Ok(0);
        }
        if detect_deadlock && table.would_deadlock(&owner, &conflicts) {
            return Err(Errno::EDEADLK);
        }
        let (task, task_cx_ptr) = block_current_task_no_schedule();
        table.enqueue_waiter(key, owner.clone(), flock.l_type, start, end, task);
        publish_record_lock_activity(&table);
        drop(table);
        schedule(task_cx_ptr);
    }
}

fn release_record_locks_for_close(entry: &FdTableEntry) {
    let process = current_process();
    if !process.has_posix_record_locks() {
        return;
    }
    let file = entry.file();
    let Ok(key) = lock_key(&file) else {
        return;
    };
    let pid = process.getpid();
    let mut table = RECORD_LOCK_TABLE.lock();
    let waiters = table.release_for_process_file(key, pid);
    process.set_has_posix_record_locks(table.has_process_locks(pid));
    publish_record_lock_activity(&table);
    drop(table);
    wake_waiters(waiters);
}

fn release_ofd_record_locks_for_close(entry: &FdTableEntry) {
    let file = entry.file();
    if !RECORD_LOCK_TABLE.lock().has_file_description_state(&file) {
        return;
    }
    if file_description_still_referenced(&file) {
        return;
    }
    let mut table = RECORD_LOCK_TABLE.lock();
    let waiters = table.release_for_file_description(&file);
    publish_record_lock_activity(&table);
    drop(table);
    wake_waiters(waiters);
}

fn release_flock_locks_for_close(entry: &FdTableEntry) {
    let file = entry.file();
    if !FLOCK_TABLE.lock().has_owner_state(&file) {
        return;
    }
    if file_description_still_referenced(&file) {
        return;
    }
    let mut table = FLOCK_TABLE.lock();
    let waiters = table.release_owner(&file);
    publish_flock_activity(&table);
    drop(table);
    wake_waiters(waiters);
}

pub(super) fn release_file_locks_for_close(entry: &FdTableEntry) {
    let active = ACTIVE_LOCK_CLASSES.load(Ordering::Acquire);
    if active & POSIX_LOCKS_ACTIVE != 0 {
        release_record_locks_for_close(entry);
    }
    if active & OFD_LOCKS_ACTIVE != 0 {
        release_ofd_record_locks_for_close(entry);
    }
    if active & FLOCKS_ACTIVE != 0 {
        release_flock_locks_for_close(entry);
    }
}

pub(crate) fn release_record_locks_for_process(pid: usize) {
    if ACTIVE_LOCK_CLASSES.load(Ordering::Acquire) & POSIX_LOCKS_ACTIVE == 0 {
        return;
    }
    let mut table = RECORD_LOCK_TABLE.lock();
    let waiters = table.release_for_process(pid);
    publish_record_lock_activity(&table);
    drop(table);
    wake_waiters(waiters);
}
