use super::{
    SigAltStack, SignalAction, current_task, prepare_exec_thread_group,
    process::{ProcessControlBlock, comm_from_cmdline, empty_process_pkey_rights},
};
use crate::config::{PAGE_SIZE, USER_STACK_SIZE};
use crate::fs::{File, VfsNodeId, track_regular_file_executable, untrack_regular_file_executable};
use crate::mm::{ElfLoadInfo, KERNEL_SPACE, MemorySet};
use crate::syscall::errno::{SysError, SysResult};
use crate::syscall::user_ptr::{copy_to_user, write_user_value};
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
const AT_HWCAP: usize = 16;
const AT_CLKTCK: usize = 17;
const AT_SECURE: usize = 23;
const AT_RANDOM: usize = 25;

#[cfg(target_arch = "riscv64")]
const fn riscv_hwcap(letter: u8) -> usize {
    1usize << (letter - b'A')
}

#[cfg(target_arch = "riscv64")]
const ELF_HWCAP: usize = riscv_hwcap(b'I')
    | riscv_hwcap(b'M')
    | riscv_hwcap(b'A')
    | riscv_hwcap(b'F')
    | riscv_hwcap(b'D')
    | riscv_hwcap(b'C');
#[cfg(not(target_arch = "riscv64"))]
const ELF_HWCAP: usize = 0;

pub(super) struct ExecStackInfo {
    pub(super) at_entry: usize,
    pub(super) phdr: usize,
    pub(super) phent: usize,
    pub(super) phnum: usize,
    pub(super) interp_base: usize,
    pub(super) uid: u32,
    pub(super) euid: u32,
    pub(super) gid: u32,
    pub(super) egid: u32,
}

fn align_down(value: usize, align: usize) -> usize {
    value & !(align - 1)
}

struct ExecStackLayout {
    user_sp: usize,
    argv_base: usize,
    envp_base: usize,
    auxv_base: usize,
    random_addr: usize,
    arg_ptrs: Vec<usize>,
    env_ptrs: Vec<usize>,
    auxv: [(usize, usize); 15],
}

fn stack_low(stack_top: usize) -> SysResult<usize> {
    // UNFINISHED: Linux derives argv/env limits from ARG_MAX, MAX_ARG_STRLEN,
    // and RLIMIT_STACK. This contest path only rejects layouts that cannot fit
    // in the eagerly mapped user stack, so oversized arguments return E2BIG
    // instead of underflowing the stack writer or panicking in kernel space.
    stack_top
        .checked_sub(USER_STACK_SIZE)
        .ok_or(SysError::E2BIG)
}

fn checked_stack_sub(sp: usize, amount: usize, low: usize) -> SysResult<usize> {
    let next = sp.checked_sub(amount).ok_or(SysError::E2BIG)?;
    if next < low {
        return Err(SysError::E2BIG);
    }
    Ok(next)
}

fn checked_stack_align_down(sp: usize, align: usize, low: usize) -> SysResult<usize> {
    let next = align_down(sp, align);
    if next < low {
        return Err(SysError::E2BIG);
    }
    Ok(next)
}

fn checked_table_size(arg_count: usize, env_count: usize, auxv_count: usize) -> SysResult<usize> {
    let word = core::mem::size_of::<usize>();
    let words = 1usize
        .checked_add(arg_count.checked_add(1).ok_or(SysError::E2BIG)?)
        .and_then(|value| value.checked_add(env_count.checked_add(1)?))
        .and_then(|value| value.checked_add(auxv_count.checked_add(1)?.checked_mul(2)?))
        .ok_or(SysError::E2BIG)?;
    words.checked_mul(word).ok_or(SysError::E2BIG)
}

fn plan_user_string(user_sp: &mut usize, stack_low: usize, string: &str) -> SysResult<usize> {
    let string_len = string.len().checked_add(1).ok_or(SysError::E2BIG)?;
    *user_sp = checked_stack_sub(*user_sp, string_len, stack_low)?;
    let addr = *user_sp;
    *user_sp = checked_stack_align_down(*user_sp, core::mem::size_of::<usize>(), stack_low)?;
    Ok(addr)
}

fn plan_user_strings(
    user_sp: &mut usize,
    stack_low: usize,
    strings: &[String],
) -> SysResult<Vec<usize>> {
    let mut ptrs = Vec::with_capacity(strings.len());
    for string in strings {
        ptrs.push(plan_user_string(user_sp, stack_low, string.as_str())?);
    }
    Ok(ptrs)
}

