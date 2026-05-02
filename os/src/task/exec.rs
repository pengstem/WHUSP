use super::process::ProcessControlBlock;
use crate::config::PAGE_SIZE;
use crate::mm::{ElfLoadInfo, KERNEL_SPACE, MemorySet, translated_refmut};
use crate::trap::{TrapContext, trap_handler};
use alloc::string::String;
use alloc::sync::Arc;
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

pub(super) struct ExecStackInfo {
    pub(super) entry_point: usize,
    pub(super) phdr: usize,
    pub(super) phent: usize,
    pub(super) phnum: usize,
    pub(super) interp_base: usize,
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

pub(super) fn init_user_stack(
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
        (AT_BASE, stack_info.interp_base),
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

impl ProcessControlBlock {
    /// Only support processes with a single thread.
    pub fn exec(
        self: &Arc<Self>,
        elf_data: &[u8],
        interpreter_data: Option<&[u8]>,
        args: Vec<String>,
        envs: Vec<String>,
    ) {
        assert_eq!(self.inner_exclusive_access().thread_count(), 1);
        let ElfLoadInfo {
            memory_set,
            ustack_base,
            entry_point,
            aux_entry,
            phdr,
            phent,
            phnum,
            interp_base,
        } = MemorySet::from_elf(elf_data, interpreter_data);
        let stack_info = ExecStackInfo {
            entry_point: aux_entry,
            phdr,
            phent,
            phnum,
            interp_base,
        };
        let new_token = memory_set.token();

        {
            let mut inner = self.inner_exclusive_access();
            inner.memory_set = memory_set;
            inner.cmdline = args.clone();
            for fd in inner.fd_table.iter_mut() {
                if fd
                    .as_ref()
                    .map(|entry| entry.close_on_exec())
                    .unwrap_or(false)
                {
                    *fd = None;
                }
            }
        }

        let task = self.inner_exclusive_access().get_task(0);
        let mut task_inner = task.inner_exclusive_access();
        task_inner.res.as_mut().unwrap().ustack_base = ustack_base;
        task_inner.res.as_mut().unwrap().alloc_user_res();
        task_inner.trap_cx_ppn = task_inner.res.as_mut().unwrap().trap_cx_ppn();
        let (user_sp, _, _) = init_user_stack(
            new_token,
            task_inner.res.as_ref().unwrap().ustack_top(),
            &args,
            &envs,
            &stack_info,
        );

        let trap_cx = TrapContext::app_init_context(
            entry_point,
            user_sp,
            KERNEL_SPACE.exclusive_access().token(),
            task.kstack.get_top(),
            trap_handler as usize,
        );
        *task_inner.get_trap_cx() = trap_cx;
    }
}
