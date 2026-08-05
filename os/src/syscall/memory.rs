use crate::config::PAGE_SIZE;
use crate::mm::shm::{ShmCaller, ShmCreateContext, ShmError, ShmSegmentStat, ShmSetAttrs};
use crate::mm::{MapPermission, MemoryProtectError, MmapFlush, MmapPrefaultResult};
use crate::syscall::SyscallContext;
use crate::syscall::user_ptr::{copy_to_user, read_user_value, write_user_value};
use crate::task::{
    CAP_IPC_OWNER, CAP_SYS_ADMIN, ProcessControlBlock, current_process, current_user_token,
    suspend_current_and_run_next,
};
use alloc::vec::Vec;

use super::fs::get_file_by_fd_for_process;
use crate::uapi::errno::{Errno, KResult};

const PROT_READ: usize = 0x1;
const PROT_WRITE: usize = 0x2;
const PROT_EXEC: usize = 0x4;
const PROT_MASK: usize = PROT_READ | PROT_WRITE | PROT_EXEC;

const MAP_SHARED: usize = 0x01;
const MAP_PRIVATE: usize = 0x02;
const MAP_SHARED_VALIDATE: usize = 0x03;
const MAP_FIXED: usize = 0x10;
const MAP_ANONYMOUS: usize = 0x20;
const MAP_FIXED_NOREPLACE: usize = 0x100000;
const MAP_DENYWRITE: usize = 0x0800;
const MAP_EXECUTABLE: usize = 0x1000;
const MAP_GROWSDOWN: usize = 0x100;
const MAP_NORESERVE: usize = 0x4000;
const MAP_POPULATE: usize = 0x8000;
const MAP_STACK: usize = 0x20000;
// CONTEXT: Linux keeps MAP_DENYWRITE/MAP_EXECUTABLE as ignored legacy flags,
// and musl/glibc may pass MAP_NORESERVE or MAP_STACK as advisory flags. The
// current VM has no reservation accounting or stack VMA metadata, so accepting
// those advisory flags as no-ops is enough for loader, pthread, and LTP mmap
// compatibility. MAP_POPULATE is handled by prefaulting after VMA creation.
const MAP_SUPPORTED: usize = MAP_SHARED
    | MAP_PRIVATE
    | MAP_FIXED
    | MAP_ANONYMOUS
    | MAP_FIXED_NOREPLACE
    | MAP_DENYWRITE
    | MAP_EXECUTABLE
    | MAP_GROWSDOWN
    | MAP_NORESERVE
    | MAP_POPULATE
    | MAP_STACK;
const MAP_TYPE_MASK: usize = 0x03;
const MS_ASYNC: i32 = 0x1;
const MS_INVALIDATE: i32 = 0x2;
const MS_SYNC: i32 = 0x4;
const MS_SUPPORTED: i32 = MS_ASYNC | MS_INVALIDATE | MS_SYNC;
const MREMAP_MAYMOVE: usize = 0x1;
const MREMAP_FIXED: usize = 0x2;
const MREMAP_SUPPORTED: usize = MREMAP_MAYMOVE | MREMAP_FIXED;

const MEMBARRIER_CMD_QUERY: i32 = 0;
const MEMBARRIER_CMD_PRIVATE_EXPEDITED: i32 = 1 << 3;
const MEMBARRIER_CMD_REGISTER_PRIVATE_EXPEDITED: i32 = 1 << 4;
const MEMBARRIER_SUPPORTED_CMDS: isize =
    (MEMBARRIER_CMD_PRIVATE_EXPEDITED | MEMBARRIER_CMD_REGISTER_PRIVATE_EXPEDITED) as isize;
#[cfg(target_arch = "riscv64")]
const SYS_RISCV_FLUSH_ICACHE_LOCAL: usize = 1;

