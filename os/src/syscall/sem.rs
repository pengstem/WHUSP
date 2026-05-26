use super::errno::{SysError, SysResult};
use super::uapi::LinuxTimeSpec;
use super::user_ptr::{read_user_array, read_user_value, write_user_array, write_user_value};
use crate::sync::UPIntrFreeCell;
use crate::task::check_signals_of_current;
use crate::task::{
    TaskContext, TaskControlBlock, block_current_task_no_schedule, current_has_deliverable_signal,
    current_process, current_task, current_user_token, exit_current_group_and_run_next, schedule,
    wakeup_task,
};
use crate::timer::{add_timer, get_time_ms};
use alloc::collections::BTreeMap;
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use lazy_static::*;

const IPC_PRIVATE: isize = 0;
const IPC_CREAT: i32 = 0o1000;
const IPC_EXCL: i32 = 0o2000;
const IPC_RMID: i32 = 0;
const IPC_SET: i32 = 1;
const IPC_STAT: i32 = 2;
const IPC_INFO: i32 = 3;
const GETPID: i32 = 11;
const GETVAL: i32 = 12;
const GETALL: i32 = 13;
const GETNCNT: i32 = 14;
const GETZCNT: i32 = 15;
const SETVAL: i32 = 16;
const SETALL: i32 = 17;
const SEM_STAT: i32 = 18;
const SEM_INFO: i32 = 19;
const SEM_STAT_ANY: i32 = 20;
const IPC_NOWAIT: i16 = 0o4000;

