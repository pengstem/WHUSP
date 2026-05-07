use crate::fs::{File, SeekWhence};
use crate::sync::UPIntrFreeCell;
use crate::syscall::errno::{SysError, SysResult};
use crate::syscall::user_ptr::{read_user_value, write_user_value};
use crate::task::{current_process, current_user_token, FdTableEntry};
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::convert::TryFrom;
use lazy_static::lazy_static;

const F_RDLCK: i16 = 0;
const F_WRLCK: i16 = 1;
const F_UNLCK: i16 = 2;
const SEEK_SET: i16 = 0;
const SEEK_CUR: i16 = 1;
const SEEK_END: i16 = 2;
const LOCK_TO_EOF: i64 = i64::MAX;

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

#[derive(Clone, Copy, Debug)]
struct PosixLock {
    key: LockKey,
    pid: usize,
    l_type: i16,
    start: i64,
    end: i64,
}

struct RecordLockTable {
    locks: Vec<PosixLock>,
}

lazy_static! {
    static ref RECORD_LOCK_TABLE: UPIntrFreeCell<RecordLockTable> =
        unsafe { UPIntrFreeCell::new(RecordLockTable::new()) };
}

impl RecordLockTable {
    fn new() -> Self {
        Self { locks: Vec::new() }
    }

    fn set_lock(
        &mut self,
        key: LockKey,
        pid: usize,
        l_type: i16,
        start: i64,
        end: i64,
    ) -> SysResult<()> {
        if l_type != F_UNLCK && self.find_conflict(key, pid, l_type, start, end).is_some() {
            return Err(SysError::EAGAIN);
        }

        self.remove_owned_range(key, pid, start, end);
        if l_type != F_UNLCK {
            self.locks.push(PosixLock {
                key,
                pid,
                l_type,
                start,
                end,
            });
            self.merge_adjacent();
        }
        Ok(())
    }

    fn find_conflict(
        &self,
        key: LockKey,
        pid: usize,
        requested_type: i16,
        start: i64,
        end: i64,
    ) -> Option<PosixLock> {
        self.locks
            .iter()
            .copied()
            .filter(|lock| {
                lock.key == key
                    && lock.pid != pid
                    && lock_conflicts(lock.l_type, requested_type)
                    && ranges_overlap(lock.start, lock.end, start, end)
            })
            .min_by_key(|lock| (lock.start, lock.end))
    }

    fn remove_owned_range(&mut self, key: LockKey, pid: usize, start: i64, end: i64) {
        let mut next = Vec::new();
        for lock in self.locks.drain(..) {
            if lock.key != key
                || lock.pid != pid
                || !ranges_overlap(lock.start, lock.end, start, end)
            {
                next.push(lock);
                continue;
            }

            if lock.start < start {
                next.push(PosixLock {
                    end: start - 1,
                    ..lock
                });
            }
            if end != LOCK_TO_EOF && end < lock.end {
                next.push(PosixLock {
                    start: end + 1,
                    ..lock
                });
            }
        }
        self.locks = next;
    }

    fn release_for_process_file(&mut self, key: LockKey, pid: usize) {
        self.locks
            .retain(|lock| !(lock.key == key && lock.pid == pid));
    }

    fn merge_adjacent(&mut self) {
        self.locks
            .sort_by_key(|lock| (lock.key, lock.pid, lock.l_type, lock.start, lock.end));
        let mut merged: Vec<PosixLock> = Vec::new();
        for lock in self.locks.drain(..) {
            if let Some(last) = merged.last_mut()
                && last.key == lock.key
                && last.pid == lock.pid
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

fn ranges_overlap(first_start: i64, first_end: i64, second_start: i64, second_end: i64) -> bool {
    first_start <= second_end && second_start <= first_end
}

fn lock_key(file: &Arc<dyn File + Send + Sync>) -> SysResult<LockKey> {
    let stat = file.stat()?;
    Ok(LockKey {
        dev: stat.dev,
        ino: stat.ino,
    })
}

fn flock_range(file: &Arc<dyn File + Send + Sync>, flock: LinuxFlock) -> SysResult<(i64, i64)> {
    if !valid_flock_whence(flock.l_whence) {
        return Err(SysError::EINVAL);
    }
    let base = match flock.l_whence {
        SEEK_SET => 0,
        SEEK_CUR => {
            i64::try_from(file.seek(0, SeekWhence::Current)?).map_err(|_| SysError::EINVAL)?
        }
        SEEK_END => i64::try_from(file.stat()?.size).map_err(|_| SysError::EINVAL)?,
        _ => unreachable!(),
    };
    let mut start = base.checked_add(flock.l_start).ok_or(SysError::EINVAL)?;
    if start < 0 {
        return Err(SysError::EINVAL);
    }

    let end = if flock.l_len > 0 {
        let len_last = flock.l_len.checked_sub(1).ok_or(SysError::EINVAL)?;
        start.checked_add(len_last).ok_or(SysError::EINVAL)?
    } else if flock.l_len < 0 {
        let end = start.checked_sub(1).ok_or(SysError::EINVAL)?;
        start = start.checked_add(flock.l_len).ok_or(SysError::EINVAL)?;
        if start < 0 {
            return Err(SysError::EINVAL);
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

fn check_lock_access(file: &Arc<dyn File + Send + Sync>, l_type: i16) -> SysResult<()> {
    match l_type {
        F_RDLCK if !file.readable() => Err(SysError::EBADF),
        F_WRLCK if !file.writable() => Err(SysError::EBADF),
        _ => Ok(()),
    }
}

pub(super) fn fcntl_getlk(entry: FdTableEntry, lock: *mut LinuxFlock) -> SysResult {
    let file = entry.file();
    let token = current_user_token();
    let mut flock = read_user_value(token, lock.cast_const())?;
    if !valid_getlk_type(flock.l_type) {
        return Err(SysError::EINVAL);
    }

    let (start, end) = flock_range(&file, flock)?;
    let key = lock_key(&file)?;
    let pid = current_process().getpid();
    let conflict =
        RECORD_LOCK_TABLE
            .exclusive_access()
            .find_conflict(key, pid, flock.l_type, start, end);
    if let Some(conflict) = conflict {
        flock.l_type = conflict.l_type;
        flock.l_whence = SEEK_SET;
        flock.l_start = conflict.start;
        flock.l_len = flock_len(conflict.start, conflict.end);
        flock.l_pid = conflict.pid as i32;
    } else {
        flock.l_type = F_UNLCK;
    }

    write_user_value(token, lock, &flock)?;
    Ok(0)
}

pub(super) fn fcntl_setlk(entry: FdTableEntry, lock: *const LinuxFlock) -> SysResult {
    let file = entry.file();
    let token = current_user_token();
    let flock = read_user_value(token, lock)?;
    if !valid_setlk_type(flock.l_type) {
        return Err(SysError::EINVAL);
    }
    check_lock_access(&file, flock.l_type)?;

    let (start, end) = flock_range(&file, flock)?;
    let key = lock_key(&file)?;
    let pid = current_process().getpid();
    // UNFINISHED: F_SETLKW still uses nonblocking F_SETLK behavior; signalable
    // waits and deadlock detection are not implemented yet.
    RECORD_LOCK_TABLE
        .exclusive_access()
        .set_lock(key, pid, flock.l_type, start, end)?;
    Ok(0)
}

pub(super) fn release_record_locks_for_close(entry: &FdTableEntry) {
    let file = entry.file();
    let Ok(key) = lock_key(&file) else {
        return;
    };
    let pid = current_process().getpid();
    RECORD_LOCK_TABLE
        .exclusive_access()
        .release_for_process_file(key, pid);
}
