use super::exec::{ExecStackInfo, init_user_stack};
use super::id::RecycleAllocator;
use super::manager::insert_into_pid2process;
use super::process::{ProcessControlBlock, ProcessControlBlockInner, ProcessCpuTimes};
use super::{
    CloneArgs, CloneFlags, FdTableEntry, SignalFlags, TaskControlBlock, add_task, pid_alloc,
};
use crate::fs::{OpenFlags, Stdin, Stdout, WorkingDir};
use crate::mm::{ElfLoadInfo, KERNEL_SPACE, MemorySet};
use crate::sync::UPIntrFreeCell;
use crate::trap::{TrapContext, trap_handler};
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;

impl ProcessControlBlock {
    pub fn attach_task(&self, task: Arc<TaskControlBlock>) -> usize {
        let tid = task.inner_exclusive_access().res.as_ref().unwrap().tid;
        let mut inner = self.inner_exclusive_access();
        while inner.tasks.len() < tid + 1 {
            inner.tasks.push(None);
        }
        inner.tasks[tid] = Some(task);
        inner.memory_set.token()
    }

    pub fn configure_cloned_main_task(&self, args: CloneArgs) -> usize {
        let inner = self.inner_exclusive_access();
        let task = inner.tasks[0].as_ref().unwrap();
        let mut task_inner = task.inner_exclusive_access();
        let trap_cx = task_inner.get_trap_cx();
        trap_cx.set_a0(0);
        if args.stack != 0 {
            trap_cx.set_sp(args.stack);
        }
        if args.flags.contains(CloneFlags::CLONE_SETTLS) {
            trap_cx.set_tp(args.tls);
        }
        if args.flags.contains(CloneFlags::CLONE_CHILD_CLEARTID) {
            task_inner.clear_child_tid = Some(args.ctid);
        }
        inner.memory_set.token()
    }

    pub fn new_with_args(elf_data: &[u8], args: Vec<String>, envs: Vec<String>) -> Arc<Self> {
        let ElfLoadInfo {
            memory_set,
            ustack_base,
            entry_point,
            phdr,
            phent,
            phnum,
        } = MemorySet::from_elf(elf_data);
        let pid_handle = pid_alloc();
        let process = Arc::new(Self {
            pid: pid_handle,
            inner: unsafe {
                UPIntrFreeCell::new(ProcessControlBlockInner {
                    is_zombie: false,
                    memory_set,
                    cwd: WorkingDir::root(),
                    cwd_path: "/".into(),
                    cmdline: args.clone(),
                    parent: None,
                    children: Vec::new(),
                    exit_code: 0,
                    fd_table: vec![
                        Some(FdTableEntry::from_file(
                            Arc::new(Stdin::new()),
                            OpenFlags::RDONLY,
                        )),
                        Some(FdTableEntry::from_file(
                            Arc::new(Stdout::new()),
                            OpenFlags::WRONLY,
                        )),
                        Some(FdTableEntry::from_file(
                            Arc::new(Stdout::new()),
                            OpenFlags::WRONLY,
                        )),
                    ],
                    signals: SignalFlags::empty(),
                    cpu_times: ProcessCpuTimes::default(),
                    tasks: Vec::new(),
                    task_res_allocator: RecycleAllocator::new(),
                })
            },
        });

        let task = Arc::new(TaskControlBlock::new(
            Arc::clone(&process),
            ustack_base,
            true,
        ));
        let process_token = process.inner_exclusive_access().memory_set.token();
        let task_inner = task.inner_exclusive_access();
        let trap_cx = task_inner.get_trap_cx();
        let user_sp = task_inner.res.as_ref().unwrap().ustack_top();
        let kstack_top = task.kstack.get_top();
        let stack_info = ExecStackInfo {
            entry_point,
            phdr,
            phent,
            phnum,
        };
        let (stack_top, _, _) = init_user_stack(process_token, user_sp, &args, &envs, &stack_info);
        let app_trap_cx = TrapContext::app_init_context(
            entry_point,
            stack_top,
            KERNEL_SPACE.exclusive_access().token(),
            kstack_top,
            trap_handler as usize,
        );
        *trap_cx = app_trap_cx;
        drop(task_inner);

        let mut process_inner = process.inner_exclusive_access();
        process_inner.tasks.push(Some(Arc::clone(&task)));
        drop(process_inner);
        insert_into_pid2process(process.getpid(), Arc::clone(&process));
        add_task(task);
        process
    }

    /// Only support processes with a single thread.
    pub fn fork(self: &Arc<Self>, child_parent: Arc<Self>) -> Arc<Self> {
        let parent = self.inner_exclusive_access();
        assert_eq!(parent.thread_count(), 1);
        let memory_set = MemorySet::from_existed_user(&parent.memory_set);
        let pid = pid_alloc();
        let new_fd_table = parent.fd_table.clone();
        let cwd = parent.cwd;
        let cwd_path = parent.cwd_path.clone();
        let cmdline = parent.cmdline.clone();
        let ustack_base = parent
            .get_task(0)
            .inner_exclusive_access()
            .res
            .as_ref()
            .unwrap()
            .ustack_base();
        drop(parent);

        let child = Arc::new(Self {
            pid,
            inner: unsafe {
                UPIntrFreeCell::new(ProcessControlBlockInner {
                    is_zombie: false,
                    memory_set,
                    cwd,
                    cwd_path,
                    cmdline,
                    parent: Some(Arc::downgrade(&child_parent)),
                    children: Vec::new(),
                    exit_code: 0,
                    fd_table: new_fd_table,
                    signals: SignalFlags::empty(),
                    cpu_times: ProcessCpuTimes::default(),
                    tasks: Vec::new(),
                    task_res_allocator: RecycleAllocator::new(),
                })
            },
        });
        child_parent
            .inner_exclusive_access()
            .children
            .push(Arc::clone(&child));

        let task = Arc::new(TaskControlBlock::new(
            Arc::clone(&child),
            ustack_base,
            false,
        ));
        let mut child_inner = child.inner_exclusive_access();
        child_inner.tasks.push(Some(Arc::clone(&task)));
        drop(child_inner);

        let task_inner = task.inner_exclusive_access();
        let trap_cx = task_inner.get_trap_cx();
        trap_cx.kernel_sp = task.kstack.get_top();
        drop(task_inner);
        insert_into_pid2process(child.getpid(), Arc::clone(&child));
        add_task(task);
        child
    }
}
