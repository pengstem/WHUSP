use crate::mm::{MemorySet, MmapFaultAccess, PageTable, StepByOne, VirtAddr};
use crate::perf;
use alloc::string::String;
use alloc::vec::Vec;
use core::mem::{MaybeUninit, size_of};

use super::SyscallContext;
use super::errno::{SysError, SysResult};

// These knobs affect only allocation/cache behavior for small ABI values. The
// fast path still goes through checked_user_pte(), so permissions, COW, and
// optional fault-in semantics must match the multi-page copy path.
const USER_COPY_SAME_PAGE_FAST_MAX: usize = 64;
const USER_COPY_LEAF_PTE_CACHE: bool = true;

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

#[derive(Clone, Copy)]
enum EffectiveUserFault<'a> {
    None,
    Function(UserFaultHandler),
    CurrentLazyFramed(&'a crate::task::ProcessControlBlock),
}

#[derive(Clone, Copy)]
struct UserFaultResolver<'a> {
    fault: EffectiveUserFault<'a>,
    current_process: Option<&'a crate::task::ProcessControlBlock>,
}

impl<'a> UserFaultResolver<'a> {
    fn none() -> Self {
        Self {
            fault: EffectiveUserFault::None,
            current_process: None,
        }
    }

    fn from_function(fault_handler: UserFaultHandler) -> Self {
        Self {
            fault: EffectiveUserFault::Function(fault_handler),
            current_process: None,
        }
    }

    fn from_current_lazy_framed(process: &'a crate::task::ProcessControlBlock) -> Self {
        Self {
            fault: EffectiveUserFault::CurrentLazyFramed(process),
            current_process: Some(process),
        }
    }

    fn with_current_process(
        fault_handler: UserFaultHandler,
        process: &'a crate::task::ProcessControlBlock,
    ) -> Self {
        Self {
            fault: EffectiveUserFault::Function(fault_handler),
            current_process: Some(process),
        }
    }

    fn can_fault(&self) -> bool {
        !matches!(self.fault, EffectiveUserFault::None)
    }

    fn resolve(&self, addr: usize, access: UserBufferAccess) -> bool {
        match self.fault {
            EffectiveUserFault::None => false,
            EffectiveUserFault::Function(handler) => handler(addr, access),
            EffectiveUserFault::CurrentLazyFramed(process) => {
                lazy_framed_user_fault_for_process(process, addr, access)
            }
        }
    }

    fn resolve_cow(&self, token: usize, addr: usize) -> bool {
        if let Some(process) = self.current_process {
            return process
                .inner_exclusive_access()
                .memory_set
                .resolve_cow_page_fault(addr);
        }
        resolve_current_cow_page(token, addr)
    }
}

fn mmap_user_fault(addr: usize, access: UserBufferAccess) -> bool {
    let access = match access {
        UserBufferAccess::Read => MmapFaultAccess::Read,
        UserBufferAccess::Write => MmapFaultAccess::Write,
    };
    crate::arch::trap::handle_user_page_fault(addr, access)
}

fn lazy_framed_user_fault(addr: usize, access: UserBufferAccess) -> bool {
    let process = crate::task::current_process();
    lazy_framed_user_fault_for_process(&process, addr, access)
}

fn lazy_framed_user_fault_for_process(
    process: &crate::task::ProcessControlBlock,
    addr: usize,
    access: UserBufferAccess,
) -> bool {
    let access = match access {
        UserBufferAccess::Read => MmapFaultAccess::Read,
        UserBufferAccess::Write => MmapFaultAccess::Write,
    };
    process
        .inner_exclusive_access()
        .memory_set
        .resolve_lazy_framed_page_fault(addr, access)
}

fn effective_user_fault_resolver(
    token: usize,
    fault_handler: Option<UserFaultHandler>,
) -> UserFaultResolver<'static> {
    if let Some(fault_handler) = fault_handler {
        return UserFaultResolver::from_function(fault_handler);
    }
    let Some(task) = crate::task::current_task() else {
        return UserFaultResolver::none();
    };
    let Some(process) = task.process.upgrade() else {
        return UserFaultResolver::none();
    };
    // Default lazy-framed faults are safe only for the current process token.
    // Child or foreign address spaces must use explicit MemorySet copy helpers
    // so user-stack setup and ptrace writes do not fault the wrong process.
    let Some(inner) = process.try_inner_exclusive_access() else {
        return UserFaultResolver::none();
    };
    if inner.memory_set.token() == token {
        UserFaultResolver::from_function(lazy_framed_user_fault)
    } else {
        UserFaultResolver::none()
    }
}

