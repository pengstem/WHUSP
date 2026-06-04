use super::errno::{SysError, SysResult};
use super::user_ptr::{copy_to_user, read_user_array, read_user_value, write_user_value};
use crate::perf;
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
use alloc::vec::Vec;
use core::sync::atomic::{AtomicIsize, AtomicUsize, Ordering};
use lazy_static::*;

const IPC_PRIVATE: isize = 0;
const IPC_CREAT: i32 = 0o1000;
const IPC_EXCL: i32 = 0o2000;
const IPC_NOWAIT: i32 = 0o4000;
const IPC_RMID: i32 = 0;
const IPC_SET: i32 = 1;
const IPC_STAT: i32 = 2;
const IPC_INFO: i32 = 3;
const MSG_STAT: i32 = 11;
const MSG_INFO: i32 = 12;
const MSG_STAT_ANY: i32 = 13;
const MSG_NOERROR: i32 = 0o10000;
const MSG_EXCEPT: i32 = 0o20000;
const MSG_COPY: i32 = 0o40000;

const MSGMNI_DEFAULT: usize = 16;
const MSGMAX_DEFAULT: usize = 8192;
const MSGMNB_DEFAULT: usize = 16_384;

static MSGMNI: AtomicUsize = AtomicUsize::new(MSGMNI_DEFAULT);
static MSGMAX: AtomicUsize = AtomicUsize::new(MSGMAX_DEFAULT);
static MSGMNB: AtomicUsize = AtomicUsize::new(MSGMNB_DEFAULT);
static MSG_NEXT_ID: AtomicIsize = AtomicIsize::new(-1);

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
struct LinuxMsqid64Ds {
    msg_perm: LinuxIpc64Perm,
    msg_stime: i64,
    msg_rtime: i64,
    msg_ctime: i64,
    msg_cbytes: usize,
    msg_qnum: usize,
    msg_qbytes: usize,
    msg_lspid: i32,
    msg_lrpid: i32,
    unused4: usize,
    unused5: usize,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
struct LinuxMsgInfo {
    msgpool: i32,
    msgmap: i32,
    msgmax: i32,
    msgmnb: i32,
    msgmni: i32,
    msgssz: i32,
    msgtql: i32,
    msgseg: u16,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum MsgError {
    NotFound,
    Exists,
    Invalid,
    NoSpace,
    AccessDenied,
    NotPermitted,
    TooBig,
    NoMessage,
    WouldBlock,
}

#[derive(Clone)]
struct MsgCaller {
    pid: usize,
    euid: u32,
    egid: u32,
    groups: Vec<u32>,
    can_override_ipc: bool,
}

#[derive(Clone, Copy)]
struct MsgCreateContext {
    uid: u32,
    gid: u32,
}

#[derive(Clone, Copy)]
struct MsgSetAttrs {
    uid: u32,
    gid: u32,
    mode: u32,
    qbytes: usize,
}

#[derive(Clone)]
struct Message {
    mtype: isize,
    text: Vec<u8>,
}

struct MsgQueue {
    key: isize,
    mode: u32,
    uid: u32,
    gid: u32,
    cuid: u32,
    cgid: u32,
    stime: i64,
    rtime: i64,
    ctime: i64,
    qbytes: usize,
    cbytes: usize,
    lspid: i32,
    lrpid: i32,
    messages: Vec<Message>,
    waiters: Vec<MsgWaiter>,
}

struct MsgWaiter {
    task: Arc<TaskControlBlock>,
}

enum MsgAttempt {
    Done(isize),
    Blocked(*mut TaskContext),
}

#[derive(Clone, Copy)]
struct MsgQueueStat {
    key: isize,
    uid: u32,
    gid: u32,
    cuid: u32,
    cgid: u32,
    mode: u32,
    stime: i64,
    rtime: i64,
    ctime: i64,
    cbytes: usize,
    qnum: usize,
    qbytes: usize,
    lspid: i32,
    lrpid: i32,
}

#[derive(Clone, Copy)]
struct MsgUsageInfo {
    used_ids: usize,
    total_messages: usize,
    total_bytes: usize,
    highest_index: usize,
}

impl MsgQueue {
    fn new(key: isize, mode: u32, context: MsgCreateContext) -> Self {
        Self {
            key,
            mode,
            uid: context.uid,
            gid: context.gid,
            cuid: context.uid,
            cgid: context.gid,
            stime: 0,
            rtime: 0,
            ctime: now_sec(),
            qbytes: current_msgmnb(),
            cbytes: 0,
            lspid: 0,
            lrpid: 0,
            messages: Vec::new(),
            waiters: Vec::new(),
        }
    }

