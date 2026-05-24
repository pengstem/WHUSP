use crate::mm::{MemorySet, MmapFaultAccess, PageTable, StepByOne, VirtAddr};
use crate::perf;
use alloc::string::String;
use alloc::vec::Vec;
use core::mem::{MaybeUninit, size_of};

use super::errno::{SysError, SysResult};

const USER_COPY_SAME_PAGE_FAST_MAX: usize = 64;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum UserBufferAccess {
    Read,
    Write,
}

/// Optional page-fault hook used while validating a user byte range.
///
/// Callers pass this only when the syscall is allowed to materialize lazy user
/// mappings before copying. A `false` return is reported as `EFAULT`.
pub(crate) type UserFaultHandler = fn(usize, UserBufferAccess) -> bool;

fn mmap_user_fault(addr: usize, access: UserBufferAccess) -> bool {
    let access = match access {
        UserBufferAccess::Read => MmapFaultAccess::Read,
        UserBufferAccess::Write => MmapFaultAccess::Write,
    };
    crate::arch::trap::handle_user_page_fault(addr, access)
}

pub(crate) fn translated_byte_buffer_checked(
    token: usize,
    ptr: *const u8,
    len: usize,
    access: UserBufferAccess,
) -> SysResult<Vec<&'static mut [u8]>> {
    translated_byte_buffer_checked_with_fault(token, ptr, len, access, None)
}

/// Validates a user byte range and faults in mmap-backed pages when needed.
///
/// Use this only for syscall copy paths where Linux-visible behavior includes
/// touching lazy user mappings as part of the copy itself.
pub(crate) fn translated_byte_buffer_checked_with_mmap_fault(
    token: usize,
    ptr: *const u8,
    len: usize,
    access: UserBufferAccess,
) -> SysResult<Vec<&'static mut [u8]>> {
    // CONTEXT: plain metadata copy helpers use `translated_byte_buffer_checked`
    // so an unmapped user range still returns `EFAULT` without invoking the
    // mmap fault handler from an unrelated ABI path.
    translated_byte_buffer_checked_with_fault(token, ptr, len, access, Some(mmap_user_fault))
}

/// Validates a user byte range and returns physical page slices covering it.
///
/// The returned slices are only valid for the current syscall copy window. This
/// helper performs permission checks for Linux-visible `EFAULT`; it does not
/// own address-space policy beyond optionally calling the supplied fault hook.
pub(crate) fn translated_byte_buffer_checked_with_fault(
    token: usize,
    ptr: *const u8,
    len: usize,
    access: UserBufferAccess,
    fault_handler: Option<UserFaultHandler>,
) -> SysResult<Vec<&'static mut [u8]>> {
    if len == 0 {
        return Ok(Vec::new());
    }
    let mut start = ptr as usize;
    let end = start.checked_add(len).ok_or(SysError::EFAULT)?;
    let page_table = PageTable::from_token(token);
    let mut buffers = Vec::new();
    while start < end {
        let start_va = VirtAddr::from(start);
        let mut vpn = start_va.floor();
        let pte = checked_user_pte(&page_table, token, start, access, fault_handler)?;
        let ppn = pte.ppn();
        vpn.step();
        let mut end_va: VirtAddr = vpn.into();
        end_va = end_va.min(VirtAddr::from(end));
        if end_va.page_offset() == 0 {
            buffers.push(&mut ppn.get_bytes_array()[start_va.page_offset()..]);
        } else {
            buffers.push(&mut ppn.get_bytes_array()[start_va.page_offset()..end_va.page_offset()]);
        }
        start = end_va.into();
    }
    Ok(buffers)
}

fn checked_user_pte(
    page_table: &PageTable,
    token: usize,
    addr: usize,
    access: UserBufferAccess,
    fault_handler: Option<UserFaultHandler>,
) -> SysResult<crate::mm::PageTableEntry> {
    let vpn = VirtAddr::from(addr).floor();
    let mut pte = match page_table.translate(vpn) {
        Some(pte) => pte,
        None => {
            let Some(fault_handler) = fault_handler else {
                return Err(SysError::EFAULT);
            };
            if !fault_handler(addr, access) {
                return Err(SysError::EFAULT);
            }
            page_table.translate(vpn).ok_or(SysError::EFAULT)?
        }
    };
    let reject_zero_ppn = fault_handler.is_some();
    if !user_pte_allows(pte, access, reject_zero_ppn) {
        if access == UserBufferAccess::Write && pte.cow() && !pte.writable() {
            if !resolve_current_cow_page(token, addr) {
                return Err(SysError::EFAULT);
            }
            pte = page_table.translate(vpn).ok_or(SysError::EFAULT)?;
        } else if let Some(fault_handler) = fault_handler
            && fault_handler(addr, access)
        {
            pte = page_table.translate(vpn).ok_or(SysError::EFAULT)?;
        }
        if !user_pte_allows(pte, access, reject_zero_ppn) {
            return Err(SysError::EFAULT);
        }
    }
    Ok(pte)
}

