use super::TaskControlBlock;
use super::id::RecycleAllocator;
use super::manager::insert_into_pid2process;
use super::{CloneArgs, CloneFlags, SignalFlags, add_task};
use super::{PidHandle, pid_alloc};
use crate::config::PAGE_SIZE;
use crate::fs::{File, Stdin, Stdout, WorkingDir};
use crate::mm::{ElfLoadInfo, KERNEL_SPACE, MemorySet, translated_refmut};
use crate::sync::{Condvar, Mutex, Semaphore, UPIntrFreeCell, UPIntrRefMut};
use crate::trap::{TrapContext, trap_handler};
use alloc::string::String;
use alloc::sync::{Arc, Weak};
use alloc::vec;
use alloc::vec::Vec;

const AT_NULL: usize = 0;
const AT_PHDR: usize = 3;
const AT_PHENT: usize = 4;
const AT_PHNUM: usize = 5;
const AT_PAGESZ: usize = 6;
const AT_BASE: usize = 7;
const AT_FLAGS: usize = 8;
const AT_ENTRY: usize = 9;
const AT_UID: usize = 11;
const AT_EUID: usize = 12;
const AT_GID: usize = 13;
const AT_EGID: usize = 14;
const AT_SECURE: usize = 23;
const AT_RANDOM: usize = 25;

struct ExecStackInfo {
    entry_point: usize,
    phdr: usize,
    phent: usize,
    phnum: usize,
}

fn align_down(value: usize, align: usize) -> usize {
    value & !(align - 1)
}

fn write_user_byte(token: usize, addr: usize, value: u8) {
    *translated_refmut(token, addr as *mut u8) = value;
}

fn write_user_usize(token: usize, addr: usize, value: usize) {
    *translated_refmut(token, addr as *mut usize) = value;
}

fn write_user_bytes(token: usize, addr: usize, bytes: &[u8]) {
    for (offset, byte) in bytes.iter().enumerate() {
        write_user_byte(token, addr + offset, *byte);
    }
}

fn push_user_string(token: usize, user_sp: &mut usize, string: &str) -> usize {
    *user_sp -= string.len() + 1;
    let addr = *user_sp;
    write_user_bytes(token, addr, string.as_bytes());
    write_user_byte(token, addr + string.len(), 0);
    *user_sp = align_down(*user_sp, core::mem::size_of::<usize>());
    addr
}

fn push_user_strings(token: usize, user_sp: &mut usize, strings: &[String]) -> Vec<usize> {
    strings
        .iter()
        .map(|string| push_user_string(token, user_sp, string.as_str()))
        .collect()
}

fn init_user_stack(
    token: usize,
    stack_top: usize,
    args: &[String],
    envs: &[String],
    stack_info: &ExecStackInfo,
) -> (usize, usize, usize) {
    let mut string_sp = stack_top;
    let env_ptrs = push_user_strings(token, &mut string_sp, envs);
    let arg_ptrs = push_user_strings(token, &mut string_sp, args);

    string_sp -= 16;
    let random_addr = string_sp;
    for offset in 0..16 {
        write_user_byte(token, random_addr + offset, 0);
    }

    let auxv = [
        (AT_PHDR, stack_info.phdr),
        (AT_PHENT, stack_info.phent),
        (AT_PHNUM, stack_info.phnum),
        (AT_PAGESZ, PAGE_SIZE),
        (AT_ENTRY, stack_info.entry_point),
        (AT_BASE, 0),
        (AT_FLAGS, 0),
        (AT_UID, 0),
        (AT_EUID, 0),
        (AT_GID, 0),
        (AT_EGID, 0),
        (AT_SECURE, 0),
        (AT_RANDOM, random_addr),
    ];

    let word = core::mem::size_of::<usize>();
    let table_size = word
        + (arg_ptrs.len() + 1) * word
        + (env_ptrs.len() + 1) * word
        + (auxv.len() + 1) * 2 * word;
    let user_sp = align_down(align_down(string_sp, 16) - table_size, 16);
    let argv_base = user_sp + word;
    let envp_base = argv_base + (arg_ptrs.len() + 1) * word;
    let auxv_base = envp_base + (env_ptrs.len() + 1) * word;

    write_user_usize(token, user_sp, arg_ptrs.len());
    for (i, ptr) in arg_ptrs.iter().enumerate() {
        write_user_usize(token, argv_base + i * word, *ptr);
    }
    write_user_usize(token, argv_base + arg_ptrs.len() * word, 0);

    for (i, ptr) in env_ptrs.iter().enumerate() {
        write_user_usize(token, envp_base + i * word, *ptr);
    }
    write_user_usize(token, envp_base + env_ptrs.len() * word, 0);

    for (i, (key, value)) in auxv.iter().enumerate() {
        let entry = auxv_base + i * 2 * word;
        write_user_usize(token, entry, *key);
        write_user_usize(token, entry + word, *value);
    }
    let null_entry = auxv_base + auxv.len() * 2 * word;
    write_user_usize(token, null_entry, AT_NULL);
    write_user_usize(token, null_entry + word, 0);

    (user_sp, argv_base, envp_base)
}