    fn can_read(&self, caller: &MsgCaller) -> bool {
        self.mode_allows(caller, 0o400, 0o040, 0o004) || caller.can_override_ipc
    }

    fn can_write(&self, caller: &MsgCaller) -> bool {
        self.mode_allows(caller, 0o200, 0o020, 0o002) || caller.can_override_ipc
    }

    fn mode_allows(
        &self,
        caller: &MsgCaller,
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

    fn is_owner_or_creator(&self, caller: &MsgCaller) -> bool {
        caller.euid == self.uid || caller.euid == self.cuid || caller.can_override_ipc
    }

    fn current_bytes(&self) -> usize {
        perf::record_sysv_msg_current_bytes(0);
        self.cbytes
    }

    fn has_capacity_for(&self, len: usize) -> bool {
        self.current_bytes().saturating_add(len) <= self.qbytes && self.messages.len() < self.qbytes
    }

    fn stat(&self) -> MsgQueueStat {
        MsgQueueStat {
            key: self.key,
            uid: self.uid,
            gid: self.gid,
            cuid: self.cuid,
            cgid: self.cgid,
            mode: self.mode & 0o777,
            stime: self.stime,
            rtime: self.rtime,
            ctime: self.ctime,
            cbytes: self.current_bytes(),
            qnum: self.messages.len(),
            qbytes: self.qbytes,
            lspid: self.lspid,
            lrpid: self.lrpid,
        }
    }

    fn wake_waiters(&mut self) {
        for waiter in core::mem::take(&mut self.waiters) {
            wakeup_task(waiter.task);
        }
    }
}

struct MsgManager {
    next_id: usize,
    queues: BTreeMap<usize, MsgQueue>,
    keyed_queues: BTreeMap<isize, usize>,
}

impl MsgManager {
    fn new() -> Self {
        Self {
            next_id: 1,
            queues: BTreeMap::new(),
            keyed_queues: BTreeMap::new(),
        }
    }

    fn alloc_id(&mut self) -> usize {
        if let Some(id) = requested_next_id() {
            if !self.queues.contains_key(&id) {
                reset_next_id();
                return id;
            }
        }
        reset_next_id();
        while self.queues.contains_key(&self.next_id) {
            self.next_id += 1;
        }
        let id = self.next_id;
        self.next_id += 1;
        while self.queues.contains_key(&self.next_id) {
            self.next_id += 1;
        }
        id
    }

    fn get_or_create(
        &mut self,
        key: isize,
        msgflg: i32,
        context: MsgCreateContext,
        caller: &MsgCaller,
    ) -> Result<usize, MsgError> {
        let mode = (msgflg & 0o777) as u32;
        let flags = msgflg & !0o777;
        if flags & !(IPC_CREAT | IPC_EXCL) != 0 {
            return Err(MsgError::Invalid);
        }
        if key == IPC_PRIVATE {
            return self.create_queue(key, mode, context);
        }
        if let Some(msqid) = self.keyed_queues.get(&key).copied() {
            if flags & (IPC_CREAT | IPC_EXCL) == (IPC_CREAT | IPC_EXCL) {
                return Err(MsgError::Exists);
            }
            let queue = self.queues.get(&msqid).ok_or(MsgError::NotFound)?;
            if mode & 0o444 != 0 && !queue.can_read(caller) {
                return Err(MsgError::AccessDenied);
            }
            if mode & 0o222 != 0 && !queue.can_write(caller) {
                return Err(MsgError::AccessDenied);
            }
            return Ok(msqid);
        }
        if flags & IPC_CREAT == 0 {
            return Err(MsgError::NotFound);
        }
        self.create_queue(key, mode, context)
    }