fn try_same_page_user_slice(
    token: usize,
    ptr: *const u8,
    len: usize,
    access: UserBufferAccess,
    fault_handler: Option<UserFaultHandler>,
) -> Option<SysResult<&'static mut [u8]>> {
    if len == 0 || len > USER_COPY_SAME_PAGE_FAST_MAX {
        return None;
    }
    let start = ptr as usize;
    let end = match start.checked_add(len) {
        Some(end) => end,
        None => return Some(Err(SysError::EFAULT)),
    };
    let start_va = VirtAddr::from(start);
    if start_va.floor() != VirtAddr::from(end - 1).floor() {
        return None;
    }

    let page_table = PageTable::from_token(token);
    let pte = match checked_user_pte(&page_table, token, start, access, fault_handler) {
        Ok(pte) => pte,
        Err(err) => return Some(Err(err)),
    };
    let offset = start_va.page_offset();
    Some(Ok(&mut pte.ppn().get_bytes_array()[offset..offset + len]))
}

fn resolve_current_cow_page(token: usize, addr: usize) -> bool {
    if token != crate::task::current_user_token() {
        return false;
    }
    crate::arch::trap::handle_user_page_fault(addr, MmapFaultAccess::Write)
}

fn user_pte_allows(
    pte: crate::mm::PageTableEntry,
    access: UserBufferAccess,
    reject_zero_ppn: bool,
) -> bool {
    if !pte.is_valid() || (reject_zero_ppn && pte.ppn().0 == 0) {
        return false;
    }
    match access {
        UserBufferAccess::Read => pte.readable(),
        UserBufferAccess::Write => pte.writable(),
    }
}

pub(crate) const PATH_MAX: usize = 4096;

/// Reads a NUL-terminated string from user memory with an explicit length cap.
///
/// Returns `EFAULT` for invalid user memory and `ENAMETOOLONG` when no NUL byte
/// is found within `max_len`, matching Linux pathname-style ABI boundaries.
pub(crate) fn read_user_c_string(
    token: usize,
    ptr: *const u8,
    max_len: usize,
) -> SysResult<String> {
    if ptr.is_null() {
        return Err(SysError::EFAULT);
    }

    let mut string = String::with_capacity(64);
    let mut offset = 0usize;
    perf::record_user_c_string_call();
    while offset < max_len {
        let addr = (ptr as usize).checked_add(offset).ok_or(SysError::EFAULT)?;
        let page_remaining = crate::config::PAGE_SIZE - (addr & (crate::config::PAGE_SIZE - 1));
        let chunk_len = page_remaining.min(max_len - offset);
        let buffers = translated_byte_buffer_checked_with_fault(
            token,
            addr as *const u8,
            chunk_len,
            UserBufferAccess::Read,
            Some(mmap_user_fault),
        )?;
        for buffer in &buffers {
            let (text_len, found_nul, is_ascii) = scan_c_string_chunk(buffer);
            let text = &buffer[..text_len];
            perf::record_user_c_string_chunk(text_len + usize::from(found_nul), text_len, is_ascii);
            append_user_string_bytes(&mut string, text, is_ascii);
            if found_nul {
                return Ok(string);
            }
        }
        offset += chunk_len;
    }
    Err(SysError::ENAMETOOLONG)
}

fn scan_c_string_chunk(buffer: &[u8]) -> (usize, bool, bool) {
    let mut is_ascii = true;
    for (idx, &byte) in buffer.iter().enumerate() {
        if byte == 0 {
            return (idx, true, is_ascii);
        }
        is_ascii &= byte.is_ascii();
    }
    (buffer.len(), false, is_ascii)
}

