use super::{
    SigAltStack, SignalAction, current_task, prepare_exec_thread_group,
    process::{ProcessControlBlock, comm_from_cmdline, empty_process_pkey_rights},
    ptrace_note_exec_current, refresh_current_user_token,
};
use crate::config::{PAGE_SIZE, USER_STACK_SIZE};
use crate::fs::{File, VfsNodeId, track_regular_file_executable, untrack_regular_file_executable};
use crate::mm::{ElfLoadInfo, KERNEL_SPACE, MemorySet};
use crate::perf;
use crate::syscall::close_detached_fd_entry_for_process_teardown;
use crate::syscall::errno::{SysError, SysResult};
use crate::syscall::user_ptr::copy_to_user;
use crate::trap::{TrapContext, trap_handler};
use alloc::string::String;
use alloc::sync::Arc;
use alloc::{vec, vec::Vec};

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

fn is_smp_sched_probe_path(path: &str) -> bool {
    matches!(path, "/x1/smp-sched-life-rv" | "/x1/smp-sched-life-la")
}

fn is_smp_cpu_probe_path(path: &str) -> bool {
    matches!(path, "/x1/smp-cpu-sentinel-rv" | "/x1/smp-cpu-sentinel-la")
}

fn is_smp_wait_io_probe_path(path: &str) -> bool {
    matches!(path, "/x1/smp-wait-io-rv" | "/x1/smp-wait-io-la")
}

fn is_smp_phase4_wait_probe_path(path: &str) -> bool {
    matches!(
        path,
        "/x1/smp-wait-timer-rv"
            | "/x1/smp-wait-timer-la"
            | "/x1/smp-wait-futex-rv"
            | "/x1/smp-wait-futex-la"
    )
}
const AT_CLKTCK: usize = 17;
const AT_SECURE: usize = 23;
const AT_RANDOM: usize = 25;
const AT_SYSINFO_EHDR: usize = 33;

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
    pub(super) sysinfo_ehdr: usize,
}

fn align_down(value: usize, align: usize) -> usize {
    value & !(align - 1)
}

struct ExecStackLayout {
    stack_top: usize,
    user_sp: usize,
    argv_base: usize,
    envp_base: usize,
    auxv_base: usize,
    random_addr: usize,
    arg_ptrs: Vec<usize>,
    env_ptrs: Vec<usize>,
    auxv: Vec<(usize, usize)>,
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

    let mut auxv = Vec::with_capacity(16);
    auxv.push((AT_PHDR, stack_info.phdr));
    auxv.push((AT_PHENT, stack_info.phent));
    auxv.push((AT_PHNUM, stack_info.phnum));
    auxv.push((AT_PAGESZ, PAGE_SIZE));
    auxv.push((AT_ENTRY, stack_info.at_entry));
    auxv.push((AT_BASE, stack_info.interp_base));
    auxv.push((AT_FLAGS, 0));
    auxv.push((AT_UID, stack_info.uid as usize));
    auxv.push((AT_EUID, stack_info.euid as usize));
    auxv.push((AT_GID, stack_info.gid as usize));
    auxv.push((AT_EGID, stack_info.egid as usize));
    auxv.push((AT_HWCAP, ELF_HWCAP));
    // Keep this in sync with TICKS_PER_SEC so libc converts times(2) clock
    // ticks using the same Linux USER_HZ value the kernel reports.
    auxv.push((AT_CLKTCK, 100));
    auxv.push((AT_SECURE, 0));
    auxv.push((AT_RANDOM, random_addr));
    if stack_info.sysinfo_ehdr != 0 {
        auxv.push((AT_SYSINFO_EHDR, stack_info.sysinfo_ehdr));
    }

    let word = core::mem::size_of::<usize>();
    let table_size = checked_table_size(arg_ptrs.len(), env_ptrs.len(), auxv.len())?;
    let table_top = checked_stack_align_down(string_sp, 16, stack_low)?;
    let table_bottom = checked_stack_sub(table_top, table_size, stack_low)?;
    let user_sp = checked_stack_align_down(table_bottom, 16, stack_low)?;
    let argv_base = user_sp + word;
    let envp_base = argv_base + (arg_ptrs.len() + 1) * word;
    let auxv_base = envp_base + (env_ptrs.len() + 1) * word;

