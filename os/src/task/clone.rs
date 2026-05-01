use super::{SIGCHLD, TaskControlBlock, current_task};
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
    }
}

#[derive(Copy, Clone)]
pub struct CloneArgs {
    pub flags: CloneFlags,
    pub stack: usize,
    pub ptid: usize,
    pub tls: usize,
    pub ctid: usize,
}

impl CloneArgs {
    pub fn parse(flags: usize, stack: usize, ptid: usize, tls: usize, ctid: usize) -> Option<Self> {
        let exit_signal = (flags & 0xff) as u32;
        if exit_signal != 0 && exit_signal != SIGCHLD {
            return None;
        }
        Some(Self {
            flags: CloneFlags::from_bits_truncate((flags & !0xff) as u32),
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
    pub tid: usize,
}

pub fn clone_current_thread(args: CloneArgs) -> ClonedThread {
    let current_task = current_task().unwrap();
    let process = current_task.process.upgrade().unwrap();
    let ustack_base = current_task
        .inner_exclusive_access()
        .res
        .as_ref()
        .unwrap()
        .ustack_base;
    let parent_trap_cx = *current_task.inner_exclusive_access().get_trap_cx();
    let new_task = Arc::new(TaskControlBlock::new(process, ustack_base, true));
    let mut new_task_inner = new_task.inner_exclusive_access();
    let new_tid = new_task_inner.res.as_ref().unwrap().tid;
    let new_ustack_top = new_task_inner.res.as_ref().unwrap().ustack_top();
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
        tid: new_tid,
    }
}