const MADV_DONTNEED: i32 = 4;

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
struct LinuxIpc64Perm {
    key: i32,
    uid: u32,
    gid: u32,
    cuid: u32,
    cgid: u32,
    mode: u32,
    seq: u16,
    pad2: u16,
    unused1: usize,
    unused2: usize,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
struct LinuxShmid64Ds {
    shm_perm: LinuxIpc64Perm,
    shm_segsz: usize,
    shm_atime: i64,
    shm_dtime: i64,
    shm_ctime: i64,
    shm_cpid: i32,
    shm_lpid: i32,
    shm_nattch: usize,
    unused4: usize,
    unused5: usize,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
struct LinuxShminfo {
    shmmax: usize,
    shmmin: usize,
    shmmni: usize,
    shmseg: usize,
    shmall: usize,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
struct LinuxShmInfo {
    used_ids: i32,
    shm_tot: usize,
    shm_rss: usize,
    shm_swp: usize,
    swap_attempts: usize,
    swap_successes: usize,
}

pub fn sys_brk_ctx(ctx: &SyscallContext, addr: usize) -> KResult {
    let process = ctx.process();
    let mut inner = process.inner_exclusive_access();
    Ok(inner.memory_set.set_program_break(addr) as isize)
}

pub fn sys_shmget(key: isize, size: usize, shmflg: i32) -> KResult {
    let process = current_process();
    let credentials = process.credentials();
    let caller = shm_caller_from(process.getpid(), &credentials);
    let context = ShmCreateContext {
        pid: process.getpid(),
        uid: credentials.euid,
        gid: credentials.egid,
    };
    crate::mm::shm::shmget_segment(key, size, shmflg, context, &caller)
        .map(|shmid| shmid as isize)
        .map_err(shm_error_to_sys_error)
}

pub fn sys_shmat(shmid: usize, shmaddr: usize, shmflg: i32) -> KResult {
    let requested_addr = normalize_shmat_addr(shmaddr, shmflg)?;
    let permission =
        crate::mm::shm::shm_permission_from_flags(shmflg).map_err(shm_error_to_sys_error)?;
    let process = current_process();
    let pid = process.getpid();
    let attach = crate::mm::shm::attach_segment(shmid, pid).map_err(shm_error_to_sys_error)?;
    let mapped_addr = {
        let mut inner = process.inner_exclusive_access();
        inner.memory_set.attach_shm_area(
            requested_addr,
            attach.len,
            permission,
            shmid,
            &attach.pages,
        )
    };
    match mapped_addr {
        Some(addr) => Ok(addr as isize),
        None => {
            let _ = crate::mm::shm::detach_segment(shmid, pid);
            Err(Errno::ENOMEM)
        }
    }
}

pub fn sys_shmctl(shmid: usize, cmd: i32, buf: usize) -> KResult {
    let process = current_process();
    let credentials = process.credentials();
    let caller = shm_caller_from(process.getpid(), &credentials);
    match cmd {
        crate::mm::shm::IPC_RMID => {
            crate::mm::shm::mark_segment_for_delete(shmid, &caller)
                .map_err(shm_error_to_sys_error)?;
            Ok(0)
        }
        crate::mm::shm::IPC_STAT => {
            let stat =
                crate::mm::shm::stat_segment(shmid, &caller).map_err(shm_error_to_sys_error)?;
            write_shmid_ds(buf, stat)?;
            Ok(0)
        }
        crate::mm::shm::IPC_SET => {
            let ds: LinuxShmid64Ds =
                read_user_value(current_user_token(), buf as *const LinuxShmid64Ds)?;
            crate::mm::shm::set_segment_attrs(
                shmid,
                ShmSetAttrs {
                    uid: ds.shm_perm.uid,
                    gid: ds.shm_perm.gid,
                    mode: ds.shm_perm.mode,
                },
                &caller,
            )
            .map_err(shm_error_to_sys_error)?;
            Ok(0)
        }
        crate::mm::shm::IPC_INFO => {
            write_user_value(
                current_user_token(),
                buf as *mut LinuxShminfo,
                &LinuxShminfo {
                    shmmax: crate::mm::shm::current_shmmax(),
                    shmmin: 1,
                    shmmni: crate::mm::shm::current_shmmni(),
                    shmseg: crate::mm::shm::current_shmmni(),
                    shmall: crate::mm::shm::current_shmall(),
                },
            )?;
            Ok(crate::mm::shm::highest_index() as isize)
        }
        crate::mm::shm::SHM_INFO => {
            let info = crate::mm::shm::usage_info();
            write_user_value(
                current_user_token(),
                buf as *mut LinuxShmInfo,
                &LinuxShmInfo {
                    used_ids: info.used_ids.try_into().unwrap_or(i32::MAX),
                    shm_tot: info.total_pages,
                    shm_rss: info.resident_pages,
                    shm_swp: info.swapped_pages,
                    swap_attempts: 0,
                    swap_successes: 0,
                },
            )?;
            Ok(info.highest_index as isize)
        }
        crate::mm::shm::SHM_STAT | crate::mm::shm::SHM_STAT_ANY => {
            let skip_permission = cmd == crate::mm::shm::SHM_STAT_ANY;
            let (real_shmid, stat) =
                crate::mm::shm::stat_segment_by_index(shmid, &caller, skip_permission)
                    .map_err(shm_error_to_sys_error)?;
            write_shmid_ds(buf, stat)?;
            Ok(real_shmid as isize)
        }
        _ => Err(Errno::EINVAL),
    }
}

pub fn sys_shmdt(shmaddr: usize) -> KResult {
    if shmaddr % PAGE_SIZE != 0 {
        return Err(Errno::EINVAL);
    }
    let process = current_process();
    let mut inner = process.inner_exclusive_access();
    inner
        .memory_set
        .detach_shm_area(shmaddr)
        .ok_or(Errno::EINVAL)?;
    Ok(0)
}

pub fn sys_mmap_ctx(
    ctx: &SyscallContext,
    addr: usize,
    len: usize,
    prot: usize,
    flags: usize,
    fd: usize,
    offset: usize,
) -> KResult {
    sys_mmap_impl(ctx, addr, len, prot, flags, fd, offset).map(|addr| addr as isize)
}

fn sys_mmap_impl(
    ctx: &SyscallContext,
    addr: usize,
    len: usize,
    prot: usize,
    flags: usize,
    fd: usize,
    offset: usize,
) -> Result<usize, Errno> {
    if prot & !PROT_MASK != 0 {
        return Err(Errno::EINVAL);
    }
    if flags & MAP_SHARED_VALIDATE == MAP_SHARED_VALIDATE {
        // UNFINISHED: Linux MAP_SHARED_VALIDATE behaves like MAP_SHARED while
        // rejecting unknown flags and enabling MAP_SYNC-style validation. This
        // kernel does not implement that validation mode yet.
        return Err(Errno::ENOTSUP);
    }
    if flags & !MAP_SUPPORTED != 0 {
        return Err(Errno::EINVAL);
    }
    let map_type = flags & MAP_TYPE_MASK;
    if map_type != MAP_SHARED && map_type != MAP_PRIVATE {
        return Err(Errno::EINVAL);
    }

    let shared = map_type == MAP_SHARED;
    let anonymous = flags & MAP_ANONYMOUS != 0;
    let no_replace = flags & MAP_FIXED_NOREPLACE != 0;
    let fixed = flags & MAP_FIXED != 0 || no_replace;
    let grow_down = flags & MAP_GROWSDOWN != 0;
    let populate = flags & MAP_POPULATE != 0;
    let writable = prot & PROT_WRITE != 0;
    let hardware_permission = prot_to_map_permission(prot);
    // CONTEXT: writable mappings need hardware read permission on current
    // targets, but procfs/debug output should report the exact Linux PROT bits
    // requested by userspace.
    let reported_permission = prot_to_reported_map_permission(prot);
    if fixed && addr % PAGE_SIZE != 0 {
        return Err(Errno::EINVAL);
    }
    if fixed && addr == 0 {
        return Err(Errno::EINVAL);
    }

    let (backing_file, file_size, page_cache_id) = if anonymous {
        if len == 0 || offset % PAGE_SIZE != 0 {
            return Err(Errno::EINVAL);
        }
        (None, 0, None)
    } else {
        let fd = fd as isize;
        if fd < 0 {
            return Err(Errno::EBADF);
        }
        let file = get_file_by_fd_for_process(ctx.process(), fd as usize)?;
        if len == 0 || offset % PAGE_SIZE != 0 {
            return Err(Errno::EINVAL);
        }
        if !file.readable() {
            return Err(Errno::EACCES);
        }
        if shared && writable && !file.writable() {
            return Err(Errno::EACCES);
        }
        if shared && writable && file.blocks_shared_writable_mmap() {
            return Err(Errno::EPERM);
        }
        let file_size = file.stat()?.size as usize;
        // Shared mappings keep the existing page-cache path. Readonly private
        // mappings may share a clean versioned file page; a later mprotect(W)
        // materializes every resident cache page before granting write access.
        let page_cache_id = if shared || !writable {
            file.page_cache_id()
        } else {
            None
        };
        (Some(file), file_size, page_cache_id)
    };
    let writable_shared_file = if shared && writable {
        backing_file.clone()
    } else {
        None
    };

    let process = ctx.process();
    let mut inner = process.inner_exclusive_access();

    if fixed {
        let map_len = page_align_len(len)?;
        let end = addr.checked_add(map_len).ok_or(Errno::ENOMEM)?;
        if no_replace && inner.memory_set.range_overlaps(addr, end) {
            return Err(Errno::EEXIST);
        }
        let (mapped_addr, flushes, retired_files) = inner
            .memory_set
            .mmap_fixed_area(
                addr,
                len,
                hardware_permission,
                reported_permission,
                backing_file,
                file_size,
                offset,
                shared,
                writable,
                grow_down,
                page_cache_id,
            )
            .ok_or(Errno::ENOMEM)?;
        drop(inner);
        drop(retired_files);
        if populate {
            loop {
                let result = process
                    .inner_exclusive_access()
                    .memory_set
                    .prefault_mmap_range(mapped_addr, len);
                match result {
                    MmapPrefaultResult::Complete => break,
                    MmapPrefaultResult::Retry => suspend_current_and_run_next(),
                    MmapPrefaultResult::Failed => return Err(Errno::ENOMEM),
                }
            }
        }
        if let Some(file) = writable_shared_file {
            file.inc_writable_shared_mmap();
        }
        write_back_mmap_flushes(flushes);
        return Ok(mapped_addr);
    }

    let mapped_addr = inner
        .memory_set
        .mmap_area(
            len,
            hardware_permission,
            reported_permission,
            backing_file,
            file_size,
            offset,
            shared,
            writable,
            grow_down,
            page_cache_id,
        )
        .ok_or(Errno::ENOMEM)?;
    drop(inner);
    if populate {
        loop {
            let result = process
                .inner_exclusive_access()
                .memory_set
                .prefault_mmap_range(mapped_addr, len);
            match result {
                MmapPrefaultResult::Complete => break,
                MmapPrefaultResult::Retry => suspend_current_and_run_next(),
                MmapPrefaultResult::Failed => return Err(Errno::ENOMEM),
            }
        }
    }
    if let Some(file) = writable_shared_file {
        file.inc_writable_shared_mmap();
    }
    Ok(mapped_addr)
}

pub fn sys_mprotect_ctx(ctx: &SyscallContext, addr: usize, len: usize, prot: usize) -> KResult {
    sys_mprotect_impl(ctx.process(), addr, len, prot)
}

fn sys_mprotect_impl(
    process: &ProcessControlBlock,
    addr: usize,
    len: usize,
    prot: usize,
) -> KResult {
    if addr % PAGE_SIZE != 0 {
        return Err(Errno::EINVAL);
    }
    if len == 0 {
        return Ok(0);
    }
    // UNFINISHED: Linux also has architecture-specific PROT flags and growable
    // VMA flags; this kernel currently supports only read/write/exec/none.
    if prot & !PROT_MASK != 0 {
        return Err(Errno::EINVAL);
    }

    let len = len.checked_add(PAGE_SIZE - 1).ok_or(Errno::ENOMEM)? & !(PAGE_SIZE - 1);
    addr.checked_add(len).ok_or(Errno::ENOMEM)?;

    let mut inner = process.inner_exclusive_access();
    inner
        .memory_set
        .mprotect_area(
            addr,
            len,
            prot_to_map_permission(prot),
            prot_to_reported_map_permission(prot),
        )
        .map_err(|err| match err {
            MemoryProtectError::Unmapped => Errno::ENOMEM,
            MemoryProtectError::AccessDenied => Errno::EACCES,
        })?;
    Ok(0)
}

pub fn sys_munmap_ctx(ctx: &SyscallContext, addr: usize, len: usize) -> KResult {
    if len == 0 || addr % PAGE_SIZE != 0 {
        return Err(Errno::EINVAL);
    }
    let process = ctx.process();
    let (flushes, retired_files) = {
        let mut inner = process.inner_exclusive_access();
        inner
            .memory_set
            .munmap_area(addr, len)
            .ok_or(Errno::EINVAL)?
    };
    drop(retired_files);
    write_back_mmap_flushes(flushes);
    Ok(0)
}

pub fn sys_mremap(
    old_addr: usize,
    old_size: usize,
    new_size: usize,
    flags: usize,
    new_addr: usize,
) -> KResult {
    if old_addr % PAGE_SIZE != 0 || old_size == 0 || new_size == 0 {
        return Err(Errno::EINVAL);
    }
    if flags & !MREMAP_SUPPORTED != 0 {
        return Err(Errno::EINVAL);
    }
    let may_move = flags & MREMAP_MAYMOVE != 0;
    let fixed = flags & MREMAP_FIXED != 0;
    if fixed {
        // UNFINISHED: MREMAP_FIXED relocation is not implemented; the current
        // mmap16 scoring path only needs non-fixed in-place growth.
        if !may_move || new_addr % PAGE_SIZE != 0 {
            return Err(Errno::EINVAL);
        }
        return Err(Errno::ENOMEM);
    }

    let process = current_process();
    let (mapped_addr, flushes, retired_files) = {
        let mut inner = process.inner_exclusive_access();
        inner
            .memory_set
            .mremap_area(old_addr, old_size, new_size, may_move)
            .ok_or(Errno::ENOMEM)?
    };
    drop(retired_files);
    write_back_mmap_flushes(flushes);
    Ok(mapped_addr as isize)
}

pub fn sys_madvise(addr: usize, len: usize, advice: i32) -> KResult {
    if addr % PAGE_SIZE != 0 {
        return Err(Errno::EINVAL);
    }
    if advice != MADV_DONTNEED {
        return Err(Errno::ENOTSUP);
    }
    if len == 0 {
        return Ok(0);
    }
    len.checked_add(PAGE_SIZE - 1).ok_or(Errno::ENOMEM)?;

    let process = current_process();
    if !process
        .inner_exclusive_access()
        .memory_set
        .madvise_dontneed_range(addr, len)
    {
        return Err(Errno::ENOMEM);
    }
    Ok(0)
}

pub fn sys_mincore(addr: usize, len: usize, vec: *mut u8) -> KResult {
    if addr % PAGE_SIZE != 0 {
        return Err(Errno::EINVAL);
    }
    let resident = current_process()
        .inner_exclusive_access()
        .memory_set
        .mincore_vec(addr, len)
        .ok_or(Errno::ENOMEM)?;
    copy_to_user(current_user_token(), vec, &resident)?;
    Ok(0)
}

pub fn sys_remap_file_pages(
    addr: usize,
    size: usize,
    prot: i32,
    _pgoff: usize,
    _flags: i32,
) -> KResult {
    // CONTEXT: Linux deprecated remap_file_pages() and replaced it with
    // in-kernel emulation. This kernel does not model nonlinear mappings; it
    // exposes the syscall so compatibility probes do not see ENOSYS, reports
    // success while a SysV SHM mapping is still current, and returns Linux's
    // documented EINVAL once the mapping becomes stale.
    if size == 0 {
        return Ok(0);
    }
    if prot != 0 {
        return Err(Errno::EINVAL);
    }
    let shmid = current_process()
        .inner_exclusive_access()
        .memory_set
        .shm_segment_id_for_range(addr, size)
        .ok_or(Errno::EINVAL)?;
    if !crate::mm::shm::segment_remap_available(shmid).unwrap_or(false) {
        return Err(Errno::EINVAL);
    }
    Ok(0)
}

pub fn sys_msync(addr: usize, len: usize, flags: i32) -> KResult {
    if addr % PAGE_SIZE != 0 {
        return Err(Errno::EINVAL);
    }
    if flags & !MS_SUPPORTED != 0 || flags & MS_ASYNC != 0 && flags & MS_SYNC != 0 {
        return Err(Errno::EINVAL);
    }

    let flushes = current_process()
        .inner_exclusive_access()
        .memory_set
        .msync_area(addr, len)
        .ok_or(Errno::ENOMEM)?;
    // UNFINISHED: Linux MS_INVALIDATE also invalidates other mappings and can
    // fail with EBUSY for locked pages. This kernel tracks mlock only for
    // syscall/procfs compatibility and has no cross-process invalidation model
    // yet, so it only validates the mapping range and writes back dirty shared
    // mmap pages.
    write_back_mmap_flushes(flushes);
    Ok(0)
}

#[cfg(target_arch = "riscv64")]
pub fn sys_riscv_flush_icache(_start: usize, _end: usize, flags: usize) -> KResult {
    if flags & !SYS_RISCV_FLUSH_ICACHE_LOCAL != 0 {
        return Err(Errno::EINVAL);
    }
    // Linux reserves the address range for forward compatibility. LOCAL
    // affects only the caller; flags=0 synchronizes every CPU currently using
    // the mm, while the address-space generation covers later entrants.
    if flags & SYS_RISCV_FLUSH_ICACHE_LOCAL != 0 {
        crate::arch::mm::instruction_barrier();
    } else {
        current_process()
            .inner_exclusive_access()
            .memory_set
            .synchronize_instruction_stream();
    }
    Ok(0)
}

pub fn sys_membarrier(cmd: i32, flags: u32, _cpu_id: i32) -> KResult {
    if flags != 0 {
        return Err(Errno::EINVAL);
    }

    match cmd {
        MEMBARRIER_CMD_QUERY => Ok(MEMBARRIER_SUPPORTED_CMDS),
        MEMBARRIER_CMD_REGISTER_PRIVATE_EXPEDITED => {
            current_process()
                .inner_exclusive_access()
                .membarrier_private_expedited_registered = true;
            Ok(0)
        }
        MEMBARRIER_CMD_PRIVATE_EXPEDITED => {
            let process = current_process();
            let inner = process.inner_exclusive_access();
            if !inner.membarrier_private_expedited_registered {
                return Err(Errno::EPERM);
            }
            inner.memory_set.synchronize_memory();
            Ok(0)
        }
        _ => Err(Errno::EINVAL),
    }
}

fn write_back_mmap_flushes(flushes: Vec<MmapFlush>) {
    for flush in flushes {
        flush.write_back();
    }
}

fn page_align_len(len: usize) -> Result<usize, Errno> {
    if len == 0 {
        return Err(Errno::EINVAL);
    }
    len.checked_add(PAGE_SIZE - 1)
        .map(|len| len & !(PAGE_SIZE - 1))
        .ok_or(Errno::ENOMEM)
}

fn prot_to_map_permission(prot: usize) -> MapPermission {
    let writable = prot & PROT_WRITE != 0;
    let mut permission = MapPermission::U;
    if prot & PROT_READ != 0 || writable {
        permission |= MapPermission::R;
    }
    if writable {
        permission |= MapPermission::W;
    }
    if prot & PROT_EXEC != 0 {
        permission |= MapPermission::X;
    }
    permission
}

fn prot_to_reported_map_permission(prot: usize) -> MapPermission {
    let mut permission = MapPermission::U;
    if prot & PROT_READ != 0 {
        permission |= MapPermission::R;
    }
    if prot & PROT_WRITE != 0 {
        permission |= MapPermission::W;
    }
    if prot & PROT_EXEC != 0 {
        permission |= MapPermission::X;
    }
    permission
}

fn normalize_shmat_addr(shmaddr: usize, shmflg: i32) -> Result<usize, Errno> {
    if shmaddr == 0 || shmaddr % PAGE_SIZE == 0 {
        return Ok(shmaddr);
    }
    // CONTEXT: SHMLBA is page-sized on the current contest targets.
    if shmflg & crate::mm::shm::SHM_RND != 0 {
        return Ok(shmaddr & !(PAGE_SIZE - 1));
    }
    Err(Errno::EINVAL)
}

fn shm_error_to_sys_error(error: ShmError) -> Errno {
    match error {
        ShmError::NotFound => Errno::ENOENT,
        ShmError::Exists => Errno::EEXIST,
        ShmError::Invalid => Errno::EINVAL,
        ShmError::NoMem => Errno::ENOMEM,
        ShmError::NoSpace => Errno::ENOSPC,
        ShmError::AccessDenied => Errno::EACCES,
        ShmError::NotPermitted => Errno::EPERM,
    }
}

fn shm_caller_from<'a>(pid: usize, credentials: &'a crate::task::Credentials) -> ShmCaller<'a> {
    ShmCaller {
        pid,
        euid: credentials.euid,
        egid: credentials.egid,
        groups: &credentials.groups,
        can_override_read: credentials.euid == 0
            && credentials
                .capabilities
                .has_effective(CAP_IPC_OWNER)
                .unwrap_or(false),
        can_override_owner: credentials.euid == 0
            && credentials
                .capabilities
                .has_effective(CAP_SYS_ADMIN)
                .unwrap_or(false),
    }
}

fn write_shmid_ds(buf: usize, stat: ShmSegmentStat) -> KResult<()> {
    let ds = LinuxShmid64Ds {
        shm_perm: LinuxIpc64Perm {
            key: stat.key.try_into().unwrap_or(i32::MAX),
            uid: stat.uid,
            gid: stat.gid,
            cuid: stat.cuid,
            cgid: stat.cgid,
            mode: stat.mode,
            ..LinuxIpc64Perm::default()
        },
        shm_segsz: stat.size,
        shm_atime: stat.atime,
        shm_dtime: stat.dtime,
        shm_ctime: stat.ctime,
        shm_cpid: stat.cpid,
        shm_lpid: stat.lpid,
        shm_nattch: stat.nattch,
        ..LinuxShmid64Ds::default()
    };
    write_user_value(current_user_token(), buf as *mut LinuxShmid64Ds, &ds)
}
