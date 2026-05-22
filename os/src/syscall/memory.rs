use crate::config::PAGE_SIZE;
use crate::mm::shm::{ShmCaller, ShmCreateContext, ShmError, ShmSegmentStat, ShmSetAttrs};
use crate::mm::{MapPermission, MemoryProtectError, MmapFlush};
use crate::syscall::user_ptr::{copy_to_user, read_user_value, write_user_value};
use crate::task::{
    CAP_IPC_LOCK, CAP_IPC_OWNER, CAP_SYS_ADMIN, PROCESS_PKEY_COUNT, RLimitResource,
    current_process, current_user_token,
};
use alloc::vec::Vec;
use core::sync::atomic::{Ordering, fence};

use super::errno::{SysError, SysResult};
use super::fs::get_file_by_fd;

const PROT_READ: usize = 0x1;
const PROT_WRITE: usize = 0x2;
const PROT_EXEC: usize = 0x4;
const PROT_MASK: usize = PROT_READ | PROT_WRITE | PROT_EXEC;

const MAP_SHARED: usize = 0x01;
const MAP_PRIVATE: usize = 0x02;
const MAP_SHARED_VALIDATE: usize = 0x03;
const MAP_FIXED: usize = 0x10;
const MAP_ANONYMOUS: usize = 0x20;
const MAP_DENYWRITE: usize = 0x0800;
const MAP_EXECUTABLE: usize = 0x1000;
const MAP_GROWSDOWN: usize = 0x100;
const MAP_NORESERVE: usize = 0x4000;
const MAP_POPULATE: usize = 0x8000;
const MAP_STACK: usize = 0x20000;
const MAP_LOCKED: usize = 0x2000;
// CONTEXT: Linux keeps MAP_DENYWRITE/MAP_EXECUTABLE as ignored legacy flags,
// and musl/glibc may pass MAP_NORESERVE or MAP_STACK as advisory flags. The
// current VM has no reservation accounting, eager MAP_POPULATE prefaulting, or
// stack VMA metadata, so accepting them as no-ops is enough for loader, pthread,
// and LTP mmap compatibility.
const MAP_SUPPORTED: usize = MAP_SHARED
    | MAP_PRIVATE
    | MAP_FIXED
    | MAP_ANONYMOUS
    | MAP_DENYWRITE
    | MAP_EXECUTABLE
    | MAP_GROWSDOWN
    | MAP_NORESERVE
    | MAP_POPULATE
    | MAP_STACK
    | MAP_LOCKED;
const MAP_TYPE_MASK: usize = 0x03;
const MS_ASYNC: i32 = 0x1;
const MS_INVALIDATE: i32 = 0x2;
const MS_SYNC: i32 = 0x4;
const MS_SUPPORTED: i32 = MS_ASYNC | MS_INVALIDATE | MS_SYNC;

const MLOCK_ONFAULT: usize = 0x1;
const MCL_CURRENT: usize = 0x1;
const MCL_FUTURE: usize = 0x2;
const MCL_ONFAULT: usize = 0x4;
const MCL_SUPPORTED: usize = MCL_CURRENT | MCL_FUTURE | MCL_ONFAULT;
const PKEY_DISABLE_ACCESS: usize = 0x1;
const PKEY_DISABLE_WRITE: usize = 0x2;
const PKEY_ACCESS_RIGHTS_MASK: usize = PKEY_DISABLE_ACCESS | PKEY_DISABLE_WRITE;

const MEMBARRIER_CMD_QUERY: i32 = 0;
const MEMBARRIER_CMD_PRIVATE_EXPEDITED: i32 = 1 << 3;
const MEMBARRIER_CMD_REGISTER_PRIVATE_EXPEDITED: i32 = 1 << 4;
const MEMBARRIER_SUPPORTED_CMDS: isize =
    (MEMBARRIER_CMD_PRIVATE_EXPEDITED | MEMBARRIER_CMD_REGISTER_PRIVATE_EXPEDITED) as isize;

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

pub fn sys_brk(addr: usize) -> SysResult {
    let process = current_process();
    let mut inner = process.inner_exclusive_access();
    Ok(inner.memory_set.set_program_break(addr) as isize)
}