// TODO: to understand😄
pub struct ProcessControlBlock {
    // immutable
    pub pid: PidHandle,
    // mutable
    inner: UPIntrFreeCell<ProcessControlBlockInner>,
}

pub struct ProcessControlBlockInner {
    pub is_zombie: bool,
    pub memory_set: MemorySet,
    pub cwd: WorkingDir,
    pub cwd_path: String,
    pub parent: Option<Weak<ProcessControlBlock>>,
    pub children: Vec<Arc<ProcessControlBlock>>,
    pub exit_code: i32,
    pub fd_table: Vec<Option<Arc<dyn File + Send + Sync>>>,
    pub signals: SignalFlags,
    pub tasks: Vec<Option<Arc<TaskControlBlock>>>,
    pub task_res_allocator: RecycleAllocator,
    pub mutex_list: Vec<Option<Arc<dyn Mutex>>>,
    pub semaphore_list: Vec<Option<Arc<Semaphore>>>,
    pub condvar_list: Vec<Option<Arc<Condvar>>>,
}

impl ProcessControlBlockInner {
    #[allow(unused)]
    pub fn get_user_token(&self) -> usize {
        self.memory_set.token()
    }

    pub fn alloc_fd(&mut self) -> usize {
        if let Some(fd) = (0..self.fd_table.len()).find(|fd| self.fd_table[*fd].is_none()) {
            fd
        } else {
            self.fd_table.push(None);
            self.fd_table.len() - 1
        }
    }

    pub fn alloc_tid(&mut self) -> usize {
        self.task_res_allocator.alloc()
    }

    pub fn dealloc_tid(&mut self, tid: usize) {
        self.task_res_allocator.dealloc(tid)
    }

    pub fn thread_count(&self) -> usize {
        self.tasks.len()
    }

    pub fn get_task(&self, tid: usize) -> Arc<TaskControlBlock> {
        self.tasks[tid].as_ref().unwrap().clone()
    }
}

