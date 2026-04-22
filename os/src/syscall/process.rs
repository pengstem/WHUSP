use crate::fs::{OpenFlags, open_file_at};
use crate::mm::{translated_ref, translated_refmut, translated_str};
use crate::task::{
    CloneArgs, CloneFlags, SignalFlags, add_task, clone_current_thread, current_process,
    current_task, current_user_token, exit_current_and_run_next, pid2process,
    suspend_current_and_run_next,
};
use crate::timer::get_time_ms;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;

pub fn sys_exit(exit_code: i32) -> ! {
    exit_current_and_run_next(exit_code);
    panic!("Unreachable in sys_exit!");
}

pub fn sys_yield() -> isize {
    suspend_current_and_run_next();
    0
}

pub fn sys_get_time() -> isize {
    get_time_ms() as isize
}

pub fn sys_getpid() -> isize {
    current_task().unwrap().process.upgrade().unwrap().getpid() as isize
}

pub fn sys_clone(flags: usize, stack: usize, ptid: usize, tls: usize, ctid: usize) -> isize {
    let Some(args) = CloneArgs::parse(flags, stack, ptid, tls, ctid) else {
        return -1;
    };
    if args.is_thread() {
        sys_clone_thread(args)
    } else {
        sys_clone_process(args)
    }
}

fn sys_clone_process(args: CloneArgs) -> isize {
    let current_process = current_process();
    let new_process = current_process.fork();
    let new_pid = new_process.getpid();
    let child_token = new_process.configure_cloned_main_task(args);

    if args.flags.contains(CloneFlags::CLONE_PARENT_SETTID) {
        let parent_token = current_user_token();
        *translated_refmut(parent_token, args.ptid as *mut i32) = new_pid as i32;
    }
    if args.flags.contains(CloneFlags::CLONE_CHILD_SETTID) {
        *translated_refmut(child_token, args.ctid as *mut i32) = new_pid as i32;
    }
    new_pid as isize
}

fn sys_clone_thread(args: CloneArgs) -> isize {
    let process = current_process();
    let cloned = clone_current_thread(args);
    let process_token = process.attach_task(Arc::clone(&cloned.task));

    if args.flags.contains(CloneFlags::CLONE_PARENT_SETTID) {
        *translated_refmut(process_token, args.ptid as *mut i32) = cloned.tid as i32;
    }
    if args.flags.contains(CloneFlags::CLONE_CHILD_SETTID) {
        *translated_refmut(process_token, args.ctid as *mut i32) = cloned.tid as i32;
    }
    add_task(cloned.task);
    cloned.tid as isize
}

fn translated_string_array(token: usize, mut ptr: *const usize) -> Vec<String> {
    if ptr.is_null() {
        return Vec::new();
    }
    let mut strings = Vec::new();
    loop {
        let string_ptr = *translated_ref(token, ptr);
        if string_ptr == 0 {
            break;
        }
        strings.push(translated_str(token, string_ptr as *const u8));
        unsafe {
            ptr = ptr.add(1);
        }
    }
    strings
}

pub fn sys_exec(path: *const u8, args: *const usize, envs: *const usize) -> isize {
    let process = current_process();
    let token = current_user_token();
    let path = translated_str(token, path);
    let args_vec = translated_string_array(token, args);
    let envs_vec = translated_string_array(token, envs);
    if let Some(app_inode) = open_file_at(process.working_dir(), path.as_str(), OpenFlags::RDONLY) {
        let all_data = app_inode.read_all();
        let argc = args_vec.len();
        process.exec(all_data.as_slice(), args_vec, envs_vec);
        // return argc because cx.x[10] will be covered with it later
        argc as isize
    } else {
        -1
    }
}

/// If there is not a child process whose pid is same as given, return -1.
/// Else if there is a child process but it is still running, return -2.
pub fn sys_waitpid(pid: isize, exit_code_ptr: *mut i32) -> isize {
    let process = current_process();
    // find a child process

    let mut inner = process.inner_exclusive_access();
    if !inner
        .children
        .iter()
        .any(|p| pid == -1 || pid as usize == p.getpid())
    {
        return -1;
        // ---- release current PCB
    }
    let pair = inner.children.iter().enumerate().find(|(_, p)| {
        // ++++ temporarily access child PCB exclusively
        p.inner_exclusive_access().is_zombie && (pid == -1 || pid as usize == p.getpid())
        // ++++ release child PCB
    });
    if let Some((idx, _)) = pair {
        let child = inner.children.remove(idx);
        // confirm that child will be deallocated after being removed from children list
        assert_eq!(Arc::strong_count(&child), 1);
        let found_pid = child.getpid();
        // ++++ temporarily access child PCB exclusively
        let exit_code = child.inner_exclusive_access().exit_code;
        // ++++ release child PCB
        *translated_refmut(inner.memory_set.token(), exit_code_ptr) = exit_code;
        found_pid as isize
    } else {
        -2
    }
    // ---- release current PCB automatically
}

pub fn sys_kill(pid: usize, signal: u32) -> isize {
    if let Some(process) = pid2process(pid) {
        if let Some(flag) = SignalFlags::from_bits(signal) {
            process.inner_exclusive_access().signals |= flag;
            0
        } else {
            -1
        }
    } else {
        -1
    }
}