fn plan_user_stack(
    stack_top: usize,
    args: &[String],
    envs: &[String],
    stack_info: &ExecStackInfo,
) -> SysResult<ExecStackLayout> {
    let stack_low = stack_low(stack_top)?;
    let mut string_sp = stack_top;
    let env_ptrs = plan_user_strings(&mut string_sp, stack_low, envs)?;
    let arg_ptrs = plan_user_strings(&mut string_sp, stack_low, args)?;

    string_sp = checked_stack_sub(string_sp, 16, stack_low)?;
    let random_addr = string_sp;

    let auxv = [
        (AT_PHDR, stack_info.phdr),
        (AT_PHENT, stack_info.phent),
        (AT_PHNUM, stack_info.phnum),
        (AT_PAGESZ, PAGE_SIZE),
        (AT_ENTRY, stack_info.at_entry),
        (AT_BASE, stack_info.interp_base),
        (AT_FLAGS, 0),
        (AT_UID, stack_info.uid as usize),
        (AT_EUID, stack_info.euid as usize),
        (AT_GID, stack_info.gid as usize),
        (AT_EGID, stack_info.egid as usize),
        (AT_HWCAP, ELF_HWCAP),
        (AT_CLKTCK, 100),
        (AT_SECURE, 0),
        (AT_RANDOM, random_addr),
    ];

    let word = core::mem::size_of::<usize>();
    let table_size = checked_table_size(arg_ptrs.len(), env_ptrs.len(), auxv.len())?;
    let table_top = checked_stack_align_down(string_sp, 16, stack_low)?;
    let table_bottom = checked_stack_sub(table_top, table_size, stack_low)?;
    let user_sp = checked_stack_align_down(table_bottom, 16, stack_low)?;
    let argv_base = user_sp + word;
    let envp_base = argv_base + (arg_ptrs.len() + 1) * word;
    let auxv_base = envp_base + (env_ptrs.len() + 1) * word;

    Ok(ExecStackLayout {
        user_sp,
        argv_base,
        envp_base,
        auxv_base,
        random_addr,
        arg_ptrs,
        env_ptrs,
        auxv,
    })
}

fn write_user_byte(token: usize, addr: usize, value: u8) -> SysResult<()> {
    copy_to_user(token, addr as *mut u8, &[value])
}

fn write_user_usize(token: usize, addr: usize, value: usize) -> SysResult<()> {
    write_user_value(token, addr as *mut usize, &value)
}

fn write_user_bytes(token: usize, addr: usize, bytes: &[u8]) -> SysResult<()> {
    copy_to_user(token, addr as *mut u8, bytes)
}

fn write_user_string(token: usize, addr: usize, string: &str) -> SysResult<()> {
    write_user_bytes(token, addr, string.as_bytes())?;
    write_user_byte(token, addr + string.len(), 0)
}

fn write_user_stack(
    token: usize,
    layout: &ExecStackLayout,
    args: &[String],
    envs: &[String],
) -> SysResult<()> {
    for (addr, string) in layout.env_ptrs.iter().zip(envs.iter()) {
        write_user_string(token, *addr, string.as_str())?;
    }
    for (addr, string) in layout.arg_ptrs.iter().zip(args.iter()) {
        write_user_string(token, *addr, string.as_str())?;
    }

    write_user_bytes(token, layout.random_addr, &[0u8; 16])?;

    let word = core::mem::size_of::<usize>();
    write_user_usize(token, layout.user_sp, layout.arg_ptrs.len())?;
    for (i, ptr) in layout.arg_ptrs.iter().enumerate() {
        write_user_usize(token, layout.argv_base + i * word, *ptr)?;
    }
    write_user_usize(token, layout.argv_base + layout.arg_ptrs.len() * word, 0)?;

    for (i, ptr) in layout.env_ptrs.iter().enumerate() {
        write_user_usize(token, layout.envp_base + i * word, *ptr)?;
    }
    write_user_usize(token, layout.envp_base + layout.env_ptrs.len() * word, 0)?;

    for (i, (key, value)) in layout.auxv.iter().enumerate() {
        let entry = layout.auxv_base + i * 2 * word;
        write_user_usize(token, entry, *key)?;
        write_user_usize(token, entry + word, *value)?;
    }
    let null_entry = layout.auxv_base + layout.auxv.len() * 2 * word;
    write_user_usize(token, null_entry, AT_NULL)?;
    write_user_usize(token, null_entry + word, 0)?;

    Ok(())
}