    fn create_queue(
        &mut self,
        key: isize,
        mode: u32,
        context: MsgCreateContext,
    ) -> Result<usize, MsgError> {
        if self.queues.len() >= current_msgmni() {
            return Err(MsgError::NoSpace);
        }
        let msqid = self.alloc_id();
        self.queues.insert(msqid, MsgQueue::new(key, mode, context));
        if key != IPC_PRIVATE {
            self.keyed_queues.insert(key, msqid);
        }
        Ok(msqid)
    }

    fn remove_queue(&mut self, msqid: usize, caller: &MsgCaller) -> Result<(), MsgError> {
        {
            let queue = self.queues.get(&msqid).ok_or(MsgError::Invalid)?;
            if !queue.is_owner_or_creator(caller) {
                return Err(MsgError::NotPermitted);
            }
        }
        let mut queue = self.queues.remove(&msqid).ok_or(MsgError::Invalid)?;
        if queue.key != IPC_PRIVATE {
            self.keyed_queues.remove(&queue.key);
        }
        queue.wake_waiters();
        Ok(())
    }

    fn stat_by_id(&self, msqid: usize, caller: &MsgCaller) -> Result<MsgQueueStat, MsgError> {
        let queue = self.queues.get(&msqid).ok_or(MsgError::Invalid)?;
        if !queue.can_read(caller) {
            return Err(MsgError::AccessDenied);
        }
        Ok(queue.stat())
    }

    fn stat_by_index(
        &self,
        index: usize,
        caller: &MsgCaller,
        skip_permission: bool,
    ) -> Result<(usize, MsgQueueStat), MsgError> {
        let queue = self.queues.get(&index).ok_or(MsgError::Invalid)?;
        if !skip_permission && !queue.can_read(caller) {
            return Err(MsgError::AccessDenied);
        }
        Ok((index, queue.stat()))
    }

    fn set_attrs(
        &mut self,
        msqid: usize,
        attrs: MsgSetAttrs,
        caller: &MsgCaller,
    ) -> Result<(), MsgError> {
        let queue = self.queues.get_mut(&msqid).ok_or(MsgError::Invalid)?;
        if !queue.is_owner_or_creator(caller) {
            return Err(MsgError::NotPermitted);
        }
        queue.uid = attrs.uid;
        queue.gid = attrs.gid;
        queue.mode = attrs.mode & 0o777;
        queue.qbytes = attrs.qbytes.max(1);
        queue.ctime = now_sec();
        queue.wake_waiters();
        Ok(())
    }

    fn send(
        &mut self,
        msqid: usize,
        message: Message,
        flags: i32,
        caller: &MsgCaller,
    ) -> Result<(), MsgError> {
        if message.text.len() > current_msgmax() {
            return Err(MsgError::Invalid);
        }
        let queue = self.queues.get_mut(&msqid).ok_or(MsgError::Invalid)?;
        if !queue.can_write(caller) {
            return Err(MsgError::AccessDenied);
        }
        let message_len = message.text.len();
        if !queue.has_capacity_for(message_len) {
            if flags & IPC_NOWAIT != 0 {
                return Err(MsgError::WouldBlock);
            }
            return Err(MsgError::WouldBlock);
        }
        queue.cbytes = queue.cbytes.saturating_add(message_len);
        queue.messages.push(message);
        queue.lspid = pid_to_i32(caller.pid);
        queue.stime = now_sec();
        queue.wake_waiters();
        Ok(())
    }