fn append_user_string_bytes(string: &mut String, bytes: &[u8], is_ascii: bool) {
    if bytes.is_empty() {
        return;
    }
    if is_ascii {
        // ASCII bytes are always valid UTF-8, so this preserves the existing
        // byte-to-char behavior while appending the common pathname case in bulk.
        string.push_str(unsafe { core::str::from_utf8_unchecked(bytes) });
        return;
    }
    for &byte in bytes {
        // UNFINISHED: Linux pathnames are byte strings except for NUL. This
        // syscall layer stores them as Rust `String`, so non-ASCII pathname
        // bytes are not preserved byte-for-byte yet.
        string.push(byte as char);
    }
}

pub(crate) fn read_user_usize(token: usize, addr: usize) -> SysResult<usize> {
    read_user_value(token, addr as *const usize)
}

/// Copies one plain ABI value from a user array after checked index arithmetic.
///
/// The element is copied byte-for-byte through the checked user access path, so
/// the user pointer does not need Rust alignment. Address arithmetic overflow
/// is reported as `EFAULT`, consistent with existing iovec readers.
pub(crate) fn read_user_array_item<T: Copy>(
    token: usize,
    ptr: *const T,
    index: usize,
) -> SysResult<T> {
    read_user_value(token, user_array_item_addr(ptr, index)? as *const T)
}

/// Writes one plain ABI value into a user array after checked index arithmetic.
pub(crate) fn write_user_array_item<T: Copy>(
    token: usize,
    ptr: *mut T,
    index: usize,
    value: &T,
) -> SysResult<()> {
    write_user_value(token, user_array_item_addr(ptr, index)? as *mut T, value)
}

fn user_array_item_addr<T>(ptr: *const T, index: usize) -> SysResult<usize> {
    let entry_size = size_of::<T>();
    (ptr as usize)
        .checked_add(index.checked_mul(entry_size).ok_or(SysError::EFAULT)?)
        .ok_or(SysError::EFAULT)
}

fn copy_from_user(
    token: usize,
    ptr: *const u8,
    dst: &mut [u8],
    fault_handler: Option<UserFaultHandler>,
) -> SysResult<()> {
    if dst.is_empty() {
        return Ok(());
    }
    if let Some(buffer) =
        try_same_page_user_slice(token, ptr, dst.len(), UserBufferAccess::Read, fault_handler)
    {
        let buffer = buffer?;
        dst.copy_from_slice(buffer);
        perf::record_usercopy_same_page_fast(perf::UsercopyAccess::Read, dst.len());
        return Ok(());
    }
    let buffers = translated_byte_buffer_checked_with_fault(
        token,
        ptr,
        dst.len(),
        UserBufferAccess::Read,
        fault_handler,
    )?;
    perf::record_usercopy_slow_path(buffers.len());
    let mut copied = 0usize;
    for buffer in buffers.iter() {
        let next = copied + buffer.len();
        dst[copied..next].copy_from_slice(buffer);
        copied = next;
    }
    Ok(())
}

fn copy_to_user_buffers(buffers: Vec<&'static mut [u8]>, src: &[u8]) {
    let mut copied = 0usize;
    for buffer in buffers {
        let next = copied + buffer.len();
        buffer.copy_from_slice(&src[copied..next]);
        copied = next;
    }
}

fn resolve_cow_write_range_in_memory_set(
    memory_set: &mut MemorySet,
    ptr: *mut u8,
    len: usize,
) -> SysResult<()> {
    if len == 0 {
        return Ok(());
    }
    let mut start = ptr as usize;
    let end = start.checked_add(len).ok_or(SysError::EFAULT)?;
    while start < end {
        let start_va = VirtAddr::from(start);
        let vpn = start_va.floor();
        let pte = memory_set.translate(vpn).ok_or(SysError::EFAULT)?;
        if pte.cow() && !pte.writable() && !memory_set.resolve_cow_page_fault(start) {
            return Err(SysError::EFAULT);
        }
        let pte = memory_set.translate(vpn).ok_or(SysError::EFAULT)?;
        if !user_pte_allows(pte, UserBufferAccess::Write, false) {
            return Err(SysError::EFAULT);
        }
        let mut next_vpn = vpn;
        next_vpn.step();
        let next_va: VirtAddr = next_vpn.into();
        start = usize::from(next_va).min(end);
    }
    Ok(())
}

