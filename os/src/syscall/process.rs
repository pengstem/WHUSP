use crate::fs::{File, OpenFlags, open_file_at};
use crate::mm::{translated_ref, translated_refmut, translated_str};
use crate::task::{
    CloneArgs, CloneFlags, ProcessCpuTimesSnapshot, SignalFlags, add_task, clone_current_thread,
    current_process, current_task, current_user_token, exit_current_and_run_next, pid2process,
    suspend_current_and_run_next,
};
use crate::timer::{get_time_clock_ticks, get_time_us, us_to_clock_ticks};
use alloc::string::{String, ToString};
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::str;

use super::errno::{SysError, SysResult};
use super::fs::user_ptr::write_user_value;

const ELF_MAGIC: &[u8] = b"\x7fELF";
const SHEBANG_MAGIC: &[u8] = b"#!";
const SHEBANG_RECURSION_LIMIT: usize = 4;
const UTS_FIELD_LEN: usize = 65;

struct ScriptInterpreter {
    path: String,
    optional_arg: Option<String>,
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct LinuxUtsName {
    sysname: [u8; UTS_FIELD_LEN],
    nodename: [u8; UTS_FIELD_LEN],
    release: [u8; UTS_FIELD_LEN],
    version: [u8; UTS_FIELD_LEN],
    machine: [u8; UTS_FIELD_LEN],
    domainname: [u8; UTS_FIELD_LEN],
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct LinuxTimeVal {
    tv_sec: isize,
    tv_usec: isize,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct LinuxTimezone {
    tz_minuteswest: i32,
    tz_dsttime: i32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct LinuxTms {
    tms_utime: isize,
    tms_stime: isize,
    tms_cutime: isize,
    tms_cstime: isize,
}

impl LinuxUtsName {
    fn field(value: &str) -> [u8; UTS_FIELD_LEN] {
        let mut field = [0u8; UTS_FIELD_LEN];
        let bytes = value.as_bytes();
        let len = bytes.len().min(UTS_FIELD_LEN - 1);
        field[..len].copy_from_slice(&bytes[..len]);
        field
    }

    fn current() -> Self {
        Self {
            sysname: Self::field("Linux"),
            nodename: Self::field("WHUSP"),
            release: Self::field("6.8.0-whusp"),
            version: Self::field("#1 SMP OSKernel2026"),
            machine: Self::field(machine_name()),
            domainname: Self::field("(none)"),
        }
    }
}

#[cfg(target_arch = "loongarch64")]
fn machine_name() -> &'static str {
    "loongarch64"
}

#[cfg(not(target_arch = "loongarch64"))]
fn machine_name() -> &'static str {
    "riscv64"
}

fn clock_ticks_to_isize(ticks: usize) -> isize {
    ticks.min(isize::MAX as usize) as isize
}

impl LinuxTms {
    fn from_cpu_times(times: ProcessCpuTimesSnapshot) -> Self {
        Self {
            tms_utime: clock_ticks_to_isize(us_to_clock_ticks(times.user_us)),
            tms_stime: clock_ticks_to_isize(us_to_clock_ticks(times.system_us)),
            tms_cutime: clock_ticks_to_isize(us_to_clock_ticks(times.children_user_us)),
            tms_cstime: clock_ticks_to_isize(us_to_clock_ticks(times.children_system_us)),
        }
    }
}

pub fn sys_exit(exit_code: i32) -> ! {
    exit_current_and_run_next(exit_code);
    panic!("Unreachable in sys_exit!");
}

pub fn sys_exit_group(exit_code: i32) -> ! {
    // UNFINISHED: Linux exit_group terminates every thread in the current
    // thread group; this compatibility path currently relies on the existing
    // process-exit behavior and is complete only for single-threaded callers.
    exit_current_and_run_next(exit_code);
    panic!("Unreachable in sys_exit_group!");
}

pub fn sys_sched_yield() -> isize {
    suspend_current_and_run_next();
    0
}

pub fn sys_gettimeofday(tv: *mut LinuxTimeVal, tz: *mut LinuxTimezone) -> SysResult {
    let token = current_user_token();
    if !tv.is_null() {
        // UNFINISHED: Linux gettimeofday reports CLOCK_REALTIME wall-clock time
        // since the Unix epoch; this kernel has no RTC-backed epoch yet, so the
        // value is derived from the monotonic machine timer.
        let current_us = get_time_us();
        let time = LinuxTimeVal {
            tv_sec: (current_us / 1_000_000) as isize,
            tv_usec: (current_us % 1_000_000) as isize,
        };
        write_user_value(token, tv, &time)?;
    }
    if !tz.is_null() {
        // CONTEXT: Linux keeps the timezone argument only for legacy callers.
        // This kernel has no timezone state, so report UTC-compatible zeroes.
        write_user_value(token, tz, &LinuxTimezone::default())?;
    }
    Ok(0)
}

pub fn sys_getpid() -> isize {
    current_task().unwrap().process.upgrade().unwrap().getpid() as isize
}

pub fn sys_getppid() -> isize {
    // UNFINISHED: PID namespaces and child subreapers are not modeled yet, so
    // this returns the single-namespace parent recorded in the PCB.
    current_process().getppid() as isize
}

pub fn sys_uname(name: *mut LinuxUtsName) -> SysResult {
    // UNFINISHED: UTS namespaces, sethostname/setdomainname, and Linux
    // personality-based uname release overrides are not implemented.
    write_user_value(current_user_token(), name, &LinuxUtsName::current())?;
    Ok(0)
}

pub fn sys_times(tms: *mut LinuxTms) -> SysResult {
    if !tms.is_null() {
        let linux_tms = LinuxTms::from_cpu_times(current_process().cpu_times_snapshot());
        write_user_value(current_user_token(), tms, &linux_tms)?;
    }
    Ok(clock_ticks_to_isize(get_time_clock_ticks()))
}

pub fn sys_clone(flags: usize, stack: usize, ptid: usize, tls: usize, ctid: usize) -> SysResult {
    let Some(args) = CloneArgs::parse(flags, stack, ptid, tls, ctid) else {
        return Err(SysError::EINVAL);
    };
    if args.is_thread() {
        sys_clone_thread(args)
    } else {
        sys_clone_process(args)
    }
}

fn sys_clone_process(args: CloneArgs) -> SysResult {
    let current_process = current_process();
    let child_parent = if args.flags.contains(CloneFlags::CLONE_PARENT) {
        current_process.parent_process().ok_or(SysError::EINVAL)?
    } else {
        Arc::clone(&current_process)
    };
    let new_process = current_process.fork(child_parent);
    let new_pid = new_process.getpid();
    let child_token = new_process.configure_cloned_main_task(args);

    if args.flags.contains(CloneFlags::CLONE_PARENT_SETTID) {
        let parent_token = current_user_token();
        *translated_refmut(parent_token, args.ptid as *mut i32) = new_pid as i32;
    }
    if args.flags.contains(CloneFlags::CLONE_CHILD_SETTID) {
        *translated_refmut(child_token, args.ctid as *mut i32) = new_pid as i32;
    }
    Ok(new_pid as isize)
}

fn sys_clone_thread(args: CloneArgs) -> SysResult {
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
    Ok(cloned.tid as isize)
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

fn is_space_or_tab(byte: u8) -> bool {
    byte == b' ' || byte == b'\t'
}

fn trim_script_line(line: &[u8]) -> &[u8] {
    let mut end = line.len();
    while end > 0 && (is_space_or_tab(line[end - 1]) || line[end - 1] == b'\r') {
        end -= 1;
    }
    &line[..end]
}

fn parse_shebang(data: &[u8]) -> SysResult<Option<ScriptInterpreter>> {
    if !data.starts_with(SHEBANG_MAGIC) {
        return Ok(None);
    }

    let line_end = data
        .iter()
        .position(|byte| *byte == b'\n')
        .unwrap_or(data.len());
    let line = trim_script_line(&data[SHEBANG_MAGIC.len()..line_end]);
    let Some(name_start) = line.iter().position(|byte| !is_space_or_tab(*byte)) else {
        return Err(SysError::ENOEXEC);
    };
    let rest = &line[name_start..];
    let name_end = rest
        .iter()
        .position(|byte| is_space_or_tab(*byte))
        .unwrap_or(rest.len());
    let interpreter = str::from_utf8(&rest[..name_end]).map_err(|_| SysError::ENOEXEC)?;
    if interpreter.is_empty() {
        return Err(SysError::ENOEXEC);
    }

    let optional_arg = rest[name_end..]
        .iter()
        .position(|byte| !is_space_or_tab(*byte))
        .map(|arg_start| {
            str::from_utf8(&rest[name_end + arg_start..])
                .map(|arg| arg.to_string())
                .map_err(|_| SysError::ENOEXEC)
        })
        .transpose()?;

    Ok(Some(ScriptInterpreter {
        path: interpreter.to_string(),
        optional_arg,
    }))
}

fn is_path_under(path: &str, root: &str) -> bool {
    match path.strip_prefix(root) {
        Some("") => true,
        Some(rest) => rest.starts_with('/'),
        None => false,
    }
}

fn libc_test_root(cwd_path: &str, script_path: &str) -> Option<&'static str> {
    if is_path_under(script_path, "/musl") || is_path_under(cwd_path, "/musl") {
        Some("/musl")
    } else if is_path_under(script_path, "/glibc") || is_path_under(cwd_path, "/glibc") {
        Some("/glibc")
    } else {
        None
    }
}

fn busybox_fallback(
    interpreter: &ScriptInterpreter,
    script_path: &str,
) -> Option<(String, Vec<String>)> {
    let cwd_path = current_process().working_dir_path();
    let root = libc_test_root(cwd_path.as_str(), script_path)?;
    let mut busybox_path = String::from(root);
    busybox_path.push_str("/busybox");

    let mut args = Vec::new();
    args.push(busybox_path.clone());
    match interpreter.path.as_str() {
        "/bin/sh" | "/bin/bash" | "sh" | "bash" => args.push(String::from("sh")),
        "/busybox" | "/bin/busybox" | "busybox" => {}
        _ => return None,
    }
    if let Some(optional_arg) = interpreter.optional_arg.as_ref() {
        args.push(optional_arg.clone());
    }
    Some((busybox_path, args))
}

fn interpreter_candidates(
    interpreter: &ScriptInterpreter,
    script_path: &str,
) -> Vec<(String, Vec<String>)> {
    let mut direct_args = Vec::new();
    direct_args.push(interpreter.path.clone());
    if let Some(optional_arg) = interpreter.optional_arg.as_ref() {
        direct_args.push(optional_arg.clone());
    }

    let mut candidates = Vec::new();
    candidates.push((interpreter.path.clone(), direct_args));
    if let Some(fallback) = busybox_fallback(interpreter, script_path) {
        // CONTEXT: Official-style test disks put shell-capable BusyBox under
        // `/musl` or `/glibc` instead of providing a real `/bin/sh`.
        candidates.push(fallback);
    }
    candidates
}

fn read_exec_file(path: &str) -> SysResult<Vec<u8>> {
    let process = current_process();
    let app_file = open_file_at(process.working_dir(), path, OpenFlags::RDONLY)?;
    Ok(read_all_file(app_file))
}

fn read_all_file(file: Arc<dyn File + Send + Sync>) -> Vec<u8> {
    let mut data = Vec::new();
    data.resize(file.stat().size as usize, 0);
    let len = file.read_at(0, data.as_mut_slice());
    data.truncate(len);
    data
}

fn append_script_args(
    mut args: Vec<String>,
    script_path: String,
    original_args: Vec<String>,
) -> Vec<String> {
    args.push(script_path);
    args.extend(original_args.into_iter().skip(1));
    args
}

fn exec_script(
    script_path: String,
    args: Vec<String>,
    envs: Vec<String>,
    interpreter: ScriptInterpreter,
    depth: usize,
) -> SysResult {
    if depth >= SHEBANG_RECURSION_LIMIT {
        return Err(SysError::ELOOP);
    }

    for (interpreter_path, candidate_args) in
        interpreter_candidates(&interpreter, script_path.as_str())
    {
        let Ok(interpreter_data) = read_exec_file(interpreter_path.as_str()) else {
            continue;
        };
        let next_args = append_script_args(candidate_args, script_path, args);
        return exec_loaded_program(
            interpreter_path,
            next_args,
            envs,
            depth + 1,
            interpreter_data,
        );
    }

    Err(SysError::ENOENT)
}

fn exec_loaded_program(
    path: String,
    args: Vec<String>,
    envs: Vec<String>,
    depth: usize,
    data: Vec<u8>,
) -> SysResult {
    if data.starts_with(ELF_MAGIC) {
        current_process().exec(data.as_slice(), args, envs);
        // CONTEXT: Linux execve starts a new image instead of returning to the
        // old program. On RISC-V glibc, entry a0 is rtld_fini; argc/argv/envp
        // are read from the initial user stack built by ProcessControlBlock::exec.
        return Ok(0);
    }

    let Some(interpreter) = parse_shebang(data.as_slice())? else {
        return Err(SysError::ENOEXEC);
    };
    exec_script(path, args, envs, interpreter, depth)
}

fn exec_path(path: String, args: Vec<String>, envs: Vec<String>) -> SysResult {
    let data = read_exec_file(path.as_str())?;
    exec_loaded_program(path, args, envs, 0, data)
}

pub fn sys_execve(path: *const u8, args: *const usize, envs: *const usize) -> SysResult {
    let token = current_user_token();
    let path = translated_str(token, path);
    let args_vec = translated_string_array(token, args);
    let envs_vec = translated_string_array(token, envs);
    exec_path(path, args_vec, envs_vec)
}

pub fn sys_kill(pid: usize, signal: u32) -> SysResult {
    if let Some(process) = pid2process(pid) {
        if let Some(flag) = SignalFlags::from_bits(signal) {
            process.inner_exclusive_access().signals |= flag;
            Ok(0)
        } else {
            Err(SysError::EINVAL)
        }
    } else {
        Err(SysError::ESRCH)
    }
}
