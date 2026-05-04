use crate::config::PAGE_SIZE;
use crate::mm::{MapPermission, MemoryProtectError};
use crate::task::current_process;

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

pub fn sys_brk(addr: usize) -> SysResult {
    let process = current_process();
    let mut inner = process.inner_exclusive_access();
    Ok(inner.memory_set.set_program_break(addr) as isize)
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
    if fixed && addr % PAGE_SIZE != 0 {
        return Err(SysError::EINVAL);
    }
    if fixed && addr == 0 {
        return Err(SysError::EINVAL);
    }

    let process = current_process();
    let (backing_file, file_size) = if anonymous {
        (None, 0)
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
        let file_size = file.stat()?.size as usize;
        (Some(file), file_size)
    };

    let mut inner = process.inner_exclusive_access();
    if fixed {
        let (mapped_addr, flushes) = inner
            .memory_set
            .mmap_fixed_area(
                addr,
                len,
                permission,
                backing_file,
                file_size,
                offset,
                shared,
                writable,
            )
            .ok_or(SysError::ENOMEM)?;
        drop(inner);
        for flush in flushes {
            flush.write_back();
        }
        return Ok(mapped_addr);
    }

    // TODO: why dose map permission do not contain shared and writable
    inner
        .memory_set
        .mmap_area(
            len,
            permission,
            backing_file,
            file_size,
            offset,
            shared,
            writable,
        )
        .ok_or(SysError::ENOMEM)
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
        .mprotect_area(addr, len, prot_to_map_permission(prot))
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