fn effective_user_fault_resolver_for_ctx<'a>(
    ctx: &'a SyscallContext,
    token: usize,
    fault_handler: Option<UserFaultHandler>,
) -> UserFaultResolver<'a> {
    let current_process = (token == ctx.user_token()).then_some(ctx.process().as_ref());
    if let Some(fault_handler) = fault_handler {
        return if let Some(process) = current_process {
            UserFaultResolver::with_current_process(fault_handler, process)
        } else {
            UserFaultResolver::from_function(fault_handler)
        };
    }
    if let Some(process) = current_process {
        UserFaultResolver::from_current_lazy_framed(process)
    } else {
        UserFaultResolver::none()
    }
}

pub(crate) fn translated_byte_buffer_checked(
    token: usize,
    ptr: *const u8,
    len: usize,
    access: UserBufferAccess,
) -> SysResult<Vec<&'static mut [u8]>> {
    translated_byte_buffer_checked_with_fault(token, ptr, len, access, None)
}

#[allow(dead_code)]
pub(crate) fn translated_byte_buffer_checked_ctx(
    ctx: &SyscallContext,
    ptr: *const u8,
    len: usize,
    access: UserBufferAccess,
) -> SysResult<Vec<&'static mut [u8]>> {
    translated_byte_buffer_checked_with_fault_ctx(ctx, ptr, len, access, None)
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

pub(crate) fn translated_byte_buffer_checked_with_mmap_fault_ctx(
    ctx: &SyscallContext,
    ptr: *const u8,
    len: usize,
    access: UserBufferAccess,
) -> SysResult<Vec<&'static mut [u8]>> {
    translated_byte_buffer_checked_with_fault_ctx(ctx, ptr, len, access, Some(mmap_user_fault))
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
    let fault_handler = effective_user_fault_resolver(token, fault_handler);
    translated_byte_buffer_checked_with_resolver(token, ptr, len, access, fault_handler)
}

fn translated_byte_buffer_checked_with_fault_ctx(
    ctx: &SyscallContext,
    ptr: *const u8,
    len: usize,
    access: UserBufferAccess,
    fault_handler: Option<UserFaultHandler>,
) -> SysResult<Vec<&'static mut [u8]>> {
    let token = ctx.user_token();
    let fault_handler = effective_user_fault_resolver_for_ctx(ctx, token, fault_handler);
    translated_byte_buffer_checked_with_resolver(token, ptr, len, access, fault_handler)
}

fn translated_byte_buffer_checked_with_resolver(
    token: usize,
    ptr: *const u8,
    len: usize,
    access: UserBufferAccess,
    fault_handler: UserFaultResolver<'_>,
) -> SysResult<Vec<&'static mut [u8]>> {
    if len == 0 {
        return Ok(Vec::new());
    }
    // CONTEXT: brk growth is VMA-reserved and materialized lazily. Default
    // current-process syscall copies should fault those framed pages in, while
    // full mmap fault handling remains opt-in through the explicit mmap helper.
    let mut start = ptr as usize;
    let end = start.checked_add(len).ok_or(SysError::EFAULT)?;
    let page_table = PageTable::from_token(token);
    let mut buffers = Vec::new();
    while start < end {
        let start_va = VirtAddr::from(start);
        let mut vpn = start_va.floor();
        let pte = checked_user_pte(&page_table, token, start, access, fault_handler, false)?;
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
    perf::record_usercopy_checked_range(buffers.len(), len);
    Ok(buffers)
}