pub fn sys_shmget(key: isize, size: usize, shmflg: i32) -> SysResult {
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

pub fn sys_shmat(shmid: usize, shmaddr: usize, shmflg: i32) -> SysResult {
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
            Err(SysError::ENOMEM)
        }
    }
}

pub fn sys_shmctl(shmid: usize, cmd: i32, buf: usize) -> SysResult {
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
                    shmmax: crate::mm::shm::SHM_MAX,
                    shmmin: 1,
                    shmmni: crate::mm::shm::SHMMNI,
                    shmseg: crate::mm::shm::SHMMNI,
                    shmall: crate::mm::shm::SHMALL,
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
        crate::mm::shm::SHM_LOCK => {
            crate::mm::shm::set_segment_locked(shmid, true, &caller)
                .map_err(shm_error_to_sys_error)?;
            Ok(0)
        }
        crate::mm::shm::SHM_UNLOCK => {
            crate::mm::shm::set_segment_locked(shmid, false, &caller)
                .map_err(shm_error_to_sys_error)?;
            Ok(0)
        }
        _ => Err(SysError::EINVAL),
    }
}

pub fn sys_shmdt(shmaddr: usize) -> SysResult {
    if shmaddr % PAGE_SIZE != 0 {
        return Err(SysError::EINVAL);
    }
    let process = current_process();
    let mut inner = process.inner_exclusive_access();
    inner
        .memory_set
        .detach_shm_area(shmaddr)
        .ok_or(SysError::EINVAL)?;
    Ok(0)
}

pub fn sys_mmap(
    addr: usize,
    len: usize,
    prot: usize,
    flags: usize,
    fd: usize,
    offset: usize,
) -> SysResult {
    sys_mmap_impl(addr, len, prot, flags, fd, offset).map(|addr| addr as isize)
}