    Ok(ExecStackLayout {
        stack_top,
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

fn stack_offset(layout: &ExecStackLayout, addr: usize, len: usize) -> SysResult<usize> {
    let offset = addr.checked_sub(layout.user_sp).ok_or(SysError::EFAULT)?;
    let end = offset.checked_add(len).ok_or(SysError::EFAULT)?;
    let stack_len = layout
        .stack_top
        .checked_sub(layout.user_sp)
        .ok_or(SysError::EFAULT)?;
    if end > stack_len {
        return Err(SysError::EFAULT);
    }
    Ok(offset)
}

fn write_stack_bytes(
    buffer: &mut [u8],
    layout: &ExecStackLayout,
    addr: usize,
    bytes: &[u8],
) -> SysResult<()> {
    let offset = stack_offset(layout, addr, bytes.len())?;
    buffer[offset..offset + bytes.len()].copy_from_slice(bytes);
    Ok(())
}

fn write_stack_usize(
    buffer: &mut [u8],
    layout: &ExecStackLayout,
    addr: usize,
    value: usize,
) -> SysResult<()> {
    write_stack_bytes(buffer, layout, addr, &value.to_ne_bytes())
}

fn write_stack_string(
    buffer: &mut [u8],
    layout: &ExecStackLayout,
    addr: usize,
    string: &str,
) -> SysResult<()> {
    write_stack_bytes(buffer, layout, addr, string.as_bytes())?;
    let nul_addr = addr.checked_add(string.len()).ok_or(SysError::EFAULT)?;
    write_stack_bytes(buffer, layout, nul_addr, &[0])
}

fn write_user_stack(
    token: usize,
    layout: &ExecStackLayout,
    args: &[String],
    envs: &[String],
) -> SysResult<()> {
    let stack_len = layout
        .stack_top
        .checked_sub(layout.user_sp)
        .ok_or(SysError::EFAULT)?;
    let mut stack = vec![0; stack_len];

    for (addr, string) in layout.env_ptrs.iter().zip(envs.iter()) {
        write_stack_string(stack.as_mut_slice(), layout, *addr, string.as_str())?;
    }
    for (addr, string) in layout.arg_ptrs.iter().zip(args.iter()) {
        write_stack_string(stack.as_mut_slice(), layout, *addr, string.as_str())?;
    }

    write_stack_bytes(stack.as_mut_slice(), layout, layout.random_addr, &[0u8; 16])?;

    let word = core::mem::size_of::<usize>();
    write_stack_usize(
        stack.as_mut_slice(),
        layout,
        layout.user_sp,
        layout.arg_ptrs.len(),
    )?;
    for (i, ptr) in layout.arg_ptrs.iter().enumerate() {
        write_stack_usize(
            stack.as_mut_slice(),
            layout,
            layout.argv_base + i * word,
            *ptr,
        )?;
    }
    write_stack_usize(
        stack.as_mut_slice(),
        layout,
        layout.argv_base + layout.arg_ptrs.len() * word,
        0,
    )?;

    for (i, ptr) in layout.env_ptrs.iter().enumerate() {
        write_stack_usize(
            stack.as_mut_slice(),
            layout,
            layout.envp_base + i * word,
            *ptr,
        )?;
    }
    write_stack_usize(
        stack.as_mut_slice(),
        layout,
        layout.envp_base + layout.env_ptrs.len() * word,
        0,
    )?;

    for (i, (key, value)) in layout.auxv.iter().enumerate() {
        let entry = layout.auxv_base + i * 2 * word;
        write_stack_usize(stack.as_mut_slice(), layout, entry, *key)?;
        write_stack_usize(stack.as_mut_slice(), layout, entry + word, *value)?;
    }
    let null_entry = layout.auxv_base + layout.auxv.len() * 2 * word;
    write_stack_usize(stack.as_mut_slice(), layout, null_entry, AT_NULL)?;
    write_stack_usize(stack.as_mut_slice(), layout, null_entry + word, 0)?;

    copy_to_user(token, layout.user_sp as *mut u8, stack.as_slice())?;
    perf::record_exec_stack_copy(stack.len());
    Ok(())
}

pub(super) fn init_user_stack(
    memory_set: &mut MemorySet,
    stack_top: usize,
    args: &[String],
    envs: &[String],
    stack_info: &ExecStackInfo,
) -> SysResult<(usize, usize, usize)> {
    let layout = plan_user_stack(stack_top, args, envs, stack_info)?;
    if !memory_set.materialize_framed_range(layout.user_sp, stack_top) {
        return Err(SysError::ENOMEM);
    }
    let token = memory_set.token();
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
    #[expect(
        clippy::too_many_arguments,
        reason = "exec receives already-resolved image resources and user vectors at the commit point"
    )]
    pub fn exec(
        self: &Arc<Self>,
        elf: &xmas_elf::ElfFile<'_>,
        executable_file: Arc<dyn File + Send + Sync>,
        executable_file_size: usize,
        interpreter: Option<(&xmas_elf::ElfFile<'_>, Arc<dyn File + Send + Sync>, usize)>,
        args: Vec<String>,
        envs: Vec<String>,
        executable_path: String,
        executable_node: Option<VfsNodeId>,
    ) -> SysResult<()> {
        let smp_sched_probe = is_smp_sched_probe_path(&executable_path);
        let smp_cpu_probe = is_smp_cpu_probe_path(&executable_path);
        let smp_wait_io_probe = is_smp_wait_io_probe_path(&executable_path);
        let smp_phase4_wait_probe = is_smp_phase4_wait_probe_path(&executable_path);
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
            sysinfo_ehdr,
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
            sysinfo_ehdr,
        };
        let expected_user_stack_top = ustack_base
            .checked_add(USER_STACK_SIZE)
            .ok_or(SysError::E2BIG)?;
        // Plan the user stack before committing the new memory set. Argument
        // overflow should fail with E2BIG while the old image is still intact.
        let stack_layout = plan_user_stack(expected_user_stack_top, &args, &envs, &stack_info)?;
        let new_token = memory_set.token();

        // From this point on, user stack writes must target `new_token`; the old
        // process token may already describe the pre-exec image.
        let (previous_executable_node, close_on_exec_entries) = {
            let mut inner = self.inner_exclusive_access();
            inner.memory_set = memory_set;
            inner.pkey_rights = empty_process_pkey_rights();
            let previous = core::mem::replace(&mut inner.executable_node, executable_node);
            inner.executable_path = executable_path;
            inner.cmdline = args.clone();
            inner.comm = comm_from_cmdline(&args);
            inner.timers.clear_posix_after_exec();
            for action in inner.signal_actions.iter_mut() {
                if action.has_user_handler() {
                    *action = SignalAction::default();
                }
            }
            let close_on_exec_entries = inner.close_on_exec_fd_entries();
            (previous, close_on_exec_entries)
        };
        // Drop close-on-exec files after releasing the PCB lock. File
        // destructors can enter VFS/mount cleanup paths, which must not run
        // while the exec image-commit state is still locked.
        for entry in close_on_exec_entries {
            close_detached_fd_entry_for_process_teardown(entry);
        }
        if let Some(node) = previous_executable_node {
            untrack_regular_file_executable(node);
        }
        if let Some(node) = executable_node {
            track_regular_file_executable(node);
        }

        let mut task_inner = task.inner_exclusive_access();
        // UNFINISHED: Linux also notifies robust-futex waiters when the owner
        // thread execve()s. This path currently clears the per-thread robust
        // list for the new image without walking the old list.
        task_inner.robust_list_head = 0;
        task_inner.sigsuspend_restore_mask = None;
        task_inner.sigaltstack = SigAltStack::disabled();
        task_inner.smp_sched_probe = smp_sched_probe;
        // Start counting only when the worker explicitly widens its affinity.
        // Lazy executable page-in can block before the first user instruction
        // and is outside the bounded scheduler lifecycle workload.
        task_inner.smp_sched_probe_active = false;
        task_inner.smp_cpu_probe = smp_cpu_probe;
        task_inner.smp_wait_io_probe = smp_wait_io_probe;
        task_inner.smp_phase4_wait_probe = smp_phase4_wait_probe;
        // Keep initial executable page-in on CPU 0. The probe's first
        // sched_setaffinity() call widens placement only after it has entered
        // its self-contained scheduler workload; shared VFS/I/O concurrency is
        // audited separately in Phase 4.
        task_inner.allowed_cpus = crate::cpu::CpuMask::single(0);
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
        self.inner_exclusive_access()
            .memory_set
            .materialize_framed_range(stack_layout.user_sp, user_stack_top)
            .then_some(())
            .ok_or(SysError::ENOMEM)?;
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
        refresh_current_user_token();
        self.release_vfork_parent();
        ptrace_note_exec_current();
        if smp_cpu_probe {
            crate::task::start_smp_cpu_probe();
        }
        if smp_wait_io_probe {
            crate::task::start_smp_wait_io_probe();
        }
        Ok(())
    }
}
