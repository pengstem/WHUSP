use crate::config::PAGE_SIZE;
use crate::mm::shm::ShmError;
use crate::mm::{MapPermission, MemoryProtectError};
use crate::task::current_process;
use core::sync::atomic::{Ordering, fence};

use super::errno::{SysError, SysResult};

const PROT_READ: usize = 0x1;
const PROT_WRITE: usize = 0x2;
const PROT_EXEC: usize = 0x4;
const PROT_MASK: usize = PROT_READ | PROT_WRITE | PROT_EXEC;

const MAP_SHARED: usize = 0x01;
const MAP_PRIVATE: usize = 0x02;
const MAP_FIXED: usize = 0x10;
const MAP_ANONYMOUS: usize = 0x20;
const MAP_DENYWRITE: usize = 0x0800;
const MAP_EXECUTABLE: usize = 0x1000;
const MAP_NORESERVE: usize = 0x4000;
const MAP_STACK: usize = 0x20000;
// CONTEXT: Linux keeps MAP_DENYWRITE/MAP_EXECUTABLE as ignored legacy flags,
// and musl/glibc may pass MAP_NORESERVE or MAP_STACK as advisory flags. The
// current VM has no reservation accounting or stack VMA metadata, so accepting
// them as no-ops is enough for loader and pthread compatibility.
const MAP_SUPPORTED: usize = MAP_SHARED
    | MAP_PRIVATE
    | MAP_FIXED
    | MAP_ANONYMOUS
    | MAP_DENYWRITE
    | MAP_EXECUTABLE
    | MAP_NORESERVE
    | MAP_STACK;
const MAP_TYPE_MASK: usize = 0x03;
const MS_ASYNC: i32 = 0x1;
const MS_INVALIDATE: i32 = 0x2;
const MS_SYNC: i32 = 0x4;
const MS_SUPPORTED: i32 = MS_ASYNC | MS_INVALIDATE | MS_SYNC;

const MEMBARRIER_CMD_QUERY: i32 = 0;
const MEMBARRIER_CMD_PRIVATE_EXPEDITED: i32 = 1 << 3;
const MEMBARRIER_CMD_REGISTER_PRIVATE_EXPEDITED: i32 = 1 << 4;
const MEMBARRIER_SUPPORTED_CMDS: isize =
    (MEMBARRIER_CMD_PRIVATE_EXPEDITED | MEMBARRIER_CMD_REGISTER_PRIVATE_EXPEDITED) as isize;

pub fn sys_brk(addr: usize) -> SysResult {
    let process = current_process();
    let mut inner = process.inner_exclusive_access();
    Ok(inner.memory_set.set_program_break(addr) as isize)
}

pub fn sys_shmget(key: isize, size: usize, shmflg: i32) -> SysResult {
    crate::mm::shm::shmget_segment(key, size, shmflg, current_process().getpid())
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

pub fn sys_shmctl(shmid: usize, cmd: i32, _buf: usize) -> SysResult {
    match cmd {
        crate::mm::shm::IPC_RMID => {
            crate::mm::shm::mark_segment_for_delete(shmid, current_process().getpid())
                .map_err(shm_error_to_sys_error)?;
            Ok(0)
        }
        // UNFINISHED: IPC_STAT, IPC_SET, IPC_INFO, SHM_STAT, SHM_INFO, and
        // SHM_LOCK/UNLOCK need Linux-compatible shmid_ds/ucred handling.
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

// TODO: prot ... i don't think this is a good name
fn sys_mmap_impl(
    addr: usize,
    len: usize,
    prot: usize,
    flags: usize,
    fd: usize,
    offset: usize,
) -> Result<usize, SysError> {
    if len == 0 || offset % PAGE_SIZE != 0 {
        return Err(SysError::EINVAL);
    }
    if prot & !PROT_MASK != 0 {
        return Err(SysError::EINVAL);
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
    let writable = prot & PROT_WRITE != 0;
    let permission = prot_to_map_permission(prot);
    let reported_permission = prot_to_reported_map_permission(prot);
    if fixed && addr % PAGE_SIZE != 0 {
        return Err(SysError::EINVAL);
    }
    if fixed && addr == 0 {
        return Err(SysError::EINVAL);
    }

    let process = current_process();
    let (backing_file, file_size, page_cache_id) = if anonymous {
        (None, 0, None)
    } else {
        let fd = fd as isize;
        if fd < 0 {
            return Err(SysError::EBADF);
        }
        let inner = process.inner_exclusive_access();
        let file = inner
            .fd_table
            .get(fd as usize)
            .and_then(|entry| entry.as_ref())
            .map(|entry| entry.file())
            .ok_or(SysError::EBADF)?;
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

    let mut inner = process.inner_exclusive_access();
    if fixed {
        let (mapped_addr, flushes) = inner
            .memory_set
            .mmap_fixed_area(
                addr,
                len,
                permission,
                reported_permission,
                backing_file,
                file_size,
                offset,
                shared,
                writable,
                page_cache_id,
            )
            .ok_or(SysError::ENOMEM)?;
        drop(inner);
        if let Some(file) = writable_shared_file {
            file.inc_writable_shared_mmap();
        }
        for flush in flushes {
            flush.write_back();
        }
        return Ok(mapped_addr);
    }

    // TODO: why dose map permission do not contain shared and writable
    let mapped_addr = inner
        .memory_set
        .mmap_area(
            len,
            permission,
            reported_permission,
            backing_file,
            file_size,
            offset,
            shared,
            writable,
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
    inner
        .memory_set
        .mprotect_area(
            addr,
            len,
            prot_to_map_permission(prot),
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
    for flush in flushes {
        flush.write_back();
    }
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
    // fail with EBUSY for locked pages. This kernel has no mlock state and no
    // cross-process invalidation model yet, so it only validates the mapping
    // range and writes back dirty shared mmap pages.
    for flush in flushes {
        flush.write_back();
    }
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
    }
}
