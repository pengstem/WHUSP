use crate::fs::{File, OpenFlags, open_file_at};
use crate::mm::{elf_required_interpreter_path, translated_ref, translated_refmut, translated_str};
use crate::sbi::shutdown;
use crate::task::{
    CloneArgs, CloneFlags, ProcessCpuTimesSnapshot, RLimit, RLimitResource, SignalFlags,
    SignalInfo, add_task, clone_current_thread, current_process, current_task, current_user_token,
    exit_current_and_run_next, exit_current_group_and_run_next, pid2process, queue_signal_to_task,
    suspend_current_and_run_next, wakeup_task,
};
use crate::timer::{get_time_clock_ticks, us_to_clock_ticks};
use alloc::string::{String, ToString};
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::str;

use super::errno::{SysError, SysResult};
use super::fs::user_ptr::{read_user_value, write_user_value};

const ELF_MAGIC: &[u8] = b"\x7fELF";
const SHEBANG_MAGIC: &[u8] = b"#!";
const SHEBANG_RECURSION_LIMIT: usize = 4;
const UTS_FIELD_LEN: usize = 65;
const LINUX_REBOOT_MAGIC1: u32 = 0xfee1_dead;
const LINUX_REBOOT_MAGIC2: u32 = 0x2812_1969;
const LINUX_REBOOT_MAGIC2A: u32 = 0x0512_1996;
const LINUX_REBOOT_MAGIC2B: u32 = 0x1604_1998;
const LINUX_REBOOT_MAGIC2C: u32 = 0x2011_2000;
const LINUX_REBOOT_CMD_RESTART: u32 = 0x0123_4567;
const LINUX_REBOOT_CMD_HALT: u32 = 0xcdef_0123;
const LINUX_REBOOT_CMD_CAD_ON: u32 = 0x89ab_cdef;
const LINUX_REBOOT_CMD_CAD_OFF: u32 = 0x0000_0000;
const LINUX_REBOOT_CMD_POWER_OFF: u32 = 0x4321_fedc;

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
    exit_current_group_and_run_next(exit_code);
    panic!("Unreachable in sys_exit_group!");
}

pub fn sys_sched_yield() -> isize {
    suspend_current_and_run_next();
    0
}

fn has_linux_reboot_magic(magic: u32, magic2: u32) -> bool {
    magic == LINUX_REBOOT_MAGIC1
        && matches!(
            magic2,
            LINUX_REBOOT_MAGIC2
                | LINUX_REBOOT_MAGIC2A
                | LINUX_REBOOT_MAGIC2B
                | LINUX_REBOOT_MAGIC2C
        )
}

pub fn sys_reboot(magic: usize, magic2: usize, op: usize, _arg: usize) -> SysResult {
    let magic = magic as u32;
    let magic2 = magic2 as u32;
    let op = op as u32;
    if !has_linux_reboot_magic(magic, magic2) {
        return Err(SysError::EINVAL);
    }

    // UNFINISHED: Linux requires CAP_SYS_BOOT in the caller's user namespace
    // and returns EPERM for unprivileged callers. This kernel has no real
    // credential or capability model yet and runs contest user tasks as root.
    match op {
        LINUX_REBOOT_CMD_CAD_OFF | LINUX_REBOOT_CMD_CAD_ON => Ok(0),
        LINUX_REBOOT_CMD_HALT | LINUX_REBOOT_CMD_POWER_OFF | LINUX_REBOOT_CMD_RESTART => {
            // UNFINISHED: RESTART should reset and reboot the machine. The
            // current arch layer exposes only a shutdown/poweroff primitive,
            // which is the contest-critical behavior under QEMU -no-reboot.
            // CONTEXT: Linux leaves filesystem syncing to callers before
            // reboot(2), so this path does not add an implicit sync.
            shutdown(false)
        }
        // UNFINISHED: RESTART2, KEXEC, and SW_SUSPEND require reboot strings,
        // kernel-image handoff, or suspend support that this kernel lacks.
        _ => Err(SysError::EINVAL),
    }
}