pub(super) fn init_user_stack(
    token: usize,
    stack_top: usize,
    args: &[String],
    envs: &[String],
    stack_info: &ExecStackInfo,
) -> SysResult<(usize, usize, usize)> {
    let layout = plan_user_stack(stack_top, args, envs, stack_info)?;
    write_user_stack(token, &layout, args, envs)?;
    Ok((layout.user_sp, layout.argv_base, layout.envp_base))
}

impl ProcessControlBlock {
    /// Replaces the current process image with a new ELF image.
    ///
    /// The caller has already resolved the executable/interpreter and copied
    /// argv/envp from userspace. This function owns the process image switch:
    /// memory set replacement, close-on-exec cleanup, signal reset, task resource
    /// rebuild, and initial trap context construction.
    pub fn exec(
        self: &Arc<Self>,
        elf: &xmas_elf::ElfFile<'_>,
        executable_file: Arc<dyn File + Send + Sync>,
        executable_file_size: usize,
        interpreter: Option<(&xmas_elf::ElfFile<'_>, Arc<dyn File + Send + Sync>, usize)>,
        args: Vec<String>,
        envs: Vec<String>,
        executable_node: Option<VfsNodeId>,
    ) -> SysResult<()> {
        let current = current_task().ok_or(SysError::ESRCH)?;
        let process_token = self.inner_exclusive_access().get_user_token();
        let task = prepare_exec_thread_group(self, current, process_token, self.getpid())?;

        let ElfLoadInfo {
            memory_set,
            ustack_base,
            entry_point,
            program_entry,
            phdr,
            phent,
            phnum,
            interp_base,
        } = MemorySet::from_elf_lazy(elf, executable_file, executable_file_size, interpreter)
            .ok_or(SysError::ENOEXEC)?;
        let stack_info = ExecStackInfo {
            at_entry: program_entry,
            phdr,
            phent,
            phnum,
            interp_base,
            uid: self.credentials().ruid,
            euid: self.credentials().euid,
            gid: self.credentials().rgid,
            egid: self.credentials().egid,
        };
        let expected_user_stack_top = ustack_base
            .checked_add(USER_STACK_SIZE)
            .ok_or(SysError::E2BIG)?;
        let stack_layout = plan_user_stack(expected_user_stack_top, &args, &envs, &stack_info)?;
        let new_token = memory_set.token();

        let previous_executable_node = {
            let mut inner = self.inner_exclusive_access();
            inner.memory_set = memory_set;
            inner.pkey_rights = empty_process_pkey_rights();
            let previous = core::mem::replace(&mut inner.executable_node, executable_node);
            inner.cmdline = args.clone();
            inner.comm = comm_from_cmdline(&args);
            inner.timers.clear_posix_after_exec();
            for action in inner.signal_actions.iter_mut() {
                if action.has_user_handler() {
                    *action = SignalAction::default();
                }
            }
            for fd in inner.fd_table.iter_mut() {
                if fd
                    .as_ref()
                    .map(|entry| entry.close_on_exec())
                    .unwrap_or(false)
                {
                    *fd = None;
                }
            }
            previous
        };
        if let Some(node) = previous_executable_node {
            untrack_regular_file_executable(node);
        }
        if let Some(node) = executable_node {
            track_regular_file_executable(node);
        }

        let mut task_inner = task.inner_exclusive_access();
        task_inner.robust_list_head = 0;
        task_inner.sigsuspend_restore_mask = None;
        task_inner.sigaltstack = SigAltStack::disabled();
        let (trap_cx_ppn, user_stack_top) = {
            let task_res = task_inner
                .res
                .as_mut()
                .expect("exec task must keep TaskUserRes while rebuilding image");
            task_res.ustack_base = ustack_base;
            task_res.alloc_user_res();
            (task_res.trap_cx_ppn(), task_res.ustack_top())
        };
        task_inner.trap_cx_ppn = trap_cx_ppn;
        debug_assert_eq!(user_stack_top, expected_user_stack_top);
        write_user_stack(new_token, &stack_layout, &args, &envs)?;
        let user_sp = stack_layout.user_sp;

        let trap_cx = TrapContext::app_init_context(
            entry_point,
            user_sp,
            KERNEL_SPACE.exclusive_access().token(),
            task.kstack.get_top(),
            trap_handler as usize,
        );
        *task_inner.get_trap_cx() = trap_cx;
        drop(task_inner);
        self.release_vfork_parent();
        Ok(())
    }
}