fn sys_mmap_impl(
    addr: usize,
    len: usize,
    prot: usize,
    flags: usize,
    fd: usize,
    offset: usize,
) -> Result<usize, SysError> {
    if prot & !PROT_MASK != 0 {
        return Err(SysError::EINVAL);
    }
    if flags & MAP_SHARED_VALIDATE == MAP_SHARED_VALIDATE {
        return Err(SysError::ENOTSUP);
    }
    if flags & !MAP_SUPPORTED != 0 {
        return Err(SysError::EINVAL);
    }
    let map_type = flags & MAP_TYPE_MASK;
    if map_type != MAP_SHARED && map_type != MAP_PRIVATE {
        return Err(SysError::EINVAL);
    }

    let shared = map_type == MAP_SHARED;
    let anonymous = flags & MAP_ANONYMOUS != 0;
    let fixed = flags & MAP_FIXED != 0;
    let grow_down = flags & MAP_GROWSDOWN != 0;
    let writable = prot & PROT_WRITE != 0;
    let hardware_permission = prot_to_map_permission(prot);
    // CONTEXT: writable mappings need hardware read permission on current
    // targets, but procfs/debug output should report the exact Linux PROT bits
    // requested by userspace.
    let reported_permission = prot_to_reported_map_permission(prot);
    if fixed && addr % PAGE_SIZE != 0 {
        return Err(SysError::EINVAL);
    }
    if fixed && addr == 0 {
        return Err(SysError::EINVAL);
    }

    let (backing_file, file_size, page_cache_id) = if anonymous {
        if len == 0 || offset % PAGE_SIZE != 0 {
            return Err(SysError::EINVAL);
        }
        (None, 0, None)
    } else {
        let fd = fd as isize;
        if fd < 0 {
            return Err(SysError::EBADF);
        }
        let file = get_file_by_fd(fd as usize)?;
        if len == 0 || offset % PAGE_SIZE != 0 {
            return Err(SysError::EINVAL);
        }
        if !file.readable() {
            return Err(SysError::EACCES);
        }
        if shared && writable && !file.writable() {
            return Err(SysError::EACCES);
        }
        if shared && writable && file.blocks_shared_writable_mmap() {
            return Err(SysError::EPERM);
        }
        let file_size = file.stat()?.size as usize;
        let page_cache_id = if shared { file.page_cache_id() } else { None };
        (Some(file), file_size, page_cache_id)
    };
    let writable_shared_file = if shared && writable {
        backing_file.clone()
    } else {
        None
    };

    let process = current_process();
    let mut inner = process.inner_exclusive_access();

    if fixed {
        let (mapped_addr, flushes) = inner
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
            .ok_or(SysError::ENOMEM)?;
        drop(inner);
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
        .ok_or(SysError::ENOMEM)?;
    drop(inner);
    if let Some(file) = writable_shared_file {
        file.inc_writable_shared_mmap();
    }
    Ok(mapped_addr)
}

pub fn sys_mprotect(addr: usize, len: usize, prot: usize) -> SysResult {
    sys_mprotect_impl(addr, len, prot, None)
}

pub fn sys_pkey_mprotect(addr: usize, len: usize, prot: usize, pkey: isize) -> SysResult {
    let pkey = match pkey {
        -1 => None,
        0 => Some(0),
        value if value > 0 && (value as usize) < PROCESS_PKEY_COUNT => Some(value as usize),
        _ => return Err(SysError::EINVAL),
    };
    sys_mprotect_impl(addr, len, prot, pkey)
}

pub fn sys_pkey_alloc(flags: usize, access_rights: usize) -> SysResult {
    if flags != 0 || access_rights & !PKEY_ACCESS_RIGHTS_MASK != 0 {
        return Err(SysError::EINVAL);
    }

    let process = current_process();
    let mut inner = process.inner_exclusive_access();
    let Some(pkey) = inner
        .pkey_rights
        .iter()
        .enumerate()
        .skip(1)
        .find_map(|(pkey, rights)| rights.is_none().then_some(pkey))
    else {
        return Err(SysError::ENOSPC);
    };
    inner.pkey_rights[pkey] = Some(access_rights);
    Ok(pkey as isize)
}

pub fn sys_pkey_free(pkey: isize) -> SysResult {
    if pkey <= 0 || (pkey as usize) >= PROCESS_PKEY_COUNT {
        return Err(SysError::EINVAL);
    }

    let process = current_process();
    let mut inner = process.inner_exclusive_access();
    let pkey = pkey as usize;
    if inner.pkey_rights[pkey].is_none() {
        return Err(SysError::EINVAL);
    }
    inner.pkey_rights[pkey] = None;
    Ok(0)
}

fn sys_mprotect_impl(addr: usize, len: usize, prot: usize, pkey: Option<usize>) -> SysResult {
    if addr % PAGE_SIZE != 0 {
        return Err(SysError::EINVAL);
    }
    if len == 0 {
        return Ok(0);
    }
    // UNFINISHED: Linux also has architecture-specific PROT flags and growable
    // VMA flags; this kernel currently supports only read/write/exec/none.
    if prot & !PROT_MASK != 0 {
        return Err(SysError::EINVAL);
    }

    let len = len.checked_add(PAGE_SIZE - 1).ok_or(SysError::ENOMEM)? & !(PAGE_SIZE - 1);
    addr.checked_add(len).ok_or(SysError::ENOMEM)?;

    let process = current_process();
    let mut inner = process.inner_exclusive_access();
    let access_rights = match pkey {
        Some(0) | None => 0,
        Some(pkey) => inner.pkey_rights[pkey].ok_or(SysError::EINVAL)?,
    };
    let effective_prot = prot_with_pkey_access_rights(prot, access_rights);
    inner
        .memory_set
        .mprotect_area(
            addr,
            len,
            prot_to_map_permission(effective_prot),
            prot_to_reported_map_permission(prot),
        )
        .map_err(|err| match err {
            MemoryProtectError::Unmapped => SysError::ENOMEM,
            MemoryProtectError::AccessDenied => SysError::EACCES,
        })?;
    Ok(0)
}

pub fn sys_munmap(addr: usize, len: usize) -> SysResult {
    if len == 0 || addr % PAGE_SIZE != 0 {
        return Err(SysError::EINVAL);
    }
    let process = current_process();
    let flushes = {
        let mut inner = process.inner_exclusive_access();
        inner
            .memory_set
            .munmap_area(addr, len)
            .ok_or(SysError::EINVAL)?
    };
    write_back_mmap_flushes(flushes);
    Ok(0)
}

// UNFINISHED: The kernel still has no swap or page-reclaim path, so these
// mlock syscalls provide Linux-compatible validation, prefaulting, RLIMIT
// checks, and procfs accounting without a real unevictable-page mechanism.
pub fn sys_mlock(addr: usize, len: usize) -> SysResult {
    let additional = {
        let process = current_process();
        let inner = process.inner_exclusive_access();
        inner
            .memory_set
            .additional_locked_bytes_for_range(addr, len)
            .ok_or(SysError::ENOMEM)?
    };
    check_memlock_limit(additional)?;

    let process = current_process();
    let mut inner = process.inner_exclusive_access();
    if !inner.memory_set.mlock_range(addr, len, false) {
        return Err(SysError::ENOMEM);
    }
    Ok(0)
}

pub fn sys_mlock2(addr: usize, len: usize, flags: usize) -> SysResult {
    if flags & !MLOCK_ONFAULT != 0 {
        return Err(SysError::EINVAL);
    }
    let on_fault = flags & MLOCK_ONFAULT != 0;
    let additional = {
        let process = current_process();
        let inner = process.inner_exclusive_access();
        inner
            .memory_set
            .additional_locked_bytes_for_range(addr, len)
            .ok_or(SysError::ENOMEM)?
    };
    check_memlock_limit(additional)?;

    let process = current_process();
    let mut inner = process.inner_exclusive_access();
    if !inner.memory_set.mlock_range(addr, len, on_fault) {
        return Err(SysError::ENOMEM);
    }
    Ok(0)
}

pub fn sys_munlock(addr: usize, len: usize) -> SysResult {
    let process = current_process();
    let mut inner = process.inner_exclusive_access();
    if !inner.memory_set.munlock_range(addr, len) {
        return Err(SysError::ENOMEM);
    }
    Ok(0)
}

pub fn sys_mlockall(flags: usize) -> SysResult {
    if flags & !MCL_SUPPORTED != 0 || flags & (MCL_CURRENT | MCL_FUTURE) == 0 {
        return Err(SysError::EINVAL);
    }
    let on_fault = flags & MCL_ONFAULT != 0;
    let lock_current = flags & MCL_CURRENT != 0;
    let lock_future = flags & MCL_FUTURE != 0;
    let additional = if lock_current {
        current_process()
            .inner_exclusive_access()
            .memory_set
            .additional_locked_bytes_for_current()
    } else {
        0
    };
    check_memlock_limit(additional)?;

    let process = current_process();
    let mut inner = process.inner_exclusive_access();
    if lock_current && !inner.memory_set.mlock_current(on_fault) {
        return Err(SysError::ENOMEM);
    }
    if lock_future {
        inner.memory_set.set_mlock_future(on_fault);
    }
    Ok(0)
}

pub fn sys_munlockall() -> SysResult {
    current_process()
        .inner_exclusive_access()
        .memory_set
        .munlock_all();
    Ok(0)
}

pub fn sys_mincore(addr: usize, len: usize, vec: *mut u8) -> SysResult {
    if addr % PAGE_SIZE != 0 {
        return Err(SysError::EINVAL);
    }
    let resident = current_process()
        .inner_exclusive_access()
        .memory_set
        .mincore_vec(addr, len)
        .ok_or(SysError::ENOMEM)?;
    copy_to_user(current_user_token(), vec, &resident)?;
    Ok(0)
}

pub fn sys_msync(addr: usize, len: usize, flags: i32) -> SysResult {
    if addr % PAGE_SIZE != 0 {
        return Err(SysError::EINVAL);
    }
    if flags & !MS_SUPPORTED != 0 || flags & MS_ASYNC != 0 && flags & MS_SYNC != 0 {
        return Err(SysError::EINVAL);
    }

    let flushes = current_process()
        .inner_exclusive_access()
        .memory_set
        .msync_area(addr, len)
        .ok_or(SysError::ENOMEM)?;
    // UNFINISHED: Linux MS_INVALIDATE also invalidates other mappings and can
    // fail with EBUSY for locked pages. This kernel tracks mlock only for
    // syscall/procfs compatibility and has no cross-process invalidation model
    // yet, so it only validates the mapping range and writes back dirty shared
    // mmap pages.
    write_back_mmap_flushes(flushes);
    Ok(0)
}

pub fn sys_membarrier(cmd: i32, flags: u32, _cpu_id: i32) -> SysResult {
    if flags != 0 {
        return Err(SysError::EINVAL);
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
            if !current_process()
                .inner_exclusive_access()
                .membarrier_private_expedited_registered
            {
                return Err(SysError::EPERM);
            }
            // UNFINISHED: A real SMP kernel must force every running sibling
            // thread through a matching memory-ordering state. The contest
            // kernel currently runs one hart, so a full local fence is enough
            // for the libc private-expedited compatibility path.
            fence(Ordering::SeqCst);
            Ok(0)
        }
        _ => Err(SysError::EINVAL),
    }
}