    fn receive(
        &mut self,
        msqid: usize,
        msgtyp: isize,
        msgsz: usize,
        flags: i32,
        caller: &MsgCaller,
    ) -> Result<Message, MsgError> {
        let queue = self.queues.get_mut(&msqid).ok_or(MsgError::Invalid)?;
        if !queue.can_read(caller) {
            return Err(MsgError::AccessDenied);
        }
        let copy_mode = flags & MSG_COPY != 0;
        if copy_mode {
            if flags & IPC_NOWAIT == 0 || flags & MSG_EXCEPT != 0 || msgtyp < 0 {
                return Err(MsgError::Invalid);
            }
        }
        let Some(index) = find_message_index(&queue.messages, msgtyp, flags) else {
            if flags & IPC_NOWAIT != 0 || copy_mode {
                return Err(MsgError::NoMessage);
            }
            return Err(MsgError::WouldBlock);
        };
        let message = queue
            .messages
            .get(index)
            .cloned()
            .ok_or(MsgError::NoMessage)?;
        if message.text.len() > msgsz && flags & MSG_NOERROR == 0 {
            return Err(MsgError::TooBig);
        }
        if !copy_mode {
            queue.cbytes = queue.cbytes.saturating_sub(message.text.len());
            queue.messages.remove(index);
            queue.lrpid = pid_to_i32(caller.pid);
            queue.rtime = now_sec();
            queue.wake_waiters();
        }
        Ok(message)
    }

    fn block_current(&mut self, msqid: usize) -> Result<*mut TaskContext, MsgError> {
        let queue = self.queues.get_mut(&msqid).ok_or(MsgError::Invalid)?;
        let (task, task_cx_ptr) = block_current_task_no_schedule();
        // UNFINISHED: System V message queue sleeps should wake directly from
        // signal delivery or IPC_RMID. This timer is a bounded fallback for the
        // current wait queue model so LTP signal interruption cases do not
        // stick until the testcase alarm.
        add_timer(get_time_ms() + 1000, Arc::clone(&task));
        queue.waiters.push(MsgWaiter { task });
        Ok(task_cx_ptr)
    }

    fn remove_waiter_for_task(&mut self, task: &Arc<TaskControlBlock>) {
        for queue in self.queues.values_mut() {
            queue
                .waiters
                .retain(|waiter| !Arc::ptr_eq(&waiter.task, task));
        }
    }

    fn usage_info(&self) -> MsgUsageInfo {
        MsgUsageInfo {
            used_ids: self.queues.len(),
            total_messages: self.queues.values().map(|queue| queue.messages.len()).sum(),
            total_bytes: self.queues.values().map(|queue| queue.cbytes).sum(),
            highest_index: self.highest_index(),
        }
    }

    fn highest_index(&self) -> usize {
        self.queues.keys().next_back().copied().unwrap_or(0)
    }