/// Copies kernel bytes into a user buffer after validating write permission.
pub(crate) fn copy_to_user(token: usize, ptr: *mut u8, src: &[u8]) -> SysResult<()> {
    copy_to_user_with_fault(token, ptr, src, None)
}

pub(crate) fn copy_to_user_with_fault(
    token: usize,
    ptr: *mut u8,
    src: &[u8],
    fault_handler: Option<UserFaultHandler>,
) -> SysResult<()> {
    if src.is_empty() {
        return Ok(());
    }
    if let Some(buffer) = try_same_page_user_slice(
        token,
        ptr.cast_const(),
        src.len(),
        UserBufferAccess::Write,
        fault_handler,
    ) {
        let buffer = buffer?;
        buffer.copy_from_slice(src);
        perf::record_usercopy_same_page_fast(perf::UsercopyAccess::Write, src.len());
        return Ok(());
    }
    let buffers = translated_byte_buffer_checked_with_fault(
        token,
        ptr.cast_const(),
        src.len(),
        UserBufferAccess::Write,
        fault_handler,
    )?;
    perf::record_usercopy_slow_path(buffers.len());
    copy_to_user_buffers(buffers, src);
    Ok(())
}

pub(crate) fn copy_to_user_in_memory_set(
    memory_set: &mut MemorySet,
    ptr: *mut u8,
    src: &[u8],
) -> SysResult<()> {
    // Used for child or freshly exec'd address spaces, not necessarily the
    // current task. Resolve COW against the supplied MemorySet before translating
    // through its token.
    resolve_cow_write_range_in_memory_set(memory_set, ptr, src.len())?;
    let buffers = translated_byte_buffer_checked(
        memory_set.token(),
        ptr.cast_const(),
        src.len(),
        UserBufferAccess::Write,
    )?;
    copy_to_user_buffers(buffers, src);
    Ok(())
}

/// Reads one plain ABI value from user memory.
///
/// The value is copied through bytes rather than dereferenced directly, so this
/// is safe for unaligned user ABI structs as long as `T: Copy`.
pub(crate) fn read_user_value<T: Copy>(token: usize, ptr: *const T) -> SysResult<T> {
    read_user_value_with_fault(token, ptr, None)
}

pub(crate) fn read_user_value_with_mmap_fault<T: Copy>(
    token: usize,
    ptr: *const T,
) -> SysResult<T> {
    read_user_value_with_fault(token, ptr, Some(mmap_user_fault))
}

pub(crate) fn read_user_value_with_fault<T: Copy>(
    token: usize,
    ptr: *const T,
    fault_handler: Option<UserFaultHandler>,
) -> SysResult<T> {
    let mut value = MaybeUninit::<T>::uninit();
    let bytes =
        unsafe { core::slice::from_raw_parts_mut(value.as_mut_ptr().cast::<u8>(), size_of::<T>()) };
    copy_from_user(token, ptr.cast::<u8>(), bytes, fault_handler)?;
    Ok(unsafe { value.assume_init() })
}

/// Writes one plain ABI value into user memory after checking access rights.
pub(crate) fn write_user_value<T: Copy>(token: usize, ptr: *mut T, value: &T) -> SysResult<()> {
    write_user_value_with_fault(token, ptr, value, None)
}

pub(crate) fn write_user_value_in_memory_set<T: Copy>(
    memory_set: &mut MemorySet,
    ptr: *mut T,
    value: &T,
) -> SysResult<()> {
    let bytes =
        unsafe { core::slice::from_raw_parts((value as *const T).cast::<u8>(), size_of::<T>()) };
    copy_to_user_in_memory_set(memory_set, ptr.cast::<u8>(), bytes)
}

pub(crate) fn write_user_value_with_mmap_fault<T: Copy>(
    token: usize,
    ptr: *mut T,
    value: &T,
) -> SysResult<()> {
    write_user_value_with_fault(token, ptr, value, Some(mmap_user_fault))
}

pub(crate) fn write_user_value_with_fault<T: Copy>(
    token: usize,
    ptr: *mut T,
    value: &T,
    fault_handler: Option<UserFaultHandler>,
) -> SysResult<()> {
    let bytes =
        unsafe { core::slice::from_raw_parts((value as *const T).cast::<u8>(), size_of::<T>()) };
    copy_to_user_with_fault(token, ptr.cast::<u8>(), bytes, fault_handler)
}