fn check_memlock_limit(additional_locked_bytes: usize) -> SysResult<()> {
    let process = current_process();
    let inner = process.inner_exclusive_access();
    let credentials = &inner.credentials;
    let privileged = credentials.euid == 0
        && credentials
            .capabilities
            .has_effective(CAP_IPC_LOCK)
            .unwrap_or(false);
    if privileged {
        return Ok(());
    }

    let limit = inner.resource_limits.get(RLimitResource::MemLock).rlim_cur;
    if limit == 0 {
        return Err(SysError::EPERM);
    }
    let locked = inner.memory_set.locked_bytes();
    if locked
        .checked_add(additional_locked_bytes)
        .is_none_or(|total| total > limit)
    {
        return Err(SysError::ENOMEM);
    }
    Ok(())
}

fn write_back_mmap_flushes(flushes: Vec<MmapFlush>) {
    for flush in flushes {
        flush.write_back();
    }
}

fn prot_with_pkey_access_rights(prot: usize, access_rights: usize) -> usize {
    // UNFINISHED: This is a contest compatibility model for pkeys. It rewrites
    // ordinary PTE permissions instead of storing hardware pkey tags and
    // per-thread PKRU rights, so it covers pkey_mprotect-style restriction and
    // restore flows but not cheap userspace PKRU flips or signal-time PKRU
    // behavior.
    if access_rights & PKEY_DISABLE_ACCESS != 0 {
        return prot & !(PROT_READ | PROT_WRITE);
    }
    if access_rights & PKEY_DISABLE_WRITE != 0 {
        return prot & !PROT_WRITE;
    }
    prot
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

fn normalize_shmat_addr(shmaddr: usize, shmflg: i32) -> Result<usize, SysError> {
    if shmaddr == 0 || shmaddr % PAGE_SIZE == 0 {
        return Ok(shmaddr);
    }
    // CONTEXT: SHMLBA is page-sized on the current contest targets.
    if shmflg & crate::mm::shm::SHM_RND != 0 {
        return Ok(shmaddr & !(PAGE_SIZE - 1));
    }
    Err(SysError::EINVAL)
}

fn shm_error_to_sys_error(error: ShmError) -> SysError {
    match error {
        ShmError::NotFound => SysError::ENOENT,
        ShmError::Exists => SysError::EEXIST,
        ShmError::Invalid => SysError::EINVAL,
        ShmError::NoMem => SysError::ENOMEM,
        ShmError::AccessDenied => SysError::EACCES,
        ShmError::NotPermitted => SysError::EPERM,
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
        can_lock_ipc: credentials.euid == 0
            && credentials
                .capabilities
                .has_effective(CAP_IPC_LOCK)
                .unwrap_or(false),
    }
}

fn write_shmid_ds(buf: usize, stat: ShmSegmentStat) -> SysResult<()> {
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
