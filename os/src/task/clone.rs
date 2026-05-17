use super::{SIGNAL_INFO_SLOTS, TaskControlBlock, current_task, pid_alloc};
use alloc::sync::Arc;
use bitflags::bitflags;

bitflags! {
    /// Linux `clone(2)` flags. Low 8 bits of the raw `flags` argument are the
    /// exit_signal (e.g. `SIGCHLD = 17`) and are extracted before constructing
    /// this bitflags value.
    #[derive(Copy, Clone)]
    pub struct CloneFlags: u32 {
        const CLONE_VM              = 1 << 8;
        const CLONE_FS              = 1 << 9;
        const CLONE_FILES           = 1 << 10;
        const CLONE_SIGHAND         = 1 << 11;
        const CLONE_PTRACE          = 1 << 13;
        const CLONE_VFORK           = 1 << 14;
        const CLONE_PARENT          = 1 << 15;
        const CLONE_THREAD          = 1 << 16;
        const CLONE_NEWNS           = 1 << 17;
        const CLONE_SYSVSEM         = 1 << 18;
        const CLONE_SETTLS          = 1 << 19;
        const CLONE_PARENT_SETTID   = 1 << 20;
        const CLONE_CHILD_CLEARTID  = 1 << 21;
        const CLONE_DETACHED        = 1 << 22;
        const CLONE_UNTRACED        = 1 << 23;
        const CLONE_CHILD_SETTID    = 1 << 24;
        const CLONE_NEWUSER         = 1 << 28;
        const CLONE_NEWPID          = 1 << 29;
        const CLONE_NEWNET          = 1 << 30;
    }
}

#[derive(Copy, Clone)]
pub struct CloneArgs {
    pub flags: CloneFlags,
    pub exit_signal: u32,
    pub stack: usize,
    pub ptid: usize,
    pub tls: usize,
    pub ctid: usize,
}

impl CloneArgs {
    pub fn parse(flags: usize, stack: usize, ptid: usize, tls: usize, ctid: usize) -> Option<Self> {
        let exit_signal = (flags & 0xff) as u32;
        let flags = CloneFlags::from_bits_truncate((flags & !0xff) as u32);
        Self::from_parts(flags, exit_signal, stack, ptid, tls, ctid)
    }

    pub fn from_parts(
        flags: CloneFlags,
        exit_signal: u32,
        stack: usize,
        ptid: usize,
        tls: usize,
        ctid: usize,
    ) -> Option<Self> {
        if exit_signal as usize >= SIGNAL_INFO_SLOTS {
            return None;
        }
        Some(Self {
            flags,
            exit_signal,
            stack,
            ptid,
            tls,
            ctid,
        })
    }

    pub fn is_thread(&self) -> bool {
        self.flags.contains(CloneFlags::CLONE_THREAD)
    }
}

pub struct ClonedThread {
    pub task: Arc<TaskControlBlock>,
    pub linux_tid: usize,
}

/// Clones the current thread into the same process address space.
///
/// The caller has already validated Linux clone flags and user pointers. This
/// function copies scheduler/signal thread state and returns a task that still
/// must be attached to the process task table by the caller.
pub fn clone_current_thread(args: CloneArgs) -> ClonedThread {
    let current_task = current_task().expect("clone_current_thread requires a current task");
    let process = current_task
        .process
        .upgrade()
        .expect("current task process must exist while cloning a thread");
    let ustack_base = current_task
        .inner_exclusive_access()
        .res
        .as_ref()
        .expect("user thread must own TaskUserRes while cloning")
        .ustack_base;
    let parent_inner = current_task.inner_exclusive_access();
    let parent_trap_cx = *parent_inner.get_trap_cx();
    let parent_signal_mask = parent_inner.signal_mask;
    let parent_sched_policy = parent_inner.sched_policy;
    let parent_sched_priority = parent_inner.sched_priority;
    let parent_sched_reset_on_fork = parent_inner.sched_reset_on_fork;
    let parent_nice = parent_inner.nice;
    let parent_timer_slack_ns = parent_inner.timer_slack_ns;
    let parent_seccomp_mode = parent_inner.seccomp_mode;
    let parent_seccomp_filter = parent_inner.seccomp_filter.clone();
    drop(parent_inner);
    // CONTEXT: pthread_create() supplies a userspace child stack to clone().
    // In that case the kernel still needs a per-thread TrapContext, but must
    // not also map the contest default 4 MiB user stack for every pthread.
    let new_task = if args.stack == 0 {
        Arc::new(TaskControlBlock::new(process, ustack_base, true))
    } else {
        Arc::new(TaskControlBlock::new_with_supplied_stack(
            process,
            ustack_base,
            true,
        ))
    };
    let mut new_task_inner = new_task.inner_exclusive_access();
    let new_ustack_top = new_task_inner
        .res
        .as_ref()
        .expect("new cloned user task must have TaskUserRes")
        .ustack_top();
    let linux_tid = pid_alloc();
    let new_linux_tid = linux_tid.0;
    new_task_inner.linux_tid = Some(linux_tid);
    new_task_inner.signal_mask = parent_signal_mask;
    new_task_inner.sched_policy = parent_sched_policy;
    new_task_inner.sched_priority = parent_sched_priority;
    new_task_inner.sched_reset_on_fork = parent_sched_reset_on_fork;
    new_task_inner.nice = parent_nice;
    new_task_inner.timer_slack_ns = parent_timer_slack_ns;
    new_task_inner.default_timer_slack_ns = parent_timer_slack_ns;
    new_task_inner.seccomp_mode = parent_seccomp_mode;
    new_task_inner.seccomp_filter = parent_seccomp_filter;
    let new_trap_cx = new_task_inner.get_trap_cx();
    *new_trap_cx = parent_trap_cx;
    new_trap_cx.set_a0(0);
    new_trap_cx.set_sp(if args.stack != 0 {
        args.stack
    } else {
        new_ustack_top
    });
    if args.flags.contains(CloneFlags::CLONE_SETTLS) {
        new_trap_cx.set_tp(args.tls);
    }
    new_trap_cx.kernel_sp = new_task.kstack.get_top();
    if args.flags.contains(CloneFlags::CLONE_CHILD_CLEARTID) {
        new_task_inner.clear_child_tid = Some(args.ctid);
    }
    drop(new_task_inner);
    ClonedThread {
        task: new_task,
        linux_tid: new_linux_tid,
    }
}
