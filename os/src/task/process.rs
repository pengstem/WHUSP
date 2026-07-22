use super::id::RecycleAllocator;
use super::{
    CloneFlags, FD_LIMIT, FdTableEntry, PidHandle, SIGNAL_INFO_SLOTS, SignalAction,
    TaskControlBlock, TaskStatus, wakeup_task,
};
use crate::config::USER_STACK_SIZE;
use crate::fs::{MountNamespaceId, PathContext, ROOT_MOUNT_NAMESPACE, VfsNodeId, WorkingDir};
use crate::mm::MemorySet;
use crate::perf;
use crate::sync::{UPIntrFreeCell, UPIntrRefMut};
use alloc::format;
use alloc::string::String;
use alloc::sync::{Arc, Weak};
use alloc::{vec, vec::Vec};
use core::ops::{Deref, DerefMut};
use core::sync::atomic::{AtomicUsize, Ordering};

pub const RLIM_INFINITY: usize = usize::MAX;
const RLIMIT_COUNT: usize = RLimitResource::RtTime as usize + 1;
const FD_BITMAP_WORD_BITS: usize = usize::BITS as usize;
pub(crate) const PROCESS_PKEY_COUNT: usize = 16;
pub(crate) type ProcessPKeyRights = [Option<usize>; PROCESS_PKEY_COUNT];
type TimerRearm = Option<(usize, u64)>;
type RealTimerExpiry = (Arc<TaskControlBlock>, TimerRearm);
type PosixTimerExpiry = (Arc<TaskControlBlock>, u32, TimerRearm);