fn checked_user_pte(
    page_table: &PageTable,
    token: usize,
    addr: usize,
    access: UserBufferAccess,
    fault_handler: UserFaultResolver<'_>,
    use_leaf_cache: bool,
) -> SysResult<crate::mm::PageTableEntry> {
    // Passing a fault handler means the copy is allowed to mutate the current
    // process mappings by resolving lazy mmap/COW faults. Cross-address-space
    // copies should use the explicit MemorySet helpers instead.
    let vpn = VirtAddr::from(addr).floor();
    let translate = |page_table: &PageTable| {
        if use_leaf_cache {
            page_table.translate_cached_user_leaf(token, vpn)
        } else {
            page_table.translate(vpn)
        }
    };
    let mut pte = match translate(page_table) {
        Some(pte) => pte,
        None => {
            if !fault_handler.can_fault() {
                return Err(SysError::EFAULT);
            }
            if !fault_handler.resolve(addr, access) {
                return Err(SysError::EFAULT);
            }
            translate(page_table).ok_or(SysError::EFAULT)?
        }
    };
    let reject_zero_ppn = fault_handler.can_fault();
    if !user_pte_allows(pte, access, reject_zero_ppn) {
        if access == UserBufferAccess::Write && pte.cow() && !pte.writable() {
            // COW resolution precedes the generic mmap hook so fork-private
            // pages become writable instead of being reported as EFAULT.
            if !fault_handler.resolve_cow(token, addr) {
                return Err(SysError::EFAULT);
            }
            pte = translate(page_table).ok_or(SysError::EFAULT)?;
        } else if fault_handler.can_fault() && fault_handler.resolve(addr, access) {
            pte = translate(page_table).ok_or(SysError::EFAULT)?;
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
    fault_handler: UserFaultResolver<'_>,
) -> Option<SysResult<&'static mut [u8]>> {
    // This is only an allocation-saving fast path for short ABI scalars. It
    // still goes through checked_user_pte(), so permission, COW, and optional
    // mmap-fault behavior match the multi-page copy path.
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
    let pte = match checked_user_pte(
        &page_table,
        token,
        start,
        access,
        fault_handler,
        USER_COPY_LEAF_PTE_CACHE,
    ) {
        Ok(pte) => pte,
        Err(err) => return Some(Err(err)),
    };
    let offset = start_va.page_offset();
    Some(Ok(&mut pte.ppn().get_bytes_array()[offset..offset + len]))
}

fn resolve_current_cow_page(token: usize, addr: usize) -> bool {
    // CONTEXT: COW fault resolution may take the current process memory lock
    // and update its page table. Cross-process writers such as ptrace must use
    // memory-set aware copy helpers instead of this current-token fast path.
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

pub(crate) fn read_user_c_string_ctx(
    ctx: &SyscallContext,
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
        let buffers = translated_byte_buffer_checked_with_mmap_fault_ctx(
            ctx,
            addr as *const u8,
            chunk_len,
            UserBufferAccess::Read,
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
    read_user_value_with_site(
        token,
        addr as *const usize,
        None,
        perf::UsercopySite::ReadUsize,
    )
}

#[allow(dead_code)]
pub(crate) fn read_user_usize_ctx(ctx: &SyscallContext, addr: usize) -> SysResult<usize> {
    read_user_value_with_site_ctx(
        ctx,
        addr as *const usize,
        None,
        perf::UsercopySite::ReadUsize,
    )
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
    read_user_value_with_site(
        token,
        user_array_item_addr(ptr, index)? as *const T,
        None,
        perf::UsercopySite::ReadArrayItem,
    )
}

#[allow(dead_code)]
pub(crate) fn read_user_array_item_ctx<T: Copy>(
    ctx: &SyscallContext,
    ptr: *const T,
    index: usize,
) -> SysResult<T> {
    read_user_value_with_site_ctx(
        ctx,
        user_array_item_addr(ptr, index)? as *const T,
        None,
        perf::UsercopySite::ReadArrayItem,
    )
}

/// Copies a plain ABI array from user memory in one checked user-copy window.
pub(crate) fn read_user_array<T: Copy>(
    token: usize,
    ptr: *const T,
    count: usize,
) -> SysResult<Vec<T>> {
    if count == 0 {
        return Ok(Vec::new());
    }
    if ptr.is_null() {
        return Err(SysError::EFAULT);
    }

    let byte_len = user_array_byte_len::<T>(count)?;
    let mut values = Vec::<MaybeUninit<T>>::with_capacity(count);
    let bytes =
        unsafe { core::slice::from_raw_parts_mut(values.as_mut_ptr().cast::<u8>(), byte_len) };
    perf::record_usercopy_site(perf::UsercopySite::ReadArrayItem, byte_len);
    copy_from_user(token, ptr.cast::<u8>(), bytes, None)?;

    unsafe {
        values.set_len(count);
        let ptr = values.as_mut_ptr().cast::<T>();
        let len = values.len();
        let capacity = values.capacity();
        core::mem::forget(values);
        Ok(Vec::from_raw_parts(ptr, len, capacity))
    }
}

pub(crate) fn read_user_array_ctx<T: Copy>(
    ctx: &SyscallContext,
    ptr: *const T,
    count: usize,
) -> SysResult<Vec<T>> {
    if count == 0 {
        return Ok(Vec::new());
    }
    if ptr.is_null() {
        return Err(SysError::EFAULT);
    }

    let byte_len = user_array_byte_len::<T>(count)?;
    let mut values = Vec::<MaybeUninit<T>>::with_capacity(count);
    let bytes =
        unsafe { core::slice::from_raw_parts_mut(values.as_mut_ptr().cast::<u8>(), byte_len) };
    perf::record_usercopy_site(perf::UsercopySite::ReadArrayItem, byte_len);
    copy_from_user_ctx(ctx, ptr.cast::<u8>(), bytes, None)?;

    unsafe {
        values.set_len(count);
        let ptr = values.as_mut_ptr().cast::<T>();
        let len = values.len();
        let capacity = values.capacity();
        core::mem::forget(values);
        Ok(Vec::from_raw_parts(ptr, len, capacity))
    }
}

/// Copies a plain ABI array into user memory in one checked user-copy window.
pub(crate) fn write_user_array<T: Copy>(token: usize, ptr: *mut T, values: &[T]) -> SysResult<()> {
    if values.is_empty() {
        return Ok(());
    }
    if ptr.is_null() {
        return Err(SysError::EFAULT);
    }

    let byte_len = user_array_byte_len::<T>(values.len())?;
    let bytes = unsafe { core::slice::from_raw_parts(values.as_ptr().cast::<u8>(), byte_len) };
    copy_to_user_with_site(
        token,
        ptr.cast::<u8>(),
        bytes,
        None,
        perf::UsercopySite::WriteArrayItem,
    )
}

#[allow(dead_code)]
pub(crate) fn write_user_array_ctx<T: Copy>(
    ctx: &SyscallContext,
    ptr: *mut T,
    values: &[T],
) -> SysResult<()> {
    if values.is_empty() {
        return Ok(());
    }
    if ptr.is_null() {
        return Err(SysError::EFAULT);
    }

    let byte_len = user_array_byte_len::<T>(values.len())?;
    let bytes = unsafe { core::slice::from_raw_parts(values.as_ptr().cast::<u8>(), byte_len) };
    copy_to_user_with_site_ctx(
        ctx,
        ptr.cast::<u8>(),
        bytes,
        None,
        perf::UsercopySite::WriteArrayItem,
    )
}

fn user_array_item_addr<T>(ptr: *const T, index: usize) -> SysResult<usize> {
    (ptr as usize)
        .checked_add(user_array_byte_len::<T>(index)?)
        .ok_or(SysError::EFAULT)
}

fn user_array_byte_len<T>(count: usize) -> SysResult<usize> {
    count.checked_mul(size_of::<T>()).ok_or(SysError::EFAULT)
}

fn copy_from_user(
    token: usize,
    ptr: *const u8,
    dst: &mut [u8],
    fault_handler: Option<UserFaultHandler>,
) -> SysResult<()> {
    let fault_handler = effective_user_fault_resolver(token, fault_handler);
    copy_from_user_with_resolver(token, ptr, dst, fault_handler)
}

fn copy_from_user_ctx(
    ctx: &SyscallContext,
    ptr: *const u8,
    dst: &mut [u8],
    fault_handler: Option<UserFaultHandler>,
) -> SysResult<()> {
    let token = ctx.user_token();
    let fault_handler = effective_user_fault_resolver_for_ctx(ctx, token, fault_handler);
    copy_from_user_with_resolver(token, ptr, dst, fault_handler)
}

fn copy_from_user_with_resolver(
    token: usize,
    ptr: *const u8,
    dst: &mut [u8],
    fault_handler: UserFaultResolver<'_>,
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
    let buffers = translated_byte_buffer_checked_with_resolver(
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
    copy_to_user_with_site(token, ptr, src, None, perf::UsercopySite::CopyToUser)
}

pub(crate) fn copy_to_user_ctx(ctx: &SyscallContext, ptr: *mut u8, src: &[u8]) -> SysResult<()> {
    copy_to_user_with_site_ctx(ctx, ptr, src, None, perf::UsercopySite::CopyToUser)
}

fn copy_to_user_with_site(
    token: usize,
    ptr: *mut u8,
    src: &[u8],
    fault_handler: Option<UserFaultHandler>,
    site: perf::UsercopySite,
) -> SysResult<()> {
    let fault_handler = effective_user_fault_resolver(token, fault_handler);
    copy_to_user_with_resolver(token, ptr, src, fault_handler, site)
}

fn copy_to_user_with_site_ctx(
    ctx: &SyscallContext,
    ptr: *mut u8,
    src: &[u8],
    fault_handler: Option<UserFaultHandler>,
    site: perf::UsercopySite,
) -> SysResult<()> {
    let token = ctx.user_token();
    let fault_handler = effective_user_fault_resolver_for_ctx(ctx, token, fault_handler);
    copy_to_user_with_resolver(token, ptr, src, fault_handler, site)
}

fn copy_to_user_with_resolver(
    token: usize,
    ptr: *mut u8,
    src: &[u8],
    fault_handler: UserFaultResolver<'_>,
    site: perf::UsercopySite,
) -> SysResult<()> {
    perf::record_usercopy_site(site, src.len());
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
    let buffers = translated_byte_buffer_checked_with_resolver(
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
    perf::record_usercopy_site(perf::UsercopySite::CopyToUserInMemorySet, src.len());
    // Used for child or freshly exec'd address spaces, not necessarily the
    // current task. Resolve COW against the supplied MemorySet before translating
    // through its token, and do not invoke the current-task mmap fault handler.
    // Exec and fork setup rely on this to keep user-stack writes scoped to the
    // address space being constructed.
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
    read_user_value_with_site(token, ptr, None, perf::UsercopySite::ReadValue)
}

#[allow(dead_code)]
pub(crate) fn read_user_value_ctx<T: Copy>(ctx: &SyscallContext, ptr: *const T) -> SysResult<T> {
    read_user_value_with_site_ctx(ctx, ptr, None, perf::UsercopySite::ReadValue)
}

pub(crate) fn read_user_value_with_mmap_fault<T: Copy>(
    token: usize,
    ptr: *const T,
) -> SysResult<T> {
    read_user_value_with_site(
        token,
        ptr,
        Some(mmap_user_fault),
        perf::UsercopySite::ReadValue,
    )
}

#[allow(dead_code)]
pub(crate) fn read_user_value_with_mmap_fault_ctx<T: Copy>(
    ctx: &SyscallContext,
    ptr: *const T,
) -> SysResult<T> {
    read_user_value_with_site_ctx(
        ctx,
        ptr,
        Some(mmap_user_fault),
        perf::UsercopySite::ReadValue,
    )
}

pub(crate) fn read_user_value_with_fault<T: Copy>(
    token: usize,
    ptr: *const T,
    fault_handler: Option<UserFaultHandler>,
) -> SysResult<T> {
    read_user_value_with_site(token, ptr, fault_handler, perf::UsercopySite::ReadValue)
}

#[allow(dead_code)]
pub(crate) fn read_user_value_with_fault_ctx<T: Copy>(
    ctx: &SyscallContext,
    ptr: *const T,
    fault_handler: Option<UserFaultHandler>,
) -> SysResult<T> {
    read_user_value_with_site_ctx(ctx, ptr, fault_handler, perf::UsercopySite::ReadValue)
}

fn read_user_value_with_site<T: Copy>(
    token: usize,
    ptr: *const T,
    fault_handler: Option<UserFaultHandler>,
    site: perf::UsercopySite,
) -> SysResult<T> {
    let mut value = MaybeUninit::<T>::uninit();
    let bytes =
        unsafe { core::slice::from_raw_parts_mut(value.as_mut_ptr().cast::<u8>(), size_of::<T>()) };
    perf::record_usercopy_site(site, bytes.len());
    copy_from_user(token, ptr.cast::<u8>(), bytes, fault_handler)?;
    Ok(unsafe { value.assume_init() })
}

#[allow(dead_code)]
fn read_user_value_with_site_ctx<T: Copy>(
    ctx: &SyscallContext,
    ptr: *const T,
    fault_handler: Option<UserFaultHandler>,
    site: perf::UsercopySite,
) -> SysResult<T> {
    let mut value = MaybeUninit::<T>::uninit();
    let bytes =
        unsafe { core::slice::from_raw_parts_mut(value.as_mut_ptr().cast::<u8>(), size_of::<T>()) };
    perf::record_usercopy_site(site, bytes.len());
    copy_from_user_ctx(ctx, ptr.cast::<u8>(), bytes, fault_handler)?;
    Ok(unsafe { value.assume_init() })
}

/// Writes one plain ABI value into user memory after checking access rights.
pub(crate) fn write_user_value<T: Copy>(token: usize, ptr: *mut T, value: &T) -> SysResult<()> {
    write_user_value_with_site(token, ptr, value, None, perf::UsercopySite::WriteValue)
}

pub(crate) fn write_user_value_ctx<T: Copy>(
    ctx: &SyscallContext,
    ptr: *mut T,
    value: &T,
) -> SysResult<()> {
    write_user_value_with_site_ctx(ctx, ptr, value, None, perf::UsercopySite::WriteValue)
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
    write_user_value_with_site(
        token,
        ptr,
        value,
        Some(mmap_user_fault),
        perf::UsercopySite::WriteValue,
    )
}

#[allow(dead_code)]
pub(crate) fn write_user_value_with_mmap_fault_ctx<T: Copy>(
    ctx: &SyscallContext,
    ptr: *mut T,
    value: &T,
) -> SysResult<()> {
    write_user_value_with_site_ctx(
        ctx,
        ptr,
        value,
        Some(mmap_user_fault),
        perf::UsercopySite::WriteValue,
    )
}

pub(crate) fn write_user_value_with_fault<T: Copy>(
    token: usize,
    ptr: *mut T,
    value: &T,
    fault_handler: Option<UserFaultHandler>,
) -> SysResult<()> {
    write_user_value_with_site(
        token,
        ptr,
        value,
        fault_handler,
        perf::UsercopySite::WriteValue,
    )
}

#[allow(dead_code)]
pub(crate) fn write_user_value_with_fault_ctx<T: Copy>(
    ctx: &SyscallContext,
    ptr: *mut T,
    value: &T,
    fault_handler: Option<UserFaultHandler>,
) -> SysResult<()> {
    write_user_value_with_site_ctx(
        ctx,
        ptr,
        value,
        fault_handler,
        perf::UsercopySite::WriteValue,
    )
}

fn write_user_value_with_site<T: Copy>(
    token: usize,
    ptr: *mut T,
    value: &T,
    fault_handler: Option<UserFaultHandler>,
    site: perf::UsercopySite,
) -> SysResult<()> {
    let bytes =
        unsafe { core::slice::from_raw_parts((value as *const T).cast::<u8>(), size_of::<T>()) };
    copy_to_user_with_site(token, ptr.cast::<u8>(), bytes, fault_handler, site)
}

fn write_user_value_with_site_ctx<T: Copy>(
    ctx: &SyscallContext,
    ptr: *mut T,
    value: &T,
    fault_handler: Option<UserFaultHandler>,
    site: perf::UsercopySite,
) -> SysResult<()> {
    let bytes =
        unsafe { core::slice::from_raw_parts((value as *const T).cast::<u8>(), size_of::<T>()) };
    copy_to_user_with_site_ctx(ctx, ptr.cast::<u8>(), bytes, fault_handler, site)
}