    fn proc_sysvipc_msg_content(&self) -> String {
        let mut output = String::from(
            "       key      msqid perms      cbytes       qnum lspid lrpid   uid   gid  cuid  cgid      stime      rtime      ctime\n",
        );
        for (&msqid, queue) in &self.queues {
            let stat = queue.stat();
            output.push_str(&format!(
                "{:10} {:10} {:5o} {:11} {:10} {:5} {:5} {:5} {:5} {:5} {:5} {:10} {:10} {:10}\n",
                stat.key,
                msqid,
                stat.mode,
                stat.cbytes,
                stat.qnum,
                stat.lspid,
                stat.lrpid,
                stat.uid,
                stat.gid,
                stat.cuid,
                stat.cgid,
                stat.stime,
                stat.rtime,
                stat.ctime,
            ));
        }
        output
    }
}

fn find_message_index(messages: &[Message], msgtyp: isize, flags: i32) -> Option<usize> {
    if flags & MSG_COPY != 0 {
        return usize::try_from(msgtyp)
            .ok()
            .filter(|&index| index < messages.len());
    }
    if msgtyp == 0 {
        return (!messages.is_empty()).then_some(0);
    }
    if msgtyp > 0 {
        if flags & MSG_EXCEPT != 0 {
            return messages.iter().position(|message| message.mtype != msgtyp);
        }
        return messages.iter().position(|message| message.mtype == msgtyp);
    }
    let max_type = msgtyp.saturating_neg();
    messages
        .iter()
        .enumerate()
        .filter(|(_, message)| message.mtype <= max_type)
        .min_by_key(|(_, message)| message.mtype)
        .map(|(index, _)| index)
}

lazy_static! {
    static ref MSG_MANAGER: UPIntrFreeCell<MsgManager> =
        unsafe { UPIntrFreeCell::new(MsgManager::new()) };
}

pub(super) fn sys_msgget(key: isize, msgflg: i32) -> SysResult {
    let process = current_process();
    let credentials = process.credentials();
    let caller = msg_caller_from(process.getpid(), &credentials);
    let context = MsgCreateContext {
        uid: credentials.euid,
        gid: credentials.egid,
    };
    MSG_MANAGER
        .exclusive_access()
        .get_or_create(key, msgflg, context, &caller)
        .map(|msqid| msqid as isize)
        .map_err(msg_error_to_sys_error)
}

pub(super) fn sys_msgctl(msqid: usize, cmd: i32, buf: usize) -> SysResult {
    let process = current_process();
    let credentials = process.credentials();
    let caller = msg_caller_from(process.getpid(), &credentials);
    match cmd {
        IPC_RMID => {
            MSG_MANAGER
                .exclusive_access()
                .remove_queue(msqid, &caller)
                .map_err(msg_error_to_sys_error)?;
            Ok(0)
        }
        IPC_STAT => {
            let stat = MSG_MANAGER
                .exclusive_access()
                .stat_by_id(msqid, &caller)
                .map_err(msg_error_to_sys_error)?;
            write_msqid_ds(buf, stat)?;
            Ok(0)
        }
        IPC_SET => {
            let ds: LinuxMsqid64Ds =
                read_user_value(current_user_token(), buf as *const LinuxMsqid64Ds)?;
            MSG_MANAGER
                .exclusive_access()
                .set_attrs(
                    msqid,
                    MsgSetAttrs {
                        uid: ds.msg_perm.uid,
                        gid: ds.msg_perm.gid,
                        mode: ds.msg_perm.mode,
                        qbytes: ds.msg_qbytes,
                    },
                    &caller,
                )
                .map_err(msg_error_to_sys_error)?;
            Ok(0)
        }
        IPC_INFO | MSG_INFO => {
            let usage = MSG_MANAGER.exclusive_access().usage_info();
            let mut info = base_msginfo();
            if cmd == MSG_INFO {
                info.msgpool = usage.used_ids.try_into().unwrap_or(i32::MAX);
                info.msgmap = usage.total_messages.try_into().unwrap_or(i32::MAX);
                info.msgtql = usage.total_bytes.try_into().unwrap_or(i32::MAX);
            }
            write_user_value(current_user_token(), buf as *mut LinuxMsgInfo, &info)?;
            Ok(usage.highest_index as isize)
        }
        MSG_STAT | MSG_STAT_ANY => {
            let skip_permission = cmd == MSG_STAT_ANY;
            let (real_msqid, stat) = MSG_MANAGER
                .exclusive_access()
                .stat_by_index(msqid, &caller, skip_permission)
                .map_err(msg_error_to_sys_error)?;
            write_msqid_ds(buf, stat)?;
            Ok(real_msqid as isize)
        }
        _ => Err(SysError::EINVAL),
    }
}

pub(super) fn sys_msgsnd(msqid: usize, msgp: *const u8, msgsz: usize, msgflg: i32) -> SysResult {
    if (msgsz as isize) < 0 {
        return Err(SysError::EINVAL);
    }
    let token = current_user_token();
    let mtype: isize = read_user_value(token, msgp as *const isize)?;
    if mtype <= 0 {
        return Err(SysError::EINVAL);
    }
    let text = read_user_array(
        token,
        msgp.wrapping_add(core::mem::size_of::<isize>()),
        msgsz,
    )?;
    let process = current_process();
    let caller = msg_caller_from(process.getpid(), &process.credentials());
    let message = Message { mtype, text };
    loop {
        match try_or_block_msgsnd(msqid, message.clone(), msgflg, &caller)? {
            MsgAttempt::Done(_) => return Ok(0),
            MsgAttempt::Blocked(task_cx_ptr) => schedule(task_cx_ptr),
        }
        post_msg_sleep_cleanup(msqid)?;
    }
}

pub(super) fn sys_msgrcv(
    msqid: usize,
    msgp: *mut u8,
    msgsz: usize,
    msgtyp: isize,
    msgflg: i32,
) -> SysResult {
    if (msgsz as isize) < 0 {
        return Err(SysError::EINVAL);
    }
    let process = current_process();
    let caller = msg_caller_from(process.getpid(), &process.credentials());
    loop {
        match try_or_block_msgrcv(msqid, msgp, msgtyp, msgsz, msgflg, &caller)? {
            MsgAttempt::Done(len) => return Ok(len),
            MsgAttempt::Blocked(task_cx_ptr) => schedule(task_cx_ptr),
        }
        post_msg_sleep_cleanup(msqid)?;
    }
}

fn try_or_block_msgsnd(
    msqid: usize,
    message: Message,
    flags: i32,
    caller: &MsgCaller,
) -> Result<MsgAttempt, SysError> {
    let mut manager = MSG_MANAGER.exclusive_access();
    match manager.send(msqid, message, flags, caller) {
        Ok(()) => Ok(MsgAttempt::Done(0)),
        Err(MsgError::WouldBlock) if flags & IPC_NOWAIT == 0 => manager
            .block_current(msqid)
            .map(MsgAttempt::Blocked)
            .map_err(msg_error_to_sys_error),
        Err(error) => Err(msg_error_to_sys_error(error)),
    }
}

fn try_or_block_msgrcv(
    msqid: usize,
    msgp: *mut u8,
    msgtyp: isize,
    msgsz: usize,
    flags: i32,
    caller: &MsgCaller,
) -> Result<MsgAttempt, SysError> {
    let mut manager = MSG_MANAGER.exclusive_access();
    match manager.receive(msqid, msgtyp, msgsz, flags, caller) {
        Ok(message) => {
            let copy_len = msgsz.min(message.text.len());
            write_message_to_user(msgp, &message, copy_len)?;
            Ok(MsgAttempt::Done(copy_len as isize))
        }
        Err(MsgError::WouldBlock) => manager
            .block_current(msqid)
            .map(MsgAttempt::Blocked)
            .map_err(msg_error_to_sys_error),
        Err(error) => Err(msg_error_to_sys_error(error)),
    }
}

fn post_msg_sleep_cleanup(msqid: usize) -> SysResult<()> {
    let Some(task) = current_task() else {
        return Err(SysError::EINTR);
    };
    MSG_MANAGER.exclusive_access().remove_waiter_for_task(&task);
    if let Some((exit_code, _message)) = check_signals_of_current() {
        exit_current_group_and_run_next(exit_code);
    }
    if current_has_deliverable_signal() {
        return Err(SysError::EINTR);
    }
    if !MSG_MANAGER.exclusive_access().queues.contains_key(&msqid) {
        return Err(SysError::EIDRM);
    }
    Ok(())
}

fn write_message_to_user(msgp: *mut u8, message: &Message, copy_len: usize) -> SysResult<isize> {
    let token = current_user_token();
    write_user_value(token, msgp as *mut isize, &message.mtype)?;
    copy_to_user(
        token,
        msgp.wrapping_add(core::mem::size_of::<isize>()),
        &message.text[..copy_len],
    )?;
    Ok(copy_len as isize)
}

fn write_msqid_ds(buf: usize, stat: MsgQueueStat) -> SysResult<()> {
    let ds = LinuxMsqid64Ds {
        msg_perm: LinuxIpc64Perm {
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
        msg_stime: stat.stime,
        msg_rtime: stat.rtime,
        msg_ctime: stat.ctime,
        msg_cbytes: stat.cbytes,
        msg_qnum: stat.qnum,
        msg_qbytes: stat.qbytes,
        msg_lspid: stat.lspid,
        msg_lrpid: stat.lrpid,
        unused4: 0,
        unused5: 0,
    };
    write_user_value(current_user_token(), buf as *mut LinuxMsqid64Ds, &ds)
}

fn base_msginfo() -> LinuxMsgInfo {
    LinuxMsgInfo {
        msgpool: current_msgmni().try_into().unwrap_or(i32::MAX),
        msgmap: current_msgmni().try_into().unwrap_or(i32::MAX),
        msgmax: current_msgmax().try_into().unwrap_or(i32::MAX),
        msgmnb: current_msgmnb().try_into().unwrap_or(i32::MAX),
        msgmni: current_msgmni().try_into().unwrap_or(i32::MAX),
        msgssz: 16,
        msgtql: (current_msgmni().saturating_mul(64))
            .try_into()
            .unwrap_or(i32::MAX),
        msgseg: u16::MAX,
    }
}

pub(crate) fn proc_sysvipc_msg_content() -> String {
    MSG_MANAGER.exclusive_access().proc_sysvipc_msg_content()
}

pub(crate) fn current_msgmni() -> usize {
    MSGMNI.load(Ordering::Relaxed)
}

pub(crate) fn current_msgmax() -> usize {
    MSGMAX.load(Ordering::Relaxed)
}

pub(crate) fn current_msgmnb() -> usize {
    MSGMNB.load(Ordering::Relaxed)
}

pub(crate) fn current_msg_next_id() -> isize {
    MSG_NEXT_ID.load(Ordering::Relaxed)
}

pub(crate) fn set_msgmni(value: usize) -> bool {
    if value == 0 {
        return false;
    }
    MSGMNI.store(value, Ordering::Relaxed);
    true
}

pub(crate) fn set_msgmax(value: usize) -> bool {
    if value == 0 {
        return false;
    }
    MSGMAX.store(value, Ordering::Relaxed);
    true
}

pub(crate) fn set_msgmnb(value: usize) -> bool {
    if value == 0 {
        return false;
    }
    MSGMNB.store(value, Ordering::Relaxed);
    true
}

pub(crate) fn set_msg_next_id(value: isize) -> bool {
    if value < -1 {
        return false;
    }
    MSG_NEXT_ID.store(value, Ordering::Relaxed);
    true
}

fn requested_next_id() -> Option<usize> {
    MSG_NEXT_ID.load(Ordering::Relaxed).try_into().ok()
}

fn reset_next_id() {
    MSG_NEXT_ID.store(-1, Ordering::Relaxed);
}

fn msg_caller_from(pid: usize, credentials: &crate::task::Credentials) -> MsgCaller {
    MsgCaller {
        pid,
        euid: credentials.euid,
        egid: credentials.egid,
        groups: credentials.groups.clone(),
        can_override_ipc: credentials.euid == 0,
    }
}

fn msg_error_to_sys_error(error: MsgError) -> SysError {
    match error {
        MsgError::NotFound => SysError::ENOENT,
        MsgError::Exists => SysError::EEXIST,
        MsgError::Invalid => SysError::EINVAL,
        MsgError::NoSpace => SysError::ENOSPC,
        MsgError::AccessDenied => SysError::EACCES,
        MsgError::NotPermitted => SysError::EPERM,
        MsgError::TooBig => SysError::E2BIG,
        MsgError::NoMessage => SysError::ENOMSG,
        MsgError::WouldBlock => SysError::EAGAIN,
    }
}

fn now_sec() -> i64 {
    (crate::timer::wall_time_nanos() / 1_000_000_000) as i64
}

fn pid_to_i32(pid: usize) -> i32 {
    pid.try_into().unwrap_or(i32::MAX)
}