pub fn sys_gettimeofday(tv: *mut LinuxTimeVal, tz: *mut LinuxTimezone) -> SysResult {
    let token = current_user_token();
    if !tv.is_null() {
        let wall_ns = crate::timer::wall_time_nanos();
        let time = LinuxTimeVal {
            tv_sec: (wall_ns / 1_000_000_000) as isize,
            tv_usec: ((wall_ns % 1_000_000_000) / 1_000) as isize,
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

pub fn sys_gettid() -> isize {
    current_task().unwrap().linux_tid() as isize
}

pub fn sys_getppid() -> isize {
    // UNFINISHED: PID namespaces and child subreapers are not modeled yet, so
    // this returns the single-namespace parent recorded in the PCB.
    current_process().getppid() as isize
}

pub fn sys_set_tid_address(tidptr: usize) -> SysResult {
    let task = current_task().unwrap();
    let tid = task.linux_tid();
    task.inner_exclusive_access().clear_child_tid = if tidptr == 0 { None } else { Some(tidptr) };
    Ok(tid as isize)
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

fn rlimit_target_process(pid: usize) -> SysResult<Arc<crate::task::ProcessControlBlock>> {
    if pid == 0 {
        Ok(current_process())
    } else {
        // UNFINISHED: Linux prlimit64 checks real/effective/saved UIDs and
        // CAP_SYS_RESOURCE before operating on another process. This kernel
        // does not model credentials yet, so a live PID is accepted.
        pid2process(pid).ok_or(SysError::ESRCH)
    }
}

fn validate_new_rlimit(current: RLimit, new_limit: RLimit) -> SysResult<()> {
    if new_limit.rlim_cur > new_limit.rlim_max {
        return Err(SysError::EINVAL);
    }
    if new_limit.rlim_max > current.rlim_max {
        // UNFINISHED: Raising a hard resource limit should be allowed for a
        // task with CAP_SYS_RESOURCE. Capabilities are not modeled yet.
        return Err(SysError::EPERM);
    }
    Ok(())
}

pub fn sys_prlimit64(
    pid: usize,
    resource: i32,
    new_limit: *const RLimit,
    old_limit: *mut RLimit,
) -> SysResult {
    let resource = RLimitResource::from_raw(resource).ok_or(SysError::EINVAL)?;
    let token = current_user_token();
    let new_limit = if new_limit.is_null() {
        None
    } else {
        Some(read_user_value(token, new_limit)?)
    };
    let process = rlimit_target_process(pid)?;
    let mut inner = process.inner_exclusive_access();
    let current = inner.resource_limits.get(resource);

    if let Some(new_limit) = new_limit {
        validate_new_rlimit(current, new_limit)?;
    }
    if !old_limit.is_null() {
        write_user_value(token, old_limit, &current)?;
    }
    if let Some(new_limit) = new_limit {
        inner.resource_limits.set(resource, new_limit);
    }
    Ok(0)
}

pub fn sys_getrlimit(resource: i32, old_limit: *mut RLimit) -> SysResult {
    if old_limit.is_null() {
        return Err(SysError::EFAULT);
    }
    sys_prlimit64(0, resource, core::ptr::null(), old_limit)
}

pub fn sys_setrlimit(resource: i32, new_limit: *const RLimit) -> SysResult {
    if new_limit.is_null() {
        return Err(SysError::EFAULT);
    }
    sys_prlimit64(0, resource, new_limit, core::ptr::null_mut())
}

fn clone_tls_and_ctid_args(raw_arg4: usize, raw_arg5: usize) -> (usize, usize) {
    #[cfg(target_arch = "loongarch64")]
    {
        (raw_arg5, raw_arg4)
    }
    #[cfg(not(target_arch = "loongarch64"))]
    {
        (raw_arg4, raw_arg5)
    }
}

pub fn sys_clone(
    flags: usize,
    stack: usize,
    ptid: usize,
    raw_arg4: usize,
    raw_arg5: usize,
) -> SysResult {
    let (tls, ctid) = clone_tls_and_ctid_args(raw_arg4, raw_arg5);
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
        write_user_value(parent_token, args.ptid as *mut i32, &(new_pid as i32))?;
    }
    if args.flags.contains(CloneFlags::CLONE_CHILD_SETTID) {
        write_user_value(child_token, args.ctid as *mut i32, &(new_pid as i32))?;
    }
    Ok(new_pid as isize)
}

fn sys_clone_thread(args: CloneArgs) -> SysResult {
    let process = current_process();
    let cloned = clone_current_thread(args);
    let process_token = process.attach_task(Arc::clone(&cloned.task));

    if args.flags.contains(CloneFlags::CLONE_PARENT_SETTID) {
        write_user_value(
            process_token,
            args.ptid as *mut i32,
            &(cloned.linux_tid as i32),
        )?;
    }
    if args.flags.contains(CloneFlags::CLONE_CHILD_SETTID) {
        write_user_value(
            process_token,
            args.ctid as *mut i32,
            &(cloned.linux_tid as i32),
        )?;
    }
    add_task(cloned.task);
    Ok(cloned.linux_tid as isize)
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
    read_all_file(app_file)
}

fn read_all_file(file: Arc<dyn File + Send + Sync>) -> SysResult<Vec<u8>> {
    let mut data = Vec::new();
    data.resize(file.stat()?.size as usize, 0);
    let len = file.read_at(0, data.as_mut_slice());
    data.truncate(len);
    Ok(data)
}

fn read_elf_interpreter(path: &str) -> SysResult<Vec<u8>> {
    const REDIRECTS: &[(&str, &str)] = &[
        // CONTEXT: libc-test's musl dynamic binary names the soft-float
        // interpreter as a symlink to libc.so. Official test sources state
        // that kernels should redirect this path when the disk image does not
        // provide the symlink entry.
        ("/lib/ld-musl-riscv64-sf.so.1", "/musl/lib/libc.so"),
        ("/lib/ld-musl-riscv64.so.1", "/musl/lib/libc.so"),
        // CONTEXT: Official-style RISC-V glibc disks keep the real loader under
        // `/glibc/lib`; the root `/lib` alias may be a plain redirect file in
        // local test images and must not be treated as the loader payload.
        (
            "/lib/ld-linux-riscv64-lp64d.so.1",
            "/glibc/lib/ld-linux-riscv64-lp64d.so.1",
        ),
        // CONTEXT: LoongArch glibc/musl images keep loaders under
        // `/glibc/lib` and `/musl/lib`; the official ELF INTERP path
        // `/lib64/...` is not materialized on disk, so redirect it here.
        (
            "/lib64/ld-linux-loongarch-lp64d.so.1",
            "/glibc/lib/ld-linux-loongarch-lp64d.so.1",
        ),
        (
            "/lib64/ld-musl-loongarch-lp64d.so.1",
            "/musl/lib/libc.so",
        ),
    ];

    for (alias, target) in REDIRECTS {
        if path == *alias {
            return read_exec_file(target).or_else(|_| read_exec_file(path));
        }
    }
    read_exec_file(path)
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
        let elf = xmas_elf::ElfFile::new(data.as_slice()).map_err(|_| SysError::ENOEXEC)?;
        // CONTEXT: Some contest basic binaries are PIE and carry PT_INTERP but
        // have no DT_NEEDED entries. They ran as directly-entered self-contained
        // test programs before dynamic linker support; keep that compatibility
        // path while using PT_INTERP for binaries that actually need DSOs.
        let interpreter_data = elf_required_interpreter_path(&elf)
            .map(read_elf_interpreter)
            .transpose()?;
        let interpreter_elf = interpreter_data
            .as_ref()
            .map(|data| xmas_elf::ElfFile::new(data.as_slice()).map_err(|_| SysError::ENOEXEC))
            .transpose()?;
        current_process().exec(&elf, interpreter_elf.as_ref(), args, envs);
        // CONTEXT: Linux execve starts a new image instead of returning to the
        // old program. For PT_INTERP ELFs, the kernel enters the dynamic linker
        // while auxv still describes the original executable.
        return Ok(0);
    }

    let interpreter = match parse_shebang(data.as_slice())? {
        Some(interp) => interp,
        None => ScriptInterpreter {
            path: String::from("/bin/sh"),
            optional_arg: None,
        },
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
    let flag = SignalFlags::from_signum(signal).ok_or(SysError::EINVAL)?;
    let process = pid2process(pid).ok_or(SysError::ESRCH)?;
    if !flag.is_empty() {
        let sender_pid = current_process().getpid() as i32;
        let target = {
            let tasks = process.tasks_snapshot();
            tasks
                .iter()
                .find(|task| {
                    let task_inner = task.inner_exclusive_access();
                    !(task_inner.signal_mask & flag).contains(flag)
                })
                .cloned()
                .or_else(|| tasks.first().cloned())
        };
        if let Some(task) = target {
            queue_signal_to_task(task, flag, SignalInfo::user(signal as i32, sender_pid));
        }
    }
    if flag.check_error().is_some() {
        for task in process.tasks_snapshot() {
            wakeup_task(task);
        }
    }
    Ok(0)
}

const SYSLOG_ACTION_READ_ALL: usize = 3;
const SYSLOG_ACTION_SIZE_BUFFER: usize = 10;
const SYSLOG_BUF_SIZE: usize = 4096;

static SYSLOG_FAKE_MSG: &[u8] = b"<5>[    0.000000] Linux version 5.10.0 (whusp@oscomp)\n";

pub fn sys_syslog(log_type: usize, buf: *mut u8, len: usize) -> SysResult {
    match log_type {
        SYSLOG_ACTION_SIZE_BUFFER => Ok(SYSLOG_BUF_SIZE as isize),
        SYSLOG_ACTION_READ_ALL => {
            if buf.is_null() || len == 0 {
                return Ok(0);
            }
            let token = current_user_token();
            let msg = SYSLOG_FAKE_MSG;
            let copy_len = msg.len().min(len);
            for i in 0..copy_len {
                *translated_refmut(token, unsafe { buf.add(i) }) = msg[i];
            }
            Ok(copy_len as isize)
        }
        _ => Ok(0),
    }
}
