use crate::fs::{File, OpenFlags, open_file_in};
use crate::mm::elf_required_interpreter_path;
use crate::syscall::errno::{SysError, SysResult};
use crate::syscall::user_ptr::{PATH_MAX, read_user_c_string, read_user_usize};
use crate::task::{current_process, current_user_token};
use alloc::string::{String, ToString};
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::str;

const ELF_MAGIC: &[u8] = b"\x7fELF";
const SHEBANG_MAGIC: &[u8] = b"#!";
const SHEBANG_RECURSION_LIMIT: usize = 4;

struct ScriptInterpreter {
    path: String,
    optional_arg: Option<String>,
}

fn translated_string_array(token: usize, mut ptr: *const usize) -> SysResult<Vec<String>> {
    if ptr.is_null() {
        return Ok(Vec::new());
    }
    let mut strings = Vec::new();
    loop {
        let string_ptr = read_user_usize(token, ptr as usize)?;
        if string_ptr == 0 {
            break;
        }
        strings.push(read_user_c_string(
            token,
            string_ptr as *const u8,
            PATH_MAX,
        )?);
        unsafe {
            ptr = ptr.add(1);
        }
    }
    Ok(strings)
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
    let snapshot = current_process().path_snapshot();
    let root = libc_test_root(snapshot.cwd_path.as_str(), script_path)?;
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

fn read_exec_file_direct(path: &str) -> SysResult<Vec<u8>> {
    let app_file = open_file_in(
        current_process().path_snapshot().context,
        path,
        OpenFlags::RDONLY,
    )?;
    read_all_file(app_file)
}

fn lmbench_all_redirect() -> &'static str {
    let snapshot = current_process().path_snapshot();
    if snapshot.cwd_path.starts_with("/glibc") {
        "/glibc/lmbench_all"
    } else {
        "/musl/lmbench_all"
    }
}

fn exec_compat_redirect(path: &str) -> Option<&'static str> {
    match path {
        // CONTEXT: The official lmbench wrapper `hello` may contain this
        // build-host absolute path. Redirect it to the libc-local test binary
        // so `lat_proc shell` measures shell+exec instead of console errors.
        "/code/lmbench_src/bin/build/lmbench_all" => Some(lmbench_all_redirect()),
        _ => None,
    }
}

fn read_exec_file(path: &str) -> SysResult<Vec<u8>> {
    match read_exec_file_direct(path) {
        Ok(data) => Ok(data),
        Err(err) => {
            if let Some(target) = exec_compat_redirect(path) {
                read_exec_file_direct(target).or(Err(err))
            } else {
                Err(err)
            }
        }
    }
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
        ("/lib64/ld-musl-loongarch-lp64d.so.1", "/musl/lib/libc.so"),
    ];

    for (alias, target) in REDIRECTS {
        if path == *alias {
            return read_exec_file_direct(target).or_else(|_| read_exec_file_direct(path));
        }
    }
    read_exec_file_direct(path)
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
    let path = read_user_c_string(token, path, PATH_MAX)?;
    let args_vec = translated_string_array(token, args)?;
    let envs_vec = translated_string_array(token, envs)?;
    exec_path(path, args_vec, envs_vec)
}