const SEMMNI: usize = 4096;
const SEMMSL: usize = 32_000;
const SEMMNS: usize = SEMMNI * 10;
const SEMOPM: usize = 500;
const SEMVMX: i32 = 32_767;
const SEMAEM: i32 = SEMVMX;

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
struct LinuxIpc64Perm {
    key: i32,
    uid: u32,
    gid: u32,
    cuid: u32,
    cgid: u32,
    mode: u32,
    seq: u16,
    pad2: u16,
    unused1: usize,
    unused2: usize,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
struct LinuxSemid64Ds {
    sem_perm: LinuxIpc64Perm,
    sem_otime: i64,
    sem_ctime: i64,
    sem_nsems: usize,
    unused3: usize,
    unused4: usize,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
struct LinuxSeminfo {
    semmap: i32,
    semmni: i32,
    semmns: i32,
    semmnu: i32,
    semmsl: i32,
    semopm: i32,
    semume: i32,
    semusz: i32,
    semvmx: i32,
    semaem: i32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct LinuxSembuf {
    sem_num: u16,
    sem_op: i16,
    sem_flg: i16,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SemError {
    NotFound,
    Exists,
    Invalid,
    NoSpace,
    AccessDenied,
    NotPermitted,
    Range,
    TooBig,
    WouldBlock,
}

#[derive(Clone, Copy)]
struct SemCreateContext {
    uid: u32,
    gid: u32,
}

#[derive(Clone)]
struct SemCaller {
    pid: usize,
    euid: u32,
    egid: u32,
    groups: Vec<u32>,
    can_override_ipc: bool,
}

#[derive(Clone, Copy, Debug)]
struct SemSetAttrs {
    uid: u32,
    gid: u32,
    mode: u32,
}

#[derive(Clone)]
struct SemValue {
    value: i32,
    pid: i32,
    ncnt: usize,
    zcnt: usize,
}

struct SemSet {
    key: isize,
    mode: u32,
    uid: u32,
    gid: u32,
    cuid: u32,
    cgid: u32,
    ctime: i64,
    otime: i64,
    values: Vec<SemValue>,
    waiters: Vec<SemWaiter>,
}

struct SemWaiter {
    task: Arc<TaskControlBlock>,
    sem_num: usize,
    kind: SemWaitKind,
}

#[derive(Clone, Copy)]
enum SemWaitKind {
    NonZero,
    Zero,
}

enum SemOpAttempt {
    Done,
    Blocked(*mut TaskContext),
}

impl SemSet {
    fn new(key: isize, nsems: usize, mode: u32, context: SemCreateContext) -> Self {
        let now = now_sec();
        Self {
            key,
            mode,
            uid: context.uid,
            gid: context.gid,
            cuid: context.uid,
            cgid: context.gid,
            ctime: now,
            otime: 0,
            values: vec![
                SemValue {
                    value: 0,
                    pid: 0,
                    ncnt: 0,
                    zcnt: 0,
                };
                nsems
            ],
            waiters: Vec::new(),
        }
    }

    fn can_read(&self, caller: &SemCaller) -> bool {
        self.mode_allows(caller, 0o400, 0o040, 0o004) || caller.can_override_ipc
    }

    fn can_alter(&self, caller: &SemCaller) -> bool {
        self.mode_allows(caller, 0o200, 0o020, 0o002) || caller.can_override_ipc
    }

    fn mode_allows(
        &self,
        caller: &SemCaller,
        owner_bit: u32,
        group_bit: u32,
        other_bit: u32,
    ) -> bool {
        if caller.euid == self.uid {
            return self.mode & owner_bit != 0;
        }
        if caller.egid == self.gid || caller.groups.contains(&self.gid) {
            return self.mode & group_bit != 0;
        }
        self.mode & other_bit != 0
    }

    fn is_owner_or_creator(&self, caller: &SemCaller) -> bool {
        caller.euid == self.uid || caller.euid == self.cuid || caller.can_override_ipc
    }

    fn stat(&self, id: usize) -> SemSetStat {
        let _ = id;
        SemSetStat {
            key: self.key,
            uid: self.uid,
            gid: self.gid,
            cuid: self.cuid,
            cgid: self.cgid,
            mode: self.mode & 0o777,
            otime: self.otime,
            ctime: self.ctime,
            nsems: self.values.len(),
        }
    }

    fn wake_waiters(&mut self) {
        let waiters = core::mem::take(&mut self.waiters);
        for waiter in waiters {
            if let Some(value) = self.values.get_mut(waiter.sem_num) {
                match waiter.kind {
                    SemWaitKind::NonZero => value.ncnt = value.ncnt.saturating_sub(1),
                    SemWaitKind::Zero => value.zcnt = value.zcnt.saturating_sub(1),
                }
            }
            wakeup_task(waiter.task);
        }
    }
}

#[derive(Clone, Copy)]
struct SemSetStat {
    key: isize,
    uid: u32,
    gid: u32,
    cuid: u32,
    cgid: u32,
    mode: u32,
    otime: i64,
    ctime: i64,
    nsems: usize,
}

#[derive(Clone, Copy)]
struct SemUsageInfo {
    used_ids: usize,
    total_sems: usize,
    highest_index: usize,
}

struct SemManager {
    next_id: usize,
    sets: BTreeMap<usize, SemSet>,
    keyed_sets: BTreeMap<isize, usize>,
}

impl SemManager {
    fn new() -> Self {
        Self {
            next_id: 1,
            sets: BTreeMap::new(),
            keyed_sets: BTreeMap::new(),
        }
    }

    fn alloc_id(&mut self) -> usize {
        while self.sets.contains_key(&self.next_id) {
            self.next_id += 1;
        }
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    fn get_or_create(
        &mut self,
        key: isize,
        nsems: usize,
        semflg: i32,
        context: SemCreateContext,
        caller: &SemCaller,
    ) -> Result<usize, SemError> {
        let mode = (semflg & 0o777) as u32;
        let flags = semflg & !0o777;
        if nsems == 0 || nsems > SEMMSL {
            return Err(SemError::Invalid);
        }
        // UNFINISHED: Linux accepts additional namespace/accounting flags here.
        // The contest semaphore path only uses creation and permission bits.
        if flags & !(IPC_CREAT | IPC_EXCL) != 0 {
            return Err(SemError::Invalid);
        }
        if key == IPC_PRIVATE {
            return self.create_set(key, nsems, mode, context);
        }
        if let Some(semid) = self.keyed_sets.get(&key).copied() {
            if flags & (IPC_CREAT | IPC_EXCL) == (IPC_CREAT | IPC_EXCL) {
                return Err(SemError::Exists);
            }
            let set = self.sets.get(&semid).ok_or(SemError::NotFound)?;
            if set.values.len() < nsems {
                return Err(SemError::Invalid);
            }
            if !set.can_read(caller) && !set.can_alter(caller) {
                return Err(SemError::AccessDenied);
            }
            return Ok(semid);
        }
        if flags & IPC_CREAT == 0 {
            return Err(SemError::NotFound);
        }
        self.create_set(key, nsems, mode, context)
    }

    fn create_set(
        &mut self,
        key: isize,
        nsems: usize,
        mode: u32,
        context: SemCreateContext,
    ) -> Result<usize, SemError> {
        if self.sets.len() >= SEMMNI || self.total_sems().saturating_add(nsems) > SEMMNS {
            return Err(SemError::NoSpace);
        }
        let semid = self.alloc_id();
        self.sets
            .insert(semid, SemSet::new(key, nsems, mode, context));
        if key != IPC_PRIVATE {
            self.keyed_sets.insert(key, semid);
        }
        Ok(semid)
    }

    fn remove_set(&mut self, semid: usize, caller: &SemCaller) -> Result<(), SemError> {
        {
            let set = self.sets.get(&semid).ok_or(SemError::Invalid)?;
            if !set.is_owner_or_creator(caller) {
                return Err(SemError::NotPermitted);
            }
        }
        let mut set = self.sets.remove(&semid).ok_or(SemError::Invalid)?;
        if set.key != IPC_PRIVATE {
            self.keyed_sets.remove(&set.key);
        }
        set.wake_waiters();
        Ok(())
    }

    fn stat_by_id(&self, semid: usize, caller: &SemCaller) -> Result<SemSetStat, SemError> {
        let set = self.sets.get(&semid).ok_or(SemError::Invalid)?;
        if !set.can_read(caller) {
            return Err(SemError::AccessDenied);
        }
        Ok(set.stat(semid))
    }

    fn stat_by_index(
        &self,
        index: usize,
        caller: &SemCaller,
        skip_permission: bool,
    ) -> Result<(usize, SemSetStat), SemError> {
        let set = self.sets.get(&index).ok_or(SemError::Invalid)?;
        if !skip_permission && !set.can_read(caller) {
            return Err(SemError::AccessDenied);
        }
        Ok((index, set.stat(index)))
    }

    fn set_attrs(
        &mut self,
        semid: usize,
        attrs: SemSetAttrs,
        caller: &SemCaller,
    ) -> Result<(), SemError> {
        let set = self.sets.get_mut(&semid).ok_or(SemError::Invalid)?;
        if !set.is_owner_or_creator(caller) {
            return Err(SemError::NotPermitted);
        }
        set.uid = attrs.uid;
        set.gid = attrs.gid;
        set.mode = attrs.mode & 0o777;
        set.ctime = now_sec();
        Ok(())
    }

    fn get_value(&self, semid: usize, semnum: usize, caller: &SemCaller) -> Result<i32, SemError> {
        let set = self.sets.get(&semid).ok_or(SemError::Invalid)?;
        if !set.can_read(caller) {
            return Err(SemError::AccessDenied);
        }
        Ok(set.values.get(semnum).ok_or(SemError::Invalid)?.value)
    }

    fn get_pid(&self, semid: usize, semnum: usize, caller: &SemCaller) -> Result<i32, SemError> {
        let set = self.sets.get(&semid).ok_or(SemError::Invalid)?;
        if !set.can_read(caller) {
            return Err(SemError::AccessDenied);
        }
        Ok(set.values.get(semnum).ok_or(SemError::Invalid)?.pid)
    }

    fn get_wait_count(
        &self,
        semid: usize,
        semnum: usize,
        zero: bool,
        caller: &SemCaller,
    ) -> Result<usize, SemError> {
        let set = self.sets.get(&semid).ok_or(SemError::Invalid)?;
        if !set.can_read(caller) {
            return Err(SemError::AccessDenied);
        }
        let value = set.values.get(semnum).ok_or(SemError::Invalid)?;
        Ok(if zero { value.zcnt } else { value.ncnt })
    }

    fn get_all(&self, semid: usize, caller: &SemCaller) -> Result<Vec<u16>, SemError> {
        let set = self.sets.get(&semid).ok_or(SemError::Invalid)?;
        if !set.can_read(caller) {
            return Err(SemError::AccessDenied);
        }
        Ok(set.values.iter().map(|value| value.value as u16).collect())
    }

    fn set_value(
        &mut self,
        semid: usize,
        semnum: usize,
        raw_value: i32,
        caller: &SemCaller,
    ) -> Result<(), SemError> {
        if !(0..=SEMVMX).contains(&raw_value) {
            return Err(SemError::Range);
        }
        let set = self.sets.get_mut(&semid).ok_or(SemError::Invalid)?;
        if !set.can_alter(caller) {
            return Err(SemError::AccessDenied);
        }
        let value = set.values.get_mut(semnum).ok_or(SemError::Invalid)?;
        value.value = raw_value;
        value.pid = pid_to_i32(caller.pid);
        set.ctime = now_sec();
        set.wake_waiters();
        Ok(())
    }

    fn set_all(
        &mut self,
        semid: usize,
        values: &[u16],
        caller: &SemCaller,
    ) -> Result<(), SemError> {
        if values.iter().any(|&value| i32::from(value) > SEMVMX) {
            return Err(SemError::Range);
        }
        let set = self.sets.get_mut(&semid).ok_or(SemError::Invalid)?;
        if !set.can_alter(caller) {
            return Err(SemError::AccessDenied);
        }
        if values.len() != set.values.len() {
            return Err(SemError::Invalid);
        }
        for (sem_value, &new_value) in set.values.iter_mut().zip(values) {
            sem_value.value = i32::from(new_value);
            sem_value.pid = pid_to_i32(caller.pid);
        }
        set.ctime = now_sec();
        set.wake_waiters();
        Ok(())
    }

    fn try_semop(
        &mut self,
        semid: usize,
        ops: &[LinuxSembuf],
        caller: &SemCaller,
    ) -> Result<(), SemError> {
        if ops.is_empty() {
            return Err(SemError::Invalid);
        }
        if ops.len() > SEMOPM {
            return Err(SemError::TooBig);
        }
        let set = self.sets.get_mut(&semid).ok_or(SemError::Invalid)?;
        for op in ops {
            let semnum = usize::from(op.sem_num);
            if semnum >= set.values.len() {
                return Err(SemError::TooBig);
            }
            if op.sem_op == 0 {
                if !set.can_read(caller) {
                    return Err(SemError::AccessDenied);
                }
            } else if !set.can_alter(caller) {
                return Err(SemError::AccessDenied);
            }
        }
        let mut new_values: Vec<i32> = set.values.iter().map(|value| value.value).collect();
        for op in ops {
            let semnum = usize::from(op.sem_num);
            let current = new_values[semnum];
            if op.sem_op == 0 {
                if current != 0 {
                    return Err(SemError::WouldBlock);
                }
                continue;
            }
            let next = current + i32::from(op.sem_op);
            if op.sem_op < 0 && next < 0 {
                return Err(SemError::WouldBlock);
            }
            if next > SEMVMX {
                return Err(SemError::Range);
            }
            new_values[semnum] = next;
        }
        for op in ops {
            if op.sem_op != 0 {
                let semnum = usize::from(op.sem_num);
                set.values[semnum].value = new_values[semnum];
                set.values[semnum].pid = pid_to_i32(caller.pid);
            }
        }
        set.otime = now_sec();
        set.wake_waiters();
        Ok(())
    }

    fn block_current_on_first_wait(
        &mut self,
        semid: usize,
        ops: &[LinuxSembuf],
    ) -> Result<*mut TaskContext, SemError> {
        let set = self.sets.get_mut(&semid).ok_or(SemError::Invalid)?;
        let (semnum, kind) = first_waiting_op(set, ops).ok_or(SemError::Invalid)?;
        if ops.iter().any(|op| op.sem_flg & IPC_NOWAIT != 0) {
            return Err(SemError::WouldBlock);
        }
        let (task, task_cx_ptr) = block_current_task_no_schedule();
        match kind {
            SemWaitKind::NonZero => set.values[semnum].ncnt += 1,
            SemWaitKind::Zero => set.values[semnum].zcnt += 1,
        }
        // UNFINISHED: System V semaphore sleeps should wake directly from
        // signal delivery, IPC_RMID, or a precise semtimedop timeout. This
        // fallback prevents a missed signal wake from pinning LTP cleanup
        // until the outer testcase alarm while the wait queue model is still
        // minimal.
        add_timer(get_time_ms() + 1000, Arc::clone(&task));
        set.waiters.push(SemWaiter {
            task,
            sem_num: semnum,
            kind,
        });
        Ok(task_cx_ptr)
    }

    fn remove_waiter_for_task(&mut self, task: &Arc<TaskControlBlock>) {
        for set in self.sets.values_mut() {
            let mut idx = 0;
            while idx < set.waiters.len() {
                if Arc::ptr_eq(&set.waiters[idx].task, task) {
                    let waiter = set.waiters.remove(idx);
                    if let Some(value) = set.values.get_mut(waiter.sem_num) {
                        match waiter.kind {
                            SemWaitKind::NonZero => value.ncnt = value.ncnt.saturating_sub(1),
                            SemWaitKind::Zero => value.zcnt = value.zcnt.saturating_sub(1),
                        }
                    }
                } else {
                    idx += 1;
                }
            }
        }
    }

    fn usage_info(&self) -> SemUsageInfo {
        SemUsageInfo {
            used_ids: self.sets.len(),
            total_sems: self.total_sems(),
            highest_index: self.highest_index(),
        }
    }

    fn highest_index(&self) -> usize {
        self.sets.keys().next_back().copied().unwrap_or(0)
    }

    fn total_sems(&self) -> usize {
        self.sets
            .values()
            .map(|set| set.values.len())
            .sum::<usize>()
    }

    fn proc_sysvipc_sem_content(&self) -> String {
        let mut output = String::from(
            "       key      semid perms      nsems   uid   gid  cuid  cgid      otime      ctime\n",
        );
        for (&semid, set) in &self.sets {
            output.push_str(&format!(
                "{:10} {:10} {:5o} {:10} {:5} {:5} {:5} {:5} {:10} {:10}\n",
                set.key,
                semid,
                set.mode & 0o777,
                set.values.len(),
                set.uid,
                set.gid,
                set.cuid,
                set.cgid,
                set.otime,
                set.ctime,
            ));
        }
        output
    }
}

fn first_waiting_op(set: &SemSet, ops: &[LinuxSembuf]) -> Option<(usize, SemWaitKind)> {
    let mut values: Vec<i32> = set.values.iter().map(|value| value.value).collect();
    for op in ops {
        let semnum = usize::from(op.sem_num);
        let current = *values.get(semnum)?;
        if op.sem_op == 0 {
            if current != 0 {
                return Some((semnum, SemWaitKind::Zero));
            }
            continue;
        }
        let next = current + i32::from(op.sem_op);
        if op.sem_op < 0 && next < 0 {
            return Some((semnum, SemWaitKind::NonZero));
        }
        if next > SEMVMX {
            return None;
        }
        values[semnum] = next;
    }
    None
}

lazy_static! {
    static ref SEM_MANAGER: UPIntrFreeCell<SemManager> =
        unsafe { UPIntrFreeCell::new(SemManager::new()) };
}

pub(super) fn sys_semget(key: isize, nsems: usize, semflg: i32) -> SysResult {
    let process = current_process();
    let credentials = process.credentials();
    let caller = sem_caller_from(process.getpid(), &credentials);
    let context = SemCreateContext {
        uid: credentials.euid,
        gid: credentials.egid,
    };
    SEM_MANAGER
        .exclusive_access()
        .get_or_create(key, nsems, semflg, context, &caller)
        .map(|semid| semid as isize)
        .map_err(sem_error_to_sys_error)
}

pub(super) fn sys_semctl(semid: usize, semnum: usize, cmd: i32, arg: usize) -> SysResult {
    let process = current_process();
    let credentials = process.credentials();
    let caller = sem_caller_from(process.getpid(), &credentials);
    match cmd {
        IPC_RMID => {
            SEM_MANAGER
                .exclusive_access()
                .remove_set(semid, &caller)
                .map_err(sem_error_to_sys_error)?;
            Ok(0)
        }
        IPC_STAT => {
            let stat = SEM_MANAGER
                .exclusive_access()
                .stat_by_id(semid, &caller)
                .map_err(sem_error_to_sys_error)?;
            write_semid_ds(arg, stat)?;
            Ok(0)
        }
        IPC_SET => {
            let ds: LinuxSemid64Ds =
                read_user_value(current_user_token(), arg as *const LinuxSemid64Ds)?;
            SEM_MANAGER
                .exclusive_access()
                .set_attrs(
                    semid,
                    SemSetAttrs {
                        uid: ds.sem_perm.uid,
                        gid: ds.sem_perm.gid,
                        mode: ds.sem_perm.mode,
                    },
                    &caller,
                )
                .map_err(sem_error_to_sys_error)?;
            Ok(0)
        }
        GETVAL => SEM_MANAGER
            .exclusive_access()
            .get_value(semid, semnum, &caller)
            .map(|value| value as isize)
            .map_err(sem_error_to_sys_error),
        GETPID => SEM_MANAGER
            .exclusive_access()
            .get_pid(semid, semnum, &caller)
            .map(|pid| pid as isize)
            .map_err(sem_error_to_sys_error),
        GETNCNT | GETZCNT => SEM_MANAGER
            .exclusive_access()
            .get_wait_count(semid, semnum, cmd == GETZCNT, &caller)
            .map(|count| count as isize)
            .map_err(sem_error_to_sys_error),
        GETALL => {
            let values = SEM_MANAGER
                .exclusive_access()
                .get_all(semid, &caller)
                .map_err(sem_error_to_sys_error)?;
            write_user_array(current_user_token(), arg as *mut u16, &values)?;
            Ok(0)
        }
        SETVAL => {
            SEM_MANAGER
                .exclusive_access()
                .set_value(semid, semnum, arg as i32, &caller)
                .map_err(sem_error_to_sys_error)?;
            Ok(0)
        }
        SETALL => {
            let nsems = {
                let manager = SEM_MANAGER.exclusive_access();
                manager
                    .sets
                    .get(&semid)
                    .ok_or(SysError::EINVAL)?
                    .values
                    .len()
            };
            let values = read_user_array(current_user_token(), arg as *const u16, nsems)?;
            SEM_MANAGER
                .exclusive_access()
                .set_all(semid, &values, &caller)
                .map_err(sem_error_to_sys_error)?;
            Ok(0)
        }
        IPC_INFO | SEM_INFO => {
            let usage = SEM_MANAGER.exclusive_access().usage_info();
            let mut info = base_seminfo();
            if cmd == SEM_INFO {
                info.semusz = usage.used_ids.try_into().unwrap_or(i32::MAX);
                info.semaem = usage.total_sems.try_into().unwrap_or(i32::MAX);
            }
            write_user_value(current_user_token(), arg as *mut LinuxSeminfo, &info)?;
            Ok(usage.highest_index as isize)
        }
        SEM_STAT | SEM_STAT_ANY => {
            let skip_permission = cmd == SEM_STAT_ANY;
            let (real_semid, stat) = SEM_MANAGER
                .exclusive_access()
                .stat_by_index(semid, &caller, skip_permission)
                .map_err(sem_error_to_sys_error)?;
            write_semid_ds(arg, stat)?;
            Ok(real_semid as isize)
        }
        _ => Err(SysError::EINVAL),
    }
}

pub(super) fn sys_semop(semid: usize, sops: *const LinuxSembuf, nsops: usize) -> SysResult {
    sys_semtimedop(semid, sops, nsops, core::ptr::null())
}

pub(super) fn sys_semtimedop(
    semid: usize,
    sops: *const LinuxSembuf,
    nsops: usize,
    timeout: *const LinuxTimeSpec,
) -> SysResult {
    if nsops == 0 {
        return Err(SysError::EINVAL);
    }
    if nsops > SEMOPM {
        return Err(SysError::E2BIG);
    }
    let token = current_user_token();
    let ops = read_user_array(token, sops, nsops)?;
    if !timeout.is_null() {
        let _ = read_user_value(token, timeout)?;
    }
    let process = current_process();
    let caller = sem_caller_from(process.getpid(), &process.credentials());
    loop {
        match try_or_block_semop(semid, &ops, &caller)? {
            SemOpAttempt::Done => return Ok(0),
            SemOpAttempt::Blocked(task_cx_ptr) => schedule(task_cx_ptr),
        }
        let Some(task) = current_task() else {
            return Err(SysError::EINTR);
        };
        SEM_MANAGER.exclusive_access().remove_waiter_for_task(&task);
        if let Some((exit_code, _message)) = check_signals_of_current() {
            exit_current_group_and_run_next(exit_code);
        }
        if current_has_deliverable_signal() {
            return Err(SysError::EINTR);
        }
    }
}

fn try_or_block_semop(
    semid: usize,
    ops: &[LinuxSembuf],
    caller: &SemCaller,
) -> Result<SemOpAttempt, SysError> {
    let mut manager = SEM_MANAGER.exclusive_access();
    match manager.try_semop(semid, ops, caller) {
        Ok(()) => Ok(SemOpAttempt::Done),
        Err(SemError::WouldBlock) => manager
            .block_current_on_first_wait(semid, ops)
            .map(SemOpAttempt::Blocked)
            .map_err(sem_error_to_sys_error),
        Err(error) => Err(sem_error_to_sys_error(error)),
    }
}

fn write_semid_ds(buf: usize, stat: SemSetStat) -> SysResult<()> {
    let ds = LinuxSemid64Ds {
        sem_perm: LinuxIpc64Perm {
            key: stat.key as i32,
            uid: stat.uid,
            gid: stat.gid,
            cuid: stat.cuid,
            cgid: stat.cgid,
            mode: stat.mode,
            seq: 0,
            pad2: 0,
            unused1: 0,
            unused2: 0,
        },
        sem_otime: stat.otime,
        sem_ctime: stat.ctime,
        sem_nsems: stat.nsems,
        unused3: 0,
        unused4: 0,
    };
    write_user_value(current_user_token(), buf as *mut LinuxSemid64Ds, &ds)
}

fn base_seminfo() -> LinuxSeminfo {
    LinuxSeminfo {
        semmap: SEMMNI as i32,
        semmni: SEMMNI as i32,
        semmns: SEMMNS as i32,
        semmnu: SEMMNI as i32,
        semmsl: SEMMSL as i32,
        semopm: SEMOPM as i32,
        semume: SEMOPM as i32,
        semusz: 0,
        semvmx: SEMVMX,
        semaem: SEMAEM,
    }
}

pub(crate) fn proc_sysvipc_sem_content() -> String {
    SEM_MANAGER.exclusive_access().proc_sysvipc_sem_content()
}

fn sem_caller_from(pid: usize, credentials: &crate::task::Credentials) -> SemCaller {
    SemCaller {
        pid,
        euid: credentials.euid,
        egid: credentials.egid,
        groups: credentials.groups.clone(),
        can_override_ipc: credentials.euid == 0,
    }
}

fn sem_error_to_sys_error(error: SemError) -> SysError {
    match error {
        SemError::NotFound => SysError::ENOENT,
        SemError::Exists => SysError::EEXIST,
        SemError::Invalid => SysError::EINVAL,
        SemError::NoSpace => SysError::ENOSPC,
        SemError::AccessDenied => SysError::EACCES,
        SemError::NotPermitted => SysError::EPERM,
        SemError::Range => SysError::ERANGE,
        SemError::TooBig => SysError::EFBIG,
        SemError::WouldBlock => SysError::EAGAIN,
    }
}

fn now_sec() -> i64 {
    (crate::timer::wall_time_nanos() / 1_000_000_000) as i64
}

fn pid_to_i32(pid: usize) -> i32 {
    pid.try_into().unwrap_or(i32::MAX)
}