pub(crate) fn empty_process_pkey_rights() -> ProcessPKeyRights {
    [None; PROCESS_PKEY_COUNT]
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RLimit {
    pub rlim_cur: usize,
    pub rlim_max: usize,
}

impl RLimit {
    const fn fixed(value: usize) -> Self {
        Self {
            rlim_cur: value,
            rlim_max: value,
        }
    }

    const fn soft_with_hard(soft: usize, hard: usize) -> Self {
        Self {
            rlim_cur: soft,
            rlim_max: hard,
        }
    }

    const fn infinity() -> Self {
        Self {
            rlim_cur: RLIM_INFINITY,
            rlim_max: RLIM_INFINITY,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(usize)]
pub enum RLimitResource {
    Cpu = 0,
    FSize = 1,
    Data = 2,
    Stack = 3,
    Core = 4,
    Rss = 5,
    NProc = 6,
    NoFile = 7,
    MemLock = 8,
    As = 9,
    Locks = 10,
    SigPending = 11,
    MsgQueue = 12,
    Nice = 13,
    RtPrio = 14,
    RtTime = 15,
}

impl RLimitResource {
    /// Decodes the Linux `RLIMIT_*` resource number used by rlimit syscalls.
    pub fn from_raw(resource: i32) -> Option<Self> {
        match resource {
            0 => Some(Self::Cpu),
            1 => Some(Self::FSize),
            2 => Some(Self::Data),
            3 => Some(Self::Stack),
            4 => Some(Self::Core),
            5 => Some(Self::Rss),
            6 => Some(Self::NProc),
            7 => Some(Self::NoFile),
            8 => Some(Self::MemLock),
            9 => Some(Self::As),
            10 => Some(Self::Locks),
            11 => Some(Self::SigPending),
            12 => Some(Self::MsgQueue),
            13 => Some(Self::Nice),
            14 => Some(Self::RtPrio),
            15 => Some(Self::RtTime),
            _ => None,
        }
    }

    const fn index(self) -> usize {
        self as usize
    }
}

#[derive(Clone, Copy, Debug)]
pub struct ProcessResourceLimits {
    limits: [RLimit; RLIMIT_COUNT],
}

impl ProcessResourceLimits {
    pub fn new() -> Self {
        // UNFINISHED: Except RLIMIT_NOFILE, the mlock-visible
        // RLIMIT_MEMLOCK subset, and the RLIMIT_CORE signal-status bit,
        // these limits are currently stored for getrlimit/setrlimit
        // compatibility but are not enforced by the memory, scheduler, or
        // fork paths yet.
        let mut limits = [RLimit::infinity(); RLIMIT_COUNT];
        limits[RLimitResource::Stack.index()] =
            RLimit::soft_with_hard(USER_STACK_SIZE, RLIM_INFINITY);
        limits[RLimitResource::NoFile.index()] = RLimit::fixed(FD_LIMIT);
        Self { limits }
    }

    pub fn get(&self, resource: RLimitResource) -> RLimit {
        self.limits[resource.index()]
    }

    pub fn set(&mut self, resource: RLimitResource, limit: RLimit) {
        self.limits[resource.index()] = limit;
    }
}

impl Default for ProcessResourceLimits {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct ProcessCpuTimesSnapshot {
    pub user_us: usize,
    pub system_us: usize,
    pub children_user_us: usize,
    pub children_system_us: usize,
    pub self_maxrss_kb: usize,
    pub children_maxrss_kb: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CapabilitySets {
    // Linux capability syscalls expose two u32 words for v2/v3 headers. Keep
    // the storage in that ABI shape so capget/capset can copy fields without
    // re-encoding every call.
    pub effective: [u32; 2],
    pub permitted: [u32; 2],
    pub inheritable: [u32; 2],
    pub bounding: [u32; 2],
    pub ambient: [u32; 2],
}

impl CapabilitySets {
    pub const CAP_SETPCAP: usize = 8;
    pub const CAP_IPC_LOCK: usize = 14;
    pub const CAP_IPC_OWNER: usize = 15;
    pub const CAP_SYS_CHROOT: usize = 18;
    pub const CAP_SYS_PTRACE: usize = 19;
    pub const CAP_SYS_ADMIN: usize = 21;
    pub const CAP_SYS_RESOURCE: usize = 24;
    pub const CAP_SYS_TIME: usize = 25;
    pub const CAP_SYS_TTY_CONFIG: usize = 26;
    pub const CAP_LAST_CAP: usize = 40;

    fn all_known_bits() -> [u32; 2] {
        let high_bits = Self::CAP_LAST_CAP + 1 - u32::BITS as usize;
        [u32::MAX, (1u32 << high_bits) - 1]
    }

    fn cap_bit(cap: usize) -> Option<(usize, u32)> {
        if cap > Self::CAP_LAST_CAP {
            return None;
        }
        Some((cap / u32::BITS as usize, 1u32 << (cap % u32::BITS as usize)))
    }

    pub fn root() -> Self {
        let all = Self::all_known_bits();
        Self {
            effective: all,
            permitted: all,
            inheritable: [0; 2],
            bounding: all,
            ambient: [0; 2],
        }
    }

    pub fn has_effective(&self, cap: usize) -> Option<bool> {
        let (index, mask) = Self::cap_bit(cap)?;
        Some(self.effective[index] & mask != 0)
    }

    pub fn has_permitted(&self, cap: usize) -> Option<bool> {
        let (index, mask) = Self::cap_bit(cap)?;
        Some(self.permitted[index] & mask != 0)
    }

    pub fn has_inheritable(&self, cap: usize) -> Option<bool> {
        let (index, mask) = Self::cap_bit(cap)?;
        Some(self.inheritable[index] & mask != 0)
    }

    pub fn ambient_contains(&self, cap: usize) -> Option<bool> {
        let (index, mask) = Self::cap_bit(cap)?;
        Some(self.ambient[index] & mask != 0)
    }

    pub fn raise_ambient(&mut self, cap: usize) -> Option<()> {
        let (index, mask) = Self::cap_bit(cap)?;
        self.ambient[index] |= mask;
        Some(())
    }

    pub fn lower_ambient(&mut self, cap: usize) -> Option<()> {
        let (index, mask) = Self::cap_bit(cap)?;
        self.ambient[index] &= !mask;
        Some(())
    }

    pub fn clear_ambient(&mut self) {
        self.ambient = [0; 2];
    }

    pub fn clamp_ambient_to_permitted_inheritable(&mut self) {
        for index in 0..self.ambient.len() {
            self.ambient[index] &= self.permitted[index] & self.inheritable[index];
        }
    }

    pub fn bounding_contains(&self, cap: usize) -> Option<bool> {
        let (index, mask) = Self::cap_bit(cap)?;
        Some(self.bounding[index] & mask != 0)
    }

    pub fn drop_bounding(&mut self, cap: usize) -> Option<()> {
        let (index, mask) = Self::cap_bit(cap)?;
        self.bounding[index] &= !mask;
        Some(())
    }
}

impl Default for CapabilitySets {
    fn default() -> Self {
        Self::root()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Credentials {
    pub ruid: u32,
    pub euid: u32,
    pub suid: u32,
    pub fsuid: u32,
    pub rgid: u32,
    pub egid: u32,
    pub sgid: u32,
    pub fsgid: u32,
    pub groups: Vec<u32>,
    pub capabilities: CapabilitySets,
}

impl Credentials {
    pub fn root() -> Self {
        Self {
            ruid: 0,
            euid: 0,
            suid: 0,
            fsuid: 0,
            rgid: 0,
            egid: 0,
            sgid: 0,
            fsgid: 0,
            groups: Vec::new(),
            capabilities: CapabilitySets::root(),
        }
    }

    pub fn is_root(&self) -> bool {
        self.euid == 0
    }

    pub fn uid_matches_saved_set(&self, uid: u32) -> bool {
        uid == self.ruid || uid == self.euid || uid == self.suid
    }

    pub fn gid_matches_saved_set(&self, gid: u32) -> bool {
        gid == self.rgid || gid == self.egid || gid == self.sgid
    }
}

impl Default for Credentials {
    fn default() -> Self {
        Self::root()
    }
}

#[derive(Clone, Debug)]
pub(crate) struct ProcessProcSnapshot {
    pub(crate) pid: usize,
    pub(crate) ppid: usize,
    pub(crate) pgid: usize,
    pub(crate) comm: String,
    pub(crate) state: char,
    pub(crate) executable_node: Option<VfsNodeId>,
    pub(crate) executable_path: String,
    pub(crate) cmdline: Vec<String>,
    pub(crate) cpu_times: ProcessCpuTimesSnapshot,
    pub(crate) credentials: Credentials,
    pub(crate) thread_count: usize,
    pub(crate) mount_namespace_id: MountNamespaceId,
    pub(crate) pid_namespace_id: usize,
    pub(crate) pid_namespace_parent_id: Option<usize>,
    pub(crate) user_namespace_id: usize,
    pub(crate) user_namespace_parent_id: Option<usize>,
    pub(crate) resident_kb: usize,
    pub(crate) locked_kb: usize,
    pub(crate) no_new_privs: bool,
    pub(crate) timer_slack_ns: usize,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct ProcessNamespace {
    pub(crate) id: usize,
    pub(crate) parent_id: Option<usize>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct KcmpResourceOwners {
    pub(crate) vm: usize,
    pub(crate) files: usize,
    pub(crate) fs: usize,
    pub(crate) sighand: usize,
    pub(crate) io: usize,
    pub(crate) sysvsem: usize,
}

impl KcmpResourceOwners {
    pub(crate) fn new(process_id: usize) -> Self {
        Self {
            vm: process_id,
            files: process_id,
            fs: process_id,
            sighand: process_id,
            io: process_id,
            sysvsem: process_id,
        }
    }

    pub(crate) fn forked(self, process_id: usize, flags: CloneFlags) -> Self {
        Self {
            vm: if flags.contains(CloneFlags::CLONE_VM) {
                self.vm
            } else {
                process_id
            },
            files: if flags.contains(CloneFlags::CLONE_FILES) {
                self.files
            } else {
                process_id
            },
            fs: if flags.contains(CloneFlags::CLONE_FS) {
                self.fs
            } else {
                process_id
            },
            sighand: if flags.contains(CloneFlags::CLONE_SIGHAND) {
                self.sighand
            } else {
                process_id
            },
            io: if flags.contains(CloneFlags::CLONE_IO) {
                self.io
            } else {
                process_id
            },
            sysvsem: if flags.contains(CloneFlags::CLONE_SYSVSEM) {
                self.sysvsem
            } else {
                process_id
            },
        }
    }
}

fn proc_task_state(status: TaskStatus, proc_sleeping: bool) -> char {
    if proc_sleeping {
        return 'S';
    }
    match status {
        TaskStatus::Ready | TaskStatus::Running => 'R',
        TaskStatus::Blocked => 'S',
        TaskStatus::Exited => 'Z',
    }
}

#[derive(Clone, Debug)]
pub(crate) struct PathSnapshot {
    pub(crate) context: PathContext,
    pub(crate) cwd_path: String,
    pub(crate) root_path: String,
}

#[derive(Clone, Debug)]
pub(crate) struct ProcessFsContext {
    // `WorkingDir` is the VFS anchor; the parallel string is the Linux-visible
    // path snapshot used by getcwd/procfs and relative-path reconstruction.
    // Keep each pair synchronized when chdir/chroot/fchdir updates either side.
    root: WorkingDir,
    root_path: String,
    cwd: WorkingDir,
    cwd_path: String,
    mount_namespace_id: MountNamespaceId,
}

impl ProcessFsContext {
    /// Builds the initial filesystem view for PID 1.
    pub(crate) fn root() -> Self {
        Self {
            root: WorkingDir::root(),
            root_path: "/".into(),
            cwd: WorkingDir::root(),
            cwd_path: "/".into(),
            mount_namespace_id: ROOT_MOUNT_NAMESPACE,
        }
    }

    /// Clones the path state for fork while installing the requested namespace.
    pub(crate) fn forked(&self, mount_namespace_id: MountNamespaceId) -> Self {
        Self {
            root: self.root,
            root_path: self.root_path.clone(),
            cwd: self.cwd,
            cwd_path: self.cwd_path.clone(),
            mount_namespace_id,
        }
    }

    fn path_context(&self) -> PathContext {
        PathContext::new_in_namespace(
            self.root,
            self.cwd,
            self.mount_namespace_id,
            self.root_path.clone(),
            self.cwd_path.clone(),
        )
    }

    fn set_working_dir(&mut self, cwd: WorkingDir, cwd_path: String) {
        self.cwd = cwd;
        self.cwd_path = cwd_path;
    }

    fn set_root_dir(&mut self, root: WorkingDir, root_path: String) {
        self.root = root;
        self.root_path = root_path;
    }

    fn references_mount(&self, mount_id: crate::fs::MountId) -> bool {
        self.root.mount_id() == mount_id || self.cwd.mount_id() == mount_id
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct ProcessCpuTimes {
    // UNFINISHED: CPU accounting is process-wide and trap-boundary based;
    // exact per-thread aggregation, scheduler tick attribution, and
    // signal/job-control resource accounting are not modeled yet.
    user_us: usize,
    system_us: usize,
    children_user_us: usize,
    children_system_us: usize,
    self_maxrss_kb: usize,
    children_maxrss_kb: usize,
    last_user_enter_us: Option<usize>,
    last_kernel_enter_us: Option<usize>,
}

impl ProcessCpuTimes {
    pub fn with_inherited_self_maxrss(self_maxrss_kb: usize) -> Self {
        Self {
            self_maxrss_kb,
            ..Self::default()
        }
    }

    pub fn mark_user_entry(&mut self, now_us: usize) {
        self.last_user_enter_us = Some(now_us);
        self.last_kernel_enter_us = None;
    }

    pub fn mark_kernel_entry(&mut self, now_us: usize) {
        self.last_kernel_enter_us = Some(now_us);
        self.last_user_enter_us = None;
    }

    pub fn account_user_until(&mut self, now_us: usize) {
        if let Some(start_us) = self.last_user_enter_us.take() {
            self.user_us = self.user_us.saturating_add(now_us.saturating_sub(start_us));
        }
        self.last_kernel_enter_us = Some(now_us);
    }

    pub fn account_system_until(&mut self, now_us: usize) {
        if let Some(start_us) = self.last_kernel_enter_us.take() {
            self.system_us = self
                .system_us
                .saturating_add(now_us.saturating_sub(start_us));
        }
        self.last_kernel_enter_us = Some(now_us);
    }

    pub fn add_waited_child(&mut self, child: ProcessCpuTimesSnapshot) {
        self.children_user_us = self
            .children_user_us
            .saturating_add(child.user_us)
            .saturating_add(child.children_user_us);
        self.children_system_us = self
            .children_system_us
            .saturating_add(child.system_us)
            .saturating_add(child.children_system_us);
        self.children_maxrss_kb = self
            .children_maxrss_kb
            .max(child.self_maxrss_kb.max(child.children_maxrss_kb));
    }

    pub fn record_resident_kb(&mut self, resident_kb: usize) {
        self.self_maxrss_kb = self.self_maxrss_kb.max(resident_kb);
    }

    pub fn snapshot(&self) -> ProcessCpuTimesSnapshot {
        ProcessCpuTimesSnapshot {
            user_us: self.user_us,
            system_us: self.system_us,
            children_user_us: self.children_user_us,
            children_system_us: self.children_system_us,
            self_maxrss_kb: self.self_maxrss_kb,
            children_maxrss_kb: self.children_maxrss_kb,
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct ProcessRealTimer {
    pub(crate) interval_us: usize,
    pub(crate) next_expire_us: usize,
    // Incremented on every set; timer heaps carry a snapshot so stale events
    // left behind by rearming or disarming are ignored at interrupt time.
    pub(crate) generation: u64,
}

impl ProcessRealTimer {
    pub(crate) fn is_armed(&self) -> bool {
        self.next_expire_us != 0
    }

    pub(crate) fn remaining_us(&self, now_us: usize) -> usize {
        if self.is_armed() {
            self.next_expire_us.saturating_sub(now_us)
        } else {
            0
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct ProcessPosixTimer {
    pub(crate) clock_id: i32,
    pub(crate) signal: u32,
    pub(crate) interval_us: usize,
    pub(crate) next_expire_us: usize,
    // Mirrors ProcessRealTimer::generation for POSIX timer ids, where delete
    // and settime can leave older heap events pending after the slot changes.
    pub(crate) generation: u64,
}

impl ProcessPosixTimer {
    pub(crate) fn new(clock_id: i32, signal: u32) -> Self {
        Self {
            clock_id,
            signal,
            ..Self::default()
        }
    }

    pub(crate) fn is_armed(&self) -> bool {
        self.next_expire_us != 0
    }

    pub(crate) fn remaining_us(&self, now_us: usize) -> usize {
        if self.is_armed() {
            self.next_expire_us.saturating_sub(now_us)
        } else {
            0
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct PtraceSyscallStop {
    pub(crate) op: u8,
    pub(crate) nr: usize,
    pub(crate) args: [usize; 6],
    pub(crate) rval: isize,
    pub(crate) is_error: bool,
    pub(crate) instruction_pointer: usize,
    pub(crate) stack_pointer: usize,
}

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct PtraceState {
    pub(crate) tracer_pid: Option<usize>,
    pub(crate) stopped: bool,
    pub(crate) stop_signal: Option<u32>,
    pub(crate) wait_stop_status: Option<i32>,
    pub(crate) options: usize,
    pub(crate) syscall_trace: bool,
    pub(crate) syscall_stop: Option<PtraceSyscallStop>,
}

#[derive(Debug, Default)]
pub(crate) struct ProcessTimers {
    pub(crate) real: ProcessRealTimer,
    pub(crate) virtual_timer: ProcessRealTimer,
    pub(crate) prof: ProcessRealTimer,
    pub(crate) posix: Vec<Option<ProcessPosixTimer>>,
}

impl ProcessTimers {
    pub(crate) fn clear_posix_after_exec(&mut self) {
        self.posix.clear();
    }
}

pub struct ProcessControlBlock {
    // immutable
    pub pid: PidHandle,
    pub(super) running_tasks: AtomicUsize,
    pub(super) exclusive_task: AtomicUsize,
    pub(super) inner_owner_cpu: AtomicUsize,
    // mutable
    pub(super) inner: UPIntrFreeCell<ProcessControlBlockInner>,
}

const NO_EXCLUSIVE_TASK: usize = 0;
const NO_INNER_OWNER: usize = usize::MAX;

pub(crate) struct TaskGroupSchedulerGuard<'a> {
    process: &'a ProcessControlBlock,
    task_id: usize,
}

impl Drop for TaskGroupSchedulerGuard<'_> {
    fn drop(&mut self) {
        self.process
            .exclusive_task
            .compare_exchange(
                self.task_id,
                NO_EXCLUSIVE_TASK,
                Ordering::Release,
                Ordering::Relaxed,
            )
            .unwrap_or_else(|owner| {
                panic!(
                    "task-group scheduler owner mismatch: expected={:#x} actual={owner:#x}",
                    self.task_id
                )
            });
        crate::cpu::wake_scheduler_cpu(crate::cpu::online_mask());
    }
}

pub struct ProcessInnerGuard<'a> {
    process: &'a ProcessControlBlock,
    inner: Option<UPIntrRefMut<'a, ProcessControlBlockInner>>,
}

impl Deref for ProcessInnerGuard<'_> {
    type Target = ProcessControlBlockInner;

    fn deref(&self) -> &Self::Target {
        self.inner.as_ref().expect("process inner guard released")
    }
}

impl DerefMut for ProcessInnerGuard<'_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.inner.as_mut().expect("process inner guard released")
    }
}

impl Drop for ProcessInnerGuard<'_> {
    fn drop(&mut self) {
        let cpu = crate::cpu::current_id();
        self.process
            .inner_owner_cpu
            .compare_exchange(cpu, NO_INNER_OWNER, Ordering::Release, Ordering::Relaxed)
            .unwrap_or_else(|owner| {
                panic!("process inner owner mismatch: expected={cpu} actual={owner}")
            });
        drop(self.inner.take());
    }
}

impl ProcessControlBlock {
    fn scheduler_task_id(task: &TaskControlBlock) -> usize {
        task as *const TaskControlBlock as usize
    }

    pub(crate) fn try_claim_scheduler_task(&self, task: &TaskControlBlock) -> bool {
        let task_id = Self::scheduler_task_id(task);
        let exclusive_task = self.exclusive_task.load(Ordering::Acquire);
        if exclusive_task != NO_EXCLUSIVE_TASK && exclusive_task != task_id {
            return false;
        }
        self.running_tasks.fetch_add(1, Ordering::AcqRel);
        let exclusive_task = self.exclusive_task.load(Ordering::Acquire);
        if exclusive_task == NO_EXCLUSIVE_TASK || exclusive_task == task_id {
            return true;
        }
        let previous = self.running_tasks.fetch_sub(1, Ordering::AcqRel);
        assert_ne!(previous, 0, "process running-task count underflow");
        false
    }

    pub(crate) fn release_scheduler_task(&self, task: &TaskControlBlock) {
        let previous = self.running_tasks.fetch_sub(1, Ordering::AcqRel);
        assert_ne!(previous, 0, "process running-task count underflow");
        let exclusive_task = self.exclusive_task.load(Ordering::Acquire);
        let task_id = Self::scheduler_task_id(task);
        assert!(
            exclusive_task == NO_EXCLUSIVE_TASK || exclusive_task == task_id || previous > 1,
            "non-owner task was the final runner under task-group exclusion"
        );
    }

    fn try_begin_scheduler_exclusion<'a>(
        &'a self,
        task: &TaskControlBlock,
    ) -> Option<TaskGroupSchedulerGuard<'a>> {
        let task_id = Self::scheduler_task_id(task);
        self.exclusive_task
            .compare_exchange(
                NO_EXCLUSIVE_TASK,
                task_id,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .ok()?;

        while self.running_tasks.load(Ordering::Acquire) != 1 {
            crate::task::suspend_current_and_run_next();
        }
        Some(TaskGroupSchedulerGuard {
            process: self,
            task_id,
        })
    }

    pub(crate) fn begin_exec_exclusion<'a>(
        &'a self,
        task: &TaskControlBlock,
    ) -> crate::syscall::errno::SysResult<TaskGroupSchedulerGuard<'a>> {
        self.try_begin_scheduler_exclusion(task)
            .ok_or(crate::syscall::errno::SysError::EAGAIN)
    }

    pub(crate) fn begin_group_exit_exclusion<'a>(
        &'a self,
        task: &TaskControlBlock,
    ) -> TaskGroupSchedulerGuard<'a> {
        loop {
            if let Some(guard) = self.try_begin_scheduler_exclusion(task) {
                return guard;
            }
            // A competing exec/group-exit owner needs this task to leave its
            // CPU before it can tear down the thread group. If that owner
            // removes us, this yield never returns; otherwise retry after the
            // earlier exclusive operation completes.
            crate::task::suspend_current_and_run_next();
        }
    }
}

pub struct ProcessControlBlockInner {
    pub is_zombie: bool,
    pub memory_set: MemorySet,
    pub executable_node: Option<VfsNodeId>,
    pub executable_path: String,
    pub(crate) fs: ProcessFsContext,
    pub cmdline: Vec<String>,
    pub pgid: usize,
    pub exit_signal: u32,
    pub parent: Option<Weak<ProcessControlBlock>>,
    pub children: Vec<Arc<ProcessControlBlock>>,
    pub exit_code: i32,
    pub fd_table: Vec<Option<FdTableEntry>>,
    pub(crate) fd_open_bits: Vec<usize>,
    pub(crate) next_fd_hint: usize,
    pub umask: u32,
    pub(crate) io_priority: u16,
    pub(crate) comm: String,
    pub(crate) pdeath_signal: u32,
    pub(crate) dumpable: bool,
    pub(crate) securebits: u32,
    pub(crate) is_child_subreaper: bool,
    pub(crate) no_new_privs: bool,
    pub(crate) thp_disabled: bool,
    pub(crate) personality: u32,
    pub(crate) wait_stop_status: Option<i32>,
    pub(crate) wait_continued: bool,
    pub(crate) ptrace: PtraceState,
    // UNFINISHED: Linux kernel credentials are per-thread, while POSIX
    // user-space expects process-wide synchronization. This first contest
    // compatibility model keeps credentials on the PCB and shares them across
    // all threads in the process.
    pub credentials: Credentials,
    pub resource_limits: ProcessResourceLimits,
    pub(crate) process_keyring: Option<i32>,
    pub(crate) session_keyring: Option<i32>,
    pub(crate) reqkey_default: usize,
    pub(crate) pkey_rights: ProcessPKeyRights,
    pub membarrier_private_expedited_registered: bool,
    pub signal_actions: [SignalAction; SIGNAL_INFO_SLOTS],
    pub cpu_times: ProcessCpuTimes,
    pub(crate) timers: ProcessTimers,
    pub(crate) vfork_parent: Option<Arc<TaskControlBlock>>,
    pub(crate) pid_namespace_id: usize,
    pub(crate) pid_namespace_parent_id: Option<usize>,
    pub(crate) user_namespace_id: usize,
    pub(crate) user_namespace_parent_id: Option<usize>,
    pub(crate) kcmp_resources: KcmpResourceOwners,
    pub tasks: Vec<Option<Arc<TaskControlBlock>>>,
    pub task_res_allocator: RecycleAllocator,
}

fn fd_bitmap_word_count(slot_count: usize) -> usize {
    if slot_count == 0 {
        0
    } else {
        (slot_count - 1) / FD_BITMAP_WORD_BITS + 1
    }
}

fn fd_bit_position(fd: usize) -> (usize, usize) {
    (
        fd / FD_BITMAP_WORD_BITS,
        1usize << (fd % FD_BITMAP_WORD_BITS),
    )
}

pub(crate) fn fd_allocation_state_from_table(
    fd_table: &[Option<FdTableEntry>],
) -> (Vec<usize>, usize) {
    let mut fd_open_bits = vec![0; fd_bitmap_word_count(fd_table.len())];
    let mut next_fd_hint = fd_table.len();

    for (fd, entry) in fd_table.iter().enumerate() {
        if entry.is_some() {
            let (word, bit) = fd_bit_position(fd);
            fd_open_bits[word] |= bit;
        } else if next_fd_hint == fd_table.len() {
            next_fd_hint = fd;
        }
    }

    (fd_open_bits, next_fd_hint)
}

impl ProcessControlBlockInner {
    pub fn get_user_token(&self) -> usize {
        self.memory_set.token()
    }

    pub fn nofile_limit(&self) -> usize {
        self.resource_limits
            .get(RLimitResource::NoFile)
            .rlim_cur
            .min(FD_LIMIT)
    }

    /// Finds the next fd number without installing an entry.
    ///
    /// Callers that need rollback-safe publication must keep the allocation and
    /// final `set_fd_entry` ordering explicit; this helper does not reserve the
    /// slot in the table. The bitmap is only a mirror of `fd_table`; updates
    /// must stay paired with install/take paths so `RLIMIT_NOFILE` searches do
    /// not observe stale open descriptors.
    pub fn alloc_fd_from(&mut self, lower_bound: usize) -> Option<usize> {
        let limit = self.nofile_limit();
        if lower_bound >= limit {
            perf::record_fd_alloc(0, 0, self.fd_table.len(), false);
            return None;
        }
        let search_start = lower_bound.max(self.next_fd_hint);
        let search_end = self.fd_table.len().min(limit);
        let mut bitmap_word_probes = 0usize;
        if let Some(fd) =
            self.find_free_fd_in_bitmap(search_start, search_end, &mut bitmap_word_probes)
        {
            perf::record_fd_bitmap_word_probes(bitmap_word_probes);
            perf::record_fd_alloc(0, 0, self.fd_table.len(), true);
            return Some(fd);
        }
        let fd = self.fd_table.len().max(search_start);
        if fd >= limit {
            perf::record_fd_bitmap_word_probes(bitmap_word_probes);
            perf::record_fd_alloc(0, 0, self.fd_table.len(), false);
            return None;
        }
        let old_len = self.fd_table.len();
        while self.fd_table.len() <= fd {
            self.fd_table.push(None);
        }
        self.ensure_fd_bitmap_covers(fd);
        let expanded_slots = self.fd_table.len().saturating_sub(old_len);
        perf::record_fd_bitmap_word_probes(bitmap_word_probes);
        perf::record_fd_alloc(0, expanded_slots, self.fd_table.len(), true);
        Some(fd)
    }

    pub fn fd_entry(&self, fd: usize) -> Option<FdTableEntry> {
        self.fd_table
            .get(fd)
            .and_then(|entry| entry.as_ref())
            .cloned()
    }

    /// Removes an fd entry from the process table for lock-free close cleanup.
    ///
    /// The returned entry must be closed or dropped after releasing
    /// `ProcessControlBlockInner` so file cleanup cannot re-enter this lock.
    pub fn take_fd_entry(&mut self, fd: usize) -> Option<FdTableEntry> {
        let entry = self.fd_table.get_mut(fd)?.take();
        if entry.is_some() {
            self.clear_fd_open_bit(fd);
            perf::record_fd_take();
        }
        entry
    }

    /// Installs an fd entry at an already validated descriptor number.
    ///
    /// Returns the entry that was previously installed at `fd`, if any. The
    /// caller owns any close cleanup for that returned entry after dropping the
    /// process lock.
    pub fn set_fd_entry(&mut self, fd: usize, entry: FdTableEntry) -> Option<FdTableEntry> {
        while self.fd_table.len() <= fd {
            self.fd_table.push(None);
        }
        let previous = self.fd_table[fd].replace(entry);
        self.set_fd_open_bit(fd);
        perf::record_fd_install(self.fd_table.len());
        previous
    }

    pub(crate) fn close_on_exec_fd_entries(&mut self) -> Vec<FdTableEntry> {
        let mut closed = Vec::new();
        for fd in 0..self.fd_table.len() {
            let should_close = self.fd_table[fd]
                .as_ref()
                .map(|entry| entry.close_on_exec())
                .unwrap_or(false);
            if should_close {
                if let Some(entry) = self.fd_table[fd].take() {
                    closed.push(entry);
                }
                self.clear_fd_open_bit(fd);
            }
        }
        closed
    }

    pub(crate) fn take_all_fd_entries(&mut self) -> Vec<FdTableEntry> {
        let mut closed = Vec::new();
        for fd in 0..self.fd_table.len() {
            if let Some(entry) = self.fd_table[fd].take() {
                closed.push(entry);
            }
        }
        self.fd_table.clear();
        self.fd_open_bits.clear();
        self.next_fd_hint = 0;
        closed
    }

    fn ensure_fd_bitmap_covers(&mut self, fd: usize) {
        let word_count = fd_bitmap_word_count(fd + 1);
        while self.fd_open_bits.len() < word_count {
            self.fd_open_bits.push(0);
        }
    }

    fn fd_open_bit_is_set(&self, fd: usize) -> bool {
        let (word, bit) = fd_bit_position(fd);
        self.fd_open_bits
            .get(word)
            .map(|bits| bits & bit != 0)
            .unwrap_or(false)
    }

    fn set_fd_open_bit(&mut self, fd: usize) {
        self.ensure_fd_bitmap_covers(fd);
        let (word, bit) = fd_bit_position(fd);
        self.fd_open_bits[word] |= bit;
        if self.next_fd_hint == fd {
            let mut next = fd + 1;
            while next < self.fd_table.len() && self.fd_open_bit_is_set(next) {
                next += 1;
            }
            self.next_fd_hint = next;
        }
    }

    fn clear_fd_open_bit(&mut self, fd: usize) {
        let (word, bit) = fd_bit_position(fd);
        if let Some(bits) = self.fd_open_bits.get_mut(word) {
            *bits &= !bit;
        }
        if fd < self.next_fd_hint {
            self.next_fd_hint = fd;
        }
    }

    fn find_free_fd_in_bitmap(
        &self,
        lower_bound: usize,
        search_end: usize,
        bitmap_word_probes: &mut usize,
    ) -> Option<usize> {
        if lower_bound >= search_end {
            return None;
        }

        let mut word_index = lower_bound / FD_BITMAP_WORD_BITS;
        while word_index * FD_BITMAP_WORD_BITS < search_end {
            *bitmap_word_probes += 1;
            let word_start = word_index * FD_BITMAP_WORD_BITS;
            let word_end = (word_start + FD_BITMAP_WORD_BITS).min(search_end);
            let used_bits = *self.fd_open_bits.get(word_index).unwrap_or(&0);
            let before_lower_bound = lower_bound.saturating_sub(word_start);
            let low_mask = if before_lower_bound == 0 {
                0
            } else {
                (1usize << before_lower_bound) - 1
            };
            let valid_bits = word_end - word_start;
            let high_mask = if valid_bits == FD_BITMAP_WORD_BITS {
                0
            } else {
                !((1usize << valid_bits) - 1)
            };
            let unavailable = used_bits | low_mask | high_mask;

            if unavailable != usize::MAX {
                let fd = word_start + (!unavailable).trailing_zeros() as usize;
                debug_assert!(fd >= lower_bound);
                debug_assert!(fd < search_end);
                debug_assert!(self.fd_table.get(fd).is_none_or(Option::is_none));
                return Some(fd);
            }
            word_index += 1;
        }

        None
    }

    pub fn alloc_tid(&mut self) -> usize {
        self.task_res_allocator.alloc()
    }

    pub fn dealloc_tid(&mut self, tid: usize) {
        self.task_res_allocator.dealloc(tid)
    }

    pub fn thread_count(&self) -> usize {
        self.tasks.iter().filter(|task| task.is_some()).count()
    }

    pub fn get_task(&self, tid: usize) -> Arc<TaskControlBlock> {
        self.tasks
            .get(tid)
            .and_then(|task| task.as_ref())
            .expect("task slot must exist while referenced by process lifecycle code")
            .clone()
    }
}

pub(crate) fn comm_from_cmdline(cmdline: &[String]) -> String {
    cmdline
        .first()
        .and_then(|arg| arg.rsplit('/').next())
        .filter(|name| !name.is_empty())
        .unwrap_or("process")
        .chars()
        .take(15)
        .collect()
}

impl ProcessControlBlock {
    pub fn inner_exclusive_access(&self) -> ProcessInnerGuard<'_> {
        let inner = self.inner.exclusive_access();
        self.note_inner_acquired();
        ProcessInnerGuard {
            process: self,
            inner: Some(inner),
        }
    }

    pub fn try_inner_exclusive_access(&self) -> Option<ProcessInnerGuard<'_>> {
        let inner = self.inner.try_exclusive_access()?;
        self.note_inner_acquired();
        Some(ProcessInnerGuard {
            process: self,
            inner: Some(inner),
        })
    }

    fn note_inner_acquired(&self) {
        let cpu = crate::cpu::current_id();
        self.inner_owner_cpu
            .compare_exchange(NO_INNER_OWNER, cpu, Ordering::Acquire, Ordering::Relaxed)
            .unwrap_or_else(|owner| panic!("process inner lock acquired with stale owner {owner}"));
    }

    pub(crate) fn inner_owned_by_current(&self) -> bool {
        self.inner_owner_cpu.load(Ordering::Relaxed) == crate::cpu::current_id()
    }

    pub(crate) fn path_snapshot(&self) -> PathSnapshot {
        // Snapshot lookup identity and visible path strings under the PCB lock,
        // then let syscall path code release it before touching fd tables or
        // VFS backends.
        let inner = self.inner_exclusive_access();
        PathSnapshot {
            context: inner.fs.path_context(),
            cwd_path: inner.fs.cwd_path.clone(),
            root_path: inner.fs.root_path.clone(),
        }
    }

    pub(crate) fn mount_namespace_id(&self) -> MountNamespaceId {
        self.inner_exclusive_access().fs.mount_namespace_id
    }

    pub(crate) fn set_mount_namespace_id(&self, mount_namespace_id: MountNamespaceId) {
        self.inner_exclusive_access().fs.mount_namespace_id = mount_namespace_id;
    }

    pub(crate) fn pid_namespace(&self) -> ProcessNamespace {
        let inner = self.inner_exclusive_access();
        ProcessNamespace {
            id: inner.pid_namespace_id,
            parent_id: inner.pid_namespace_parent_id,
        }
    }

    pub(crate) fn user_namespace(&self) -> ProcessNamespace {
        let inner = self.inner_exclusive_access();
        ProcessNamespace {
            id: inner.user_namespace_id,
            parent_id: inner.user_namespace_parent_id,
        }
    }

    pub(crate) fn enter_new_pid_namespace(&self, id: usize) {
        let mut inner = self.inner_exclusive_access();
        inner.pid_namespace_parent_id = Some(inner.pid_namespace_id);
        inner.pid_namespace_id = id;
    }

    pub(crate) fn enter_new_user_namespace(&self, id: usize) {
        let mut inner = self.inner_exclusive_access();
        inner.user_namespace_parent_id = Some(inner.user_namespace_id);
        inner.user_namespace_id = id;
    }

    pub(crate) fn visible_pid(&self) -> usize {
        self.pid_visible_from_namespace(self.pid_namespace())
            .unwrap_or(self.pid.0)
    }

    pub(crate) fn pid_visible_from_namespace(&self, namespace: ProcessNamespace) -> Option<usize> {
        let inner = self.inner_exclusive_access();
        if namespace.parent_id.is_none() {
            return Some(self.pid.0);
        }
        if inner.pid_namespace_id == namespace.id {
            if inner.pid_namespace_parent_id.is_some() && self.pid.0 == inner.pid_namespace_id {
                Some(1)
            } else {
                Some(self.pid.0)
            }
        } else if inner.pid_namespace_parent_id == Some(namespace.id) {
            Some(self.pid.0)
        } else {
            None
        }
    }

    pub fn set_working_dir(&self, cwd: WorkingDir, cwd_path: String) {
        let mut inner = self.inner_exclusive_access();
        inner.fs.set_working_dir(cwd, cwd_path);
    }

    pub fn set_root_dir(&self, root: WorkingDir, root_path: String) {
        let mut inner = self.inner_exclusive_access();
        inner.fs.set_root_dir(root, root_path);
    }

    pub(crate) fn references_vfs_mount(&self, mount_id: crate::fs::MountId) -> bool {
        let inner = self.inner_exclusive_access();
        inner.fs.references_mount(mount_id)
            || inner
                .fd_table
                .iter()
                .flatten()
                .any(|entry| entry.vfs_mount_id() == Some(mount_id))
    }

    pub(crate) fn references_file_description(
        &self,
        file: &Arc<dyn crate::fs::File + Send + Sync>,
    ) -> bool {
        self.inner
            .exclusive_access()
            .fd_table
            .iter()
            .flatten()
            .any(|entry| entry.is_same_file_description(file))
    }

    pub fn getpid(&self) -> usize {
        self.pid.0
    }

    pub fn parent_process(&self) -> Option<Arc<Self>> {
        self.inner
            .exclusive_access()
            .parent
            .as_ref()
            .and_then(Weak::upgrade)
    }

    pub(crate) fn main_task(&self) -> Arc<TaskControlBlock> {
        self.inner_exclusive_access().get_task(0)
    }

    /// Records the task that must be released when a CLONE_VFORK child execs
    /// or exits.
    pub(crate) fn begin_vfork(&self, parent_task: Arc<TaskControlBlock>) {
        self.inner_exclusive_access().vfork_parent = Some(parent_task);
    }

    pub(crate) fn vfork_in_progress(&self) -> bool {
        self.inner_exclusive_access().vfork_parent.is_some()
    }

    /// Wakes the saved CLONE_VFORK parent exactly once.
    ///
    /// The parent task is stored on the child PCB because either execve() or
    /// process exit can complete the vfork critical section.
    pub(crate) fn release_vfork_parent(&self) {
        let parent_task = self.inner_exclusive_access().vfork_parent.take();
        if let Some(parent_task) = parent_task {
            wakeup_task(parent_task);
        }
    }

    pub fn getppid(&self) -> usize {
        let namespace = self.pid_namespace();
        self.parent_process()
            .and_then(|parent| parent.pid_visible_from_namespace(namespace))
            .unwrap_or(0)
    }

    pub fn process_group_id(&self) -> usize {
        self.inner_exclusive_access().pgid
    }

    pub fn set_process_group_id(&self, pgid: usize) {
        self.inner_exclusive_access().pgid = pgid;
    }

    pub(crate) fn proc_snapshot(&self) -> ProcessProcSnapshot {
        let mut inner = self.inner_exclusive_access();
        let leader_status = inner
            .tasks
            .first()
            .and_then(|task| task.as_ref())
            .map(|task| {
                let task_inner = task.inner_exclusive_access();
                proc_task_state(task_inner.task_status, task_inner.proc_sleeping)
            });
        let state = if inner.is_zombie {
            'Z'
        } else {
            // CONTEXT: Linux /proc/<tgid>/stat reports the thread-group
            // leader state. LTP uses this to wait until the main thread blocks
            // even while a helper thread in the same process is still running.
            match leader_status {
                Some(state) => state,
                None => {
                    if inner.tasks.iter().flatten().any(|task| {
                        matches!(
                            task.inner_exclusive_access().task_status,
                            TaskStatus::Ready | TaskStatus::Running
                        )
                    }) {
                        'R'
                    } else {
                        'S'
                    }
                }
            }
        };
        let timer_slack_ns = inner
            .tasks
            .first()
            .and_then(|task| task.as_ref())
            .map(|task| task.inner_exclusive_access().timer_slack_ns)
            .unwrap_or(crate::task::DEFAULT_TIMER_SLACK_NS);
        let resident_kb = inner.memory_set.resident_bytes() / 1024;
        inner.cpu_times.record_resident_kb(resident_kb);
        ProcessProcSnapshot {
            pid: self.pid.0,
            ppid: inner
                .parent
                .as_ref()
                .and_then(Weak::upgrade)
                .map_or(0, |parent| parent.getpid()),
            pgid: inner.pgid,
            comm: inner.comm.clone(),
            state,
            executable_node: inner.executable_node,
            executable_path: inner.executable_path.clone(),
            cmdline: inner.cmdline.clone(),
            cpu_times: inner.cpu_times.snapshot(),
            credentials: inner.credentials.clone(),
            thread_count: inner.thread_count(),
            mount_namespace_id: inner.fs.mount_namespace_id,
            pid_namespace_id: inner.pid_namespace_id,
            pid_namespace_parent_id: inner.pid_namespace_parent_id,
            user_namespace_id: inner.user_namespace_id,
            user_namespace_parent_id: inner.user_namespace_parent_id,
            resident_kb,
            locked_kb: inner.memory_set.locked_bytes() / 1024,
            no_new_privs: inner.no_new_privs,
            timer_slack_ns,
        }
    }

    pub(crate) fn proc_maps_content(&self) -> String {
        let entries = {
            let inner = self.inner_exclusive_access();
            inner.memory_set.proc_maps_entries()
        };
        let mut output = String::new();
        for entry in entries {
            let mut perms = String::new();
            perms.push(if entry.readable { 'r' } else { '-' });
            perms.push(if entry.writable { 'w' } else { '-' });
            perms.push(if entry.executable { 'x' } else { '-' });
            perms.push(if entry.shared { 's' } else { 'p' });
            output.push_str(&format!(
                "{:x}-{:x} {} {:08x} 00:00 0\n",
                entry.start, entry.end, perms, entry.offset
            ));
        }
        output
    }

    pub(crate) fn proc_smaps_content(&self) -> String {
        let entries = {
            let inner = self.inner_exclusive_access();
            inner.memory_set.proc_maps_entries()
        };
        let mut output = String::new();
        for entry in entries {
            let mut perms = String::new();
            perms.push(if entry.readable { 'r' } else { '-' });
            perms.push(if entry.writable { 'w' } else { '-' });
            perms.push(if entry.executable { 'x' } else { '-' });
            perms.push(if entry.shared { 's' } else { 'p' });
            output.push_str(&format!(
                "{:x}-{:x} {} {:08x} 00:00 0\n\
                 Size:\t{} kB\n\
                 Rss:\t{} kB\n\
                 Locked:\t{} kB\n",
                entry.start,
                entry.end,
                perms,
                entry.offset,
                (entry.end - entry.start) / 1024,
                entry.resident_kb,
                entry.locked_kb,
            ));
        }
        output
    }

    pub fn mark_user_time_entry(&self, now_us: usize) {
        self.inner_exclusive_access()
            .cpu_times
            .mark_user_entry(now_us);
    }

    pub fn mark_kernel_time_entry(&self, now_us: usize) {
        self.inner_exclusive_access()
            .cpu_times
            .mark_kernel_entry(now_us);
    }

    pub fn account_user_time_until(&self, now_us: usize) {
        self.inner_exclusive_access()
            .cpu_times
            .account_user_until(now_us);
    }

    pub fn account_system_time_until(&self, now_us: usize) {
        self.inner_exclusive_access()
            .cpu_times
            .account_system_until(now_us);
    }

    pub fn try_account_system_time_until(&self, now_us: usize) {
        if let Some(mut inner) = self.try_inner_exclusive_access() {
            inner.cpu_times.account_system_until(now_us);
        }
    }

    pub fn cpu_times_snapshot(&self) -> ProcessCpuTimesSnapshot {
        let mut inner = self.inner_exclusive_access();
        let resident_kb = inner.memory_set.resident_bytes() / 1024;
        inner.cpu_times.record_resident_kb(resident_kb);
        inner.cpu_times.snapshot()
    }

    pub fn credentials(&self) -> Credentials {
        self.inner_exclusive_access().credentials.clone()
    }

    pub fn umask(&self) -> u32 {
        self.inner_exclusive_access().umask
    }

    pub fn set_umask(&self, mask: u32) -> u32 {
        let mut inner = self.inner_exclusive_access();
        let previous = inner.umask;
        inner.umask = mask & 0o777;
        previous
    }

    pub fn personality(&self) -> u32 {
        self.inner_exclusive_access().personality
    }

    pub fn set_personality(&self, personality: u32) {
        self.inner_exclusive_access().personality = personality;
    }

    pub fn replace_supplementary_groups(&self, groups: Vec<u32>) {
        self.inner_exclusive_access().credentials.groups = groups;
    }

    pub(crate) fn mutate_credentials<R>(&self, f: impl FnOnce(&mut Credentials) -> R) -> R {
        let mut inner = self.inner_exclusive_access();
        f(&mut inner.credentials)
    }

    pub(crate) fn expire_real_timer(
        &self,
        generation: u64,
        now_us: usize,
    ) -> Option<RealTimerExpiry> {
        let mut inner = self.inner_exclusive_access();
        if inner.timers.real.generation != generation
            || !inner.timers.real.is_armed()
            || inner.timers.real.next_expire_us > now_us
        {
            return None;
        }
        let task = inner
            .tasks
            .first()
            .and_then(|task| task.as_ref().cloned())?;
        let next_timer = if inner.timers.real.interval_us == 0 {
            inner.timers.real.next_expire_us = 0;
            None
        } else {
            let next_expire_us = now_us.saturating_add(inner.timers.real.interval_us);
            inner.timers.real.next_expire_us = next_expire_us;
            Some((next_expire_us, generation))
        };
        Some((task, next_timer))
    }

    pub(crate) fn create_posix_timer(&self, clock_id: i32, signal: u32) -> usize {
        let mut inner = self.inner_exclusive_access();
        if let Some((idx, slot)) = inner
            .timers
            .posix
            .iter_mut()
            .enumerate()
            .find(|(_, slot)| slot.is_none())
        {
            *slot = Some(ProcessPosixTimer::new(clock_id, signal));
            idx
        } else {
            inner
                .timers
                .posix
                .push(Some(ProcessPosixTimer::new(clock_id, signal)));
            inner.timers.posix.len() - 1
        }
    }

    pub(crate) fn posix_timer_clock(&self, timer_id: usize) -> Option<i32> {
        let inner = self.inner_exclusive_access();
        Some(inner.timers.posix.get(timer_id)?.as_ref()?.clock_id)
    }

    pub(crate) fn set_posix_timer(
        &self,
        timer_id: usize,
        interval_us: usize,
        next_expire_us: usize,
        now_us: usize,
    ) -> Option<(usize, usize, u64)> {
        let mut inner = self.inner_exclusive_access();
        let timer = inner.timers.posix.get_mut(timer_id)?.as_mut()?;
        let old_interval_us = timer.interval_us;
        let old_remaining_us = timer.remaining_us(now_us);
        timer.generation = timer.generation.wrapping_add(1);
        timer.interval_us = interval_us;
        timer.next_expire_us = next_expire_us;
        Some((old_interval_us, old_remaining_us, timer.generation))
    }

    pub(crate) fn posix_timer_snapshot(
        &self,
        timer_id: usize,
        now_us: usize,
    ) -> Option<(usize, usize)> {
        let inner = self.inner_exclusive_access();
        let timer = inner.timers.posix.get(timer_id)?.as_ref()?;
        Some((timer.interval_us, timer.remaining_us(now_us)))
    }

    pub(crate) fn delete_posix_timer(&self, timer_id: usize) -> Option<()> {
        let mut inner = self.inner_exclusive_access();
        let slot = inner.timers.posix.get_mut(timer_id)?;
        slot.take()?;
        Some(())
    }

    pub(crate) fn expire_posix_timer(
        &self,
        timer_id: usize,
        generation: u64,
        now_us: usize,
    ) -> Option<PosixTimerExpiry> {
        let mut inner = self.inner_exclusive_access();
        let timer = inner.timers.posix.get_mut(timer_id)?.as_mut()?;
        if timer.generation != generation || !timer.is_armed() || timer.next_expire_us > now_us {
            return None;
        }
        let signal = timer.signal;
        let next_timer = if timer.interval_us == 0 {
            timer.next_expire_us = 0;
            None
        } else {
            let next_expire_us = now_us.saturating_add(timer.interval_us);
            timer.next_expire_us = next_expire_us;
            Some((next_expire_us, timer.generation))
        };
        let task = inner
            .tasks
            .first()
            .and_then(|task| task.as_ref().cloned())?;
        Some((task, signal, next_timer))
    }

    pub(crate) fn tasks_snapshot(&self) -> Vec<Arc<TaskControlBlock>> {
        self.inner_exclusive_access()
            .tasks
            .iter()
            .flatten()
            .map(Arc::clone)
            .collect()
    }
}