impl ProcessControlBlock {
    pub fn inner_exclusive_access(&self) -> UPIntrRefMut<'_, ProcessControlBlockInner> {
        self.inner.exclusive_access()
    }

    pub fn working_dir(&self) -> WorkingDir {
        self.inner.exclusive_access().cwd
    }

    pub fn working_dir_path(&self) -> String {
        self.inner.exclusive_access().cwd_path.clone()
    }

    pub fn set_working_dir(&self, cwd: WorkingDir, cwd_path: String) {
        let mut inner = self.inner.exclusive_access();
        inner.cwd = cwd;
        inner.cwd_path = cwd_path;
    }

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

    // TODO: to understand
    pub fn new(elf_data: &[u8]) -> Arc<Self> {
        // memory_set with elf program headers/trampoline/trap context/user stack
        let ElfLoadInfo {
            memory_set,
            ustack_base,
            entry_point,
            ..
        } = MemorySet::from_elf(elf_data);
        // allocate a pid
        let pid_handle = pid_alloc();
        let process = Arc::new(Self {
            pid: pid_handle,
            inner: unsafe {
                UPIntrFreeCell::new(ProcessControlBlockInner {
                    is_zombie: false,
                    memory_set,
                    cwd: WorkingDir::root(),
                    cwd_path: "/".into(),
                    parent: None,
                    children: Vec::new(),
                    exit_code: 0,
                    // TODO: could try to extract this piece of code, neater
                    fd_table: vec![
                        // 0 -> stdin
                        Some(Arc::new(Stdin)),
                        // 1 -> stdout
                        Some(Arc::new(Stdout)),
                        // 2 -> stderr
                        Some(Arc::new(Stdout)),
                    ],
                    signals: SignalFlags::empty(),
                    tasks: Vec::new(),
                    task_res_allocator: RecycleAllocator::new(),
                    mutex_list: Vec::new(),
                    semaphore_list: Vec::new(),
                    condvar_list: Vec::new(),
                })
            },
        });
        // create a main thread, we should allocate ustack and trap_cx here
        let task = Arc::new(TaskControlBlock::new(
            Arc::clone(&process),
            ustack_base,
            true,
        ));
        // prepare trap_cx of main thread
        let task_inner = task.inner_exclusive_access();
        let trap_cx = task_inner.get_trap_cx();
        let ustack_top = task_inner.res.as_ref().unwrap().ustack_top();
        let kstack_top = task.kstack.get_top();
        drop(task_inner);
        *trap_cx = TrapContext::app_init_context(
            entry_point,
            ustack_top,
            KERNEL_SPACE.exclusive_access().token(),
            kstack_top,
            trap_handler as usize,
        );
        // add main thread to the process
        let mut process_inner = process.inner_exclusive_access();
        process_inner.tasks.push(Some(Arc::clone(&task)));
        drop(process_inner);
        insert_into_pid2process(process.getpid(), Arc::clone(&process));
        // add main thread to scheduler
        add_task(task);
        process
    }

    /// Only support processes with a single thread.
    pub fn exec(self: &Arc<Self>, elf_data: &[u8], args: Vec<String>, envs: Vec<String>) {
        assert_eq!(self.inner_exclusive_access().thread_count(), 1);
        // memory_set with elf program headers/trampoline/trap context/user stack
        let ElfLoadInfo {
            memory_set,
            ustack_base,
            entry_point,
            phdr,
            phent,
            phnum,
        } = MemorySet::from_elf(elf_data);
        let stack_info = ExecStackInfo {
            entry_point,
            phdr,
            phent,
            phnum,
        };
        let new_token = memory_set.token();
        // substitute memory_set
        self.inner_exclusive_access().memory_set = memory_set;
        // then we alloc user resource for main thread again
        // since memory_set has been changed
        let task = self.inner_exclusive_access().get_task(0);
        let mut task_inner = task.inner_exclusive_access();
        task_inner.res.as_mut().unwrap().ustack_base = ustack_base;
        task_inner.res.as_mut().unwrap().alloc_user_res();
        task_inner.trap_cx_ppn = task_inner.res.as_mut().unwrap().trap_cx_ppn();
        let (user_sp, argv_base, envp_base) = init_user_stack(
            new_token,
            task_inner.res.as_ref().unwrap().ustack_top(),
            &args,
            &envs,
            &stack_info,
        );
        // initialize trap_cx
        let mut trap_cx = TrapContext::app_init_context(
            entry_point,
            user_sp,
            KERNEL_SPACE.exclusive_access().token(),
            task.kstack.get_top(),
            trap_handler as usize,
        );
        trap_cx.x[10] = args.len();
        trap_cx.x[11] = argv_base;
        trap_cx.x[12] = envp_base;
        *task_inner.get_trap_cx() = trap_cx;
    }

    /// Only support processes with a single thread.
    pub fn fork(self: &Arc<Self>) -> Arc<Self> {
        let mut parent = self.inner_exclusive_access();
        assert_eq!(parent.thread_count(), 1);
        // clone parent's memory_set completely including trampoline/ustacks/trap_cxs
        let memory_set = MemorySet::from_existed_user(&parent.memory_set);
        // alloc a pid
        let pid = pid_alloc();
        // copy fd table
        let mut new_fd_table: Vec<Option<Arc<dyn File + Send + Sync>>> = Vec::new();
        for fd in parent.fd_table.iter() {
            if let Some(file) = fd {
                new_fd_table.push(Some(file.clone()));
            } else {
                new_fd_table.push(None);
            }
        }
        // create child process pcb
        let child = Arc::new(Self {
            pid,
            inner: unsafe {
                UPIntrFreeCell::new(ProcessControlBlockInner {
                    is_zombie: false,
                    memory_set,
                    cwd: parent.cwd,
                    cwd_path: parent.cwd_path.clone(),
                    parent: Some(Arc::downgrade(self)),
                    children: Vec::new(),
                    exit_code: 0,
                    fd_table: new_fd_table,
                    signals: SignalFlags::empty(),
                    tasks: Vec::new(),
                    task_res_allocator: RecycleAllocator::new(),
                    mutex_list: Vec::new(),
                    semaphore_list: Vec::new(),
                    condvar_list: Vec::new(),
                })
            },
        });
        // add child
        parent.children.push(Arc::clone(&child));
        // create main thread of child process
        let task = Arc::new(TaskControlBlock::new(
            Arc::clone(&child),
            parent
                .get_task(0)
                .inner_exclusive_access()
                .res
                .as_ref()
                .unwrap()
                .ustack_base(),
            // here we do not allocate trap_cx or ustack again
            // but mention that we allocate a new kstack here
            false,
        ));
        // attach task to child process
        let mut child_inner = child.inner_exclusive_access();
        child_inner.tasks.push(Some(Arc::clone(&task)));
        drop(child_inner);
        // modify kstack_top in trap_cx of this thread
        let task_inner = task.inner_exclusive_access();
        let trap_cx = task_inner.get_trap_cx();
        trap_cx.kernel_sp = task.kstack.get_top();
        drop(task_inner);
        insert_into_pid2process(child.getpid(), Arc::clone(&child));
        // add this thread to scheduler
        add_task(task);
        child
    }

    pub fn getpid(&self) -> usize {
        self.pid.0
    }
}
