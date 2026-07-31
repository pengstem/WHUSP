use crate::mm::{
    FaultOrigin, FaultRetryReason, FrameTracker, MemorySet, MmapFaultAccess, PageTable, StepByOne,
    TranslatedUserBuffer, UserFaultFatal, UserFaultOutcome, VirtAddr, record_fault_retry,
    record_fault_retry_chain, record_fault_retry_wait, record_fault_retry_yield,
    record_usercopy_fault_retry_terminal, resolve_user_page_fault,
};
use crate::perf;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::{vec, vec::Vec};
use core::mem::{MaybeUninit, size_of};
use core::ops::{Deref, DerefMut};

use super::SyscallContext;
use crate::uapi::errno::{Errno, KResult};

// This limit affects only allocation behavior for small ABI values. The fast
// path still goes through checked_user_pte(), so permissions, COW, and optional
// fault-in semantics must match the multi-page copy path.
const USER_COPY_SAME_PAGE_FAST_MAX: usize = 64;
const DIRECT_PATH_COMPONENT_MAX: usize = 255;

pub(crate) struct DirectPathComponent {
    bytes: [u8; DIRECT_PATH_COMPONENT_MAX],
    len: usize,
}

impl DirectPathComponent {
    pub(crate) fn as_str(&self) -> &str {
        // The direct reader rejects every non-ASCII byte before publishing
        // this value, so the populated prefix is valid UTF-8.
        unsafe { core::str::from_utf8_unchecked(&self.bytes[..self.len]) }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum UserBufferAccess {
    Read,
    Write,
}

/// Optional page-fault hook used while validating a user byte range.
///
/// Callers pass this only when the syscall is allowed to materialize lazy user
/// mappings before copying. Fatal outcomes are reported as `EFAULT`; retryable
/// outcomes retain their reason so the current task can wait outside MM locks.
pub(crate) type UserFaultHandler = fn(usize, UserBufferAccess) -> UserFaultOutcome;

#[derive(Clone)]
enum EffectiveUserFault {
    None,
    Function(UserFaultHandler),
    CurrentLazyFramed(Arc<crate::task::ProcessControlBlock>),
}

#[derive(Clone)]
struct UserFaultResolver {
    fault: EffectiveUserFault,
    current_process: Option<Arc<crate::task::ProcessControlBlock>>,
}

impl UserFaultResolver {
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

    fn from_current_lazy_framed(process: Arc<crate::task::ProcessControlBlock>) -> Self {
        Self {
            fault: EffectiveUserFault::CurrentLazyFramed(Arc::clone(&process)),
            current_process: Some(process),
        }
    }

    fn with_current_process(
        fault_handler: UserFaultHandler,
        process: Arc<crate::task::ProcessControlBlock>,
    ) -> Self {
        Self {
            fault: EffectiveUserFault::Function(fault_handler),
            current_process: Some(process),
        }
    }

    fn can_fault(&self) -> bool {
        !matches!(self.fault, EffectiveUserFault::None)
    }

    fn resolve(&self, addr: usize, access: UserBufferAccess) -> UserFaultOutcome {
        match self.fault {
            EffectiveUserFault::None => UserFaultOutcome::Fatal(UserFaultFatal::Segv),
            EffectiveUserFault::Function(handler) => handler(addr, access),
            EffectiveUserFault::CurrentLazyFramed(ref process) => {
                if lazy_framed_user_fault_for_process(process.as_ref(), addr, access) {
                    UserFaultOutcome::Resolved
                } else {
                    UserFaultOutcome::Fatal(UserFaultFatal::Segv)
                }
            }
        }
    }

    fn resolve_cow(&self, token: usize, addr: usize) -> bool {
        if let Some(process) = &self.current_process {
            return process
                .inner_exclusive_access()
                .memory_set
                .resolve_cow_page_fault(addr);
        }
        resolve_current_cow_page(token, addr)
    }
}

#[derive(Default)]
struct UsercopyFaultRetryState {
    last_reason: Option<FaultRetryReason>,
    consecutive: usize,
    had_retry: bool,
}

impl UsercopyFaultRetryState {
    fn wait_or_reschedule(
        &mut self,
        addr: usize,
        access: UserBufferAccess,
        retry: crate::mm::FaultRetry,
    ) {
        let mmap_access = match access {
            UserBufferAccess::Read => MmapFaultAccess::Read,
            UserBufferAccess::Write => MmapFaultAccess::Write,
        };
        record_fault_retry(FaultOrigin::Usercopy, addr, mmap_access, &retry);
        let reason = retry.reason();
        self.had_retry = true;
        if self.last_reason == Some(reason) {
            self.consecutive = self.consecutive.saturating_add(1);
        } else {
            self.last_reason = Some(reason);
            self.consecutive = 1;
        }
        record_fault_retry_chain(FaultOrigin::Usercopy, self.consecutive);
        if let Some(waited_us) = retry.wait() {
            record_fault_retry_wait(FaultOrigin::Usercopy, waited_us);
            return;
        }
        // Generation waits carry a completion gate. Other retry reasons mean
        // that VMA or install state changed while MM locks were dropped: rewalk
        // once immediately, then yield only if the same race repeats.
        if self.consecutive > 1 {
            record_fault_retry_yield(FaultOrigin::Usercopy);
            crate::task::suspend_current_and_run_next();
        }
    }

    fn record_terminal(&self, resolved: bool) {
        record_usercopy_fault_retry_terminal(self.had_retry, resolved);
    }
}

fn mmap_user_fault(addr: usize, access: UserBufferAccess) -> UserFaultOutcome {
    let access = match access {
        UserBufferAccess::Read => MmapFaultAccess::Read,
        UserBufferAccess::Write => MmapFaultAccess::Write,
    };
    resolve_user_page_fault(&crate::task::current_process(), addr, access)
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
) -> UserFaultResolver {
    let Some(task) = crate::task::current_task() else {
        return UserFaultResolver::none();
    };
    let Some(process) = task.process.upgrade() else {
        return UserFaultResolver::none();
    };
    // Default lazy-framed faults are safe only for the current process token.
    // Child or foreign address spaces must use explicit MemorySet copy helpers
    // so user-stack setup and ptrace writes do not fault the wrong process.
    let is_current_token = if process.inner_owned_by_current() {
        // A caller already holding this PCB lock owns mapping stability. Do
        // not recurse; leave `current_process` empty so the checked walk uses
        // that existing serialization and still retains the frame.
        token == crate::task::current_user_token()
    } else {
        let inner = process.inner_exclusive_access();
        let matches = inner.memory_set.token() == token;
        drop(inner);
        matches
    };
    if !is_current_token {
        return fault_handler
            .map_or_else(UserFaultResolver::none, UserFaultResolver::from_function);
    }
    if process.inner_owned_by_current() {
        return fault_handler
            .map_or_else(UserFaultResolver::none, UserFaultResolver::from_function);
    }
    if let Some(fault_handler) = fault_handler {
        UserFaultResolver::with_current_process(fault_handler, process)
    } else {
        UserFaultResolver::from_current_lazy_framed(process)
    }
}

fn effective_user_fault_resolver_for_ctx(
    ctx: &SyscallContext,
    token: usize,
    fault_handler: Option<UserFaultHandler>,
) -> UserFaultResolver {
    // A SyscallContext pins the entry address-space token. Only attach the
    // current process to the resolver when the requested token is that same
    // token; foreign MemorySet copies must not fault the running process.
    let current_process = (token == ctx.user_token() && !ctx.process().inner_owned_by_current())
        .then(|| Arc::clone(ctx.process()));
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
) -> KResult<TranslatedUserBuffer> {
    translated_byte_buffer_checked_with_fault(token, ptr, len, access, None)
}

pub(crate) fn translated_byte_buffer_checked_ctx(
    ctx: &SyscallContext,
    ptr: *const u8,
    len: usize,
    access: UserBufferAccess,
) -> KResult<TranslatedUserBuffer> {
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
) -> KResult<TranslatedUserBuffer> {
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
) -> KResult<TranslatedUserBuffer> {
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
) -> KResult<TranslatedUserBuffer> {
    let fault_handler = effective_user_fault_resolver(token, fault_handler);
    translated_byte_buffer_checked_with_resolver(token, ptr, len, access, fault_handler)
}

fn translated_byte_buffer_checked_with_fault_ctx(
    ctx: &SyscallContext,
    ptr: *const u8,
    len: usize,
    access: UserBufferAccess,
    fault_handler: Option<UserFaultHandler>,
) -> KResult<TranslatedUserBuffer> {
    let token = ctx.user_token();
    let fault_handler = effective_user_fault_resolver_for_ctx(ctx, token, fault_handler);
    translated_byte_buffer_checked_with_resolver(token, ptr, len, access, fault_handler)
}

fn translated_byte_buffer_checked_with_resolver(
    token: usize,
    ptr: *const u8,
    len: usize,
    access: UserBufferAccess,
    fault_handler: UserFaultResolver,
) -> KResult<TranslatedUserBuffer> {
    if len == 0 {
        return Ok(TranslatedUserBuffer::empty());
    }
    // CONTEXT: brk growth is VMA-reserved and materialized lazily. Default
    // current-process syscall copies should fault those framed pages in, while
    // full mmap fault handling remains opt-in through the explicit mmap helper.
    let mut start = ptr as usize;
    let end = start.checked_add(len).ok_or(Errno::EFAULT)?;
    let start_va = VirtAddr::from(start);
    if start_va.floor() == VirtAddr::from(end - 1).floor() {
        let (pte, pin) = checked_and_pin_user_pte(token, start, access, &fault_handler)?;
        let offset = start_va.page_offset();
        let buffers = vec![&mut pte.ppn().get_bytes_array()[offset..offset + len]];
        perf::record_usercopy_checked_range(1, len);
        return Ok(TranslatedUserBuffer::new(buffers, vec![pin]));
    }
    let mut buffers = Vec::new();
    let mut pins = Vec::new();
    while start < end {
        let start_va = VirtAddr::from(start);
        let mut vpn = start_va.floor();
        let (pte, pin) = checked_and_pin_user_pte(token, start, access, &fault_handler)?;
        let ppn = pte.ppn();
        pins.push(pin);
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
    Ok(TranslatedUserBuffer::new(buffers, pins))
}

fn checked_and_pin_user_pte(
    token: usize,
    addr: usize,
    access: UserBufferAccess,
    fault_handler: &UserFaultResolver,
) -> KResult<(crate::mm::PageTableEntry, FrameTracker)> {
    if let Some(process) = &fault_handler.current_process {
        let mut retry_state = UsercopyFaultRetryState::default();
        loop {
            // Linux does not take a process-wide write lock for resident
            // copy_to/from_user. Traverse the active page table under the
            // address-space read side and retain the physical frame before
            // releasing it; munmap/COW writers cannot recycle that frame in
            // the translate -> retain window.
            let (ready, pte) = process.with_memory_read(|| {
                let page_table = PageTable::from_token(token);
                let pte = page_table.translate(VirtAddr::from(addr).floor());
                let ready = pte
                    .filter(|pte| user_pte_allows(*pte, access, fault_handler.can_fault()))
                    .map(|pte| {
                        FrameTracker::from_retained(pte.ppn())
                            .map(|pin| (pte, pin))
                            .ok_or(Errno::EFAULT)
                    })
                    .transpose()?;
                Ok::<_, Errno>((ready, pte))
            })?;
            if let Some(ready) = ready {
                retry_state.record_terminal(true);
                return Ok(ready);
            }
            let cow = pte.is_some_and(|pte| {
                access == UserBufferAccess::Write && pte.cow() && !pte.writable()
            });
            let outcome = if cow {
                if fault_handler.resolve_cow(token, addr) {
                    UserFaultOutcome::Resolved
                } else {
                    UserFaultOutcome::Fatal(UserFaultFatal::Segv)
                }
            } else if fault_handler.can_fault() {
                fault_handler.resolve(addr, access)
            } else {
                UserFaultOutcome::Fatal(UserFaultFatal::Segv)
            };
            match outcome {
                UserFaultOutcome::Resolved => {}
                UserFaultOutcome::Retry(retry) => {
                    retry_state.wait_or_reschedule(addr, access, retry);
                }
                UserFaultOutcome::Fatal(_) => {
                    retry_state.record_terminal(false);
                    return Err(Errno::EFAULT);
                }
            }
        }
    }

    // Foreign or freshly constructed address spaces are translated only by
    // callers that already own their MemorySet. Retain immediately so the
    // returned carrier still has an explicit physical-page lifetime.
    let page_table = PageTable::from_token(token);
    let pte = checked_user_pte(&page_table, token, addr, access, fault_handler)?;
    let pin = FrameTracker::from_retained(pte.ppn()).ok_or(Errno::EFAULT)?;
    Ok((pte, pin))
}

fn with_user_page_ctx<V>(
    ctx: &SyscallContext,
    addr: usize,
    access: UserBufferAccess,
    fault_handler: Option<UserFaultHandler>,
    mut access_page: impl FnMut(&mut [u8]) -> V,
) -> KResult<V> {
    let token = ctx.user_token();
    let process = ctx.process();
    if process.inner_owned_by_current() {
        let resolver =
            fault_handler.map_or_else(UserFaultResolver::none, UserFaultResolver::from_function);
        let page_table = PageTable::from_token(token);
        let pte = checked_user_pte(&page_table, token, addr, access, &resolver)?;
        return Ok(access_page(pte.ppn().get_bytes_array()));
    }

    let mut retry_state = UsercopyFaultRetryState::default();
    loop {
        let (value, pte) = process.with_memory_read(|| {
            let page_table = PageTable::from_token(token);
            let pte = page_table.translate(VirtAddr::from(addr).floor());
            let value = pte
                .filter(|pte| user_pte_allows(*pte, access, true))
                .map(|pte| access_page(pte.ppn().get_bytes_array()));
            (value, pte)
        });
        if let Some(value) = value {
            retry_state.record_terminal(true);
            return Ok(value);
        }
        let cow = pte
            .is_some_and(|pte| access == UserBufferAccess::Write && pte.cow() && !pte.writable());
        let outcome = if cow {
            if process
                .inner_exclusive_access()
                .memory_set
                .resolve_cow_page_fault(addr)
            {
                UserFaultOutcome::Resolved
            } else {
                UserFaultOutcome::Fatal(UserFaultFatal::Segv)
            }
        } else if let Some(handler) = fault_handler {
            handler(addr, access)
        } else if lazy_framed_user_fault_for_process(process, addr, access) {
            UserFaultOutcome::Resolved
        } else {
            UserFaultOutcome::Fatal(UserFaultFatal::Segv)
        };
        match outcome {
            UserFaultOutcome::Resolved => {}
            UserFaultOutcome::Retry(retry) => {
                retry_state.wait_or_reschedule(addr, access, retry);
            }
            UserFaultOutcome::Fatal(_) => {
                retry_state.record_terminal(false);
                return Err(Errno::EFAULT);
            }
        }
    }
}

fn checked_user_pte(
    page_table: &PageTable,
    token: usize,
    addr: usize,
    access: UserBufferAccess,
    fault_handler: &UserFaultResolver,
) -> KResult<crate::mm::PageTableEntry> {
    // Passing a fault handler means the copy is allowed to mutate the current
    // process mappings by resolving lazy mmap/COW faults. Cross-address-space
    // copies should use the explicit MemorySet helpers instead.
    let vpn = VirtAddr::from(addr).floor();
    let translate = |page_table: &PageTable| page_table.translate(vpn);
    let mut pte = match translate(page_table) {
        Some(pte) => pte,
        None => {
            if !fault_handler.can_fault() {
                return Err(Errno::EFAULT);
            }
            if !matches!(
                fault_handler.resolve(addr, access),
                UserFaultOutcome::Resolved
            ) {
                return Err(Errno::EFAULT);
            }
            translate(page_table).ok_or(Errno::EFAULT)?
        }
    };
    let reject_zero_ppn = fault_handler.can_fault();
    if !user_pte_allows(pte, access, reject_zero_ppn) {
        if access == UserBufferAccess::Write && pte.cow() && !pte.writable() {
            // COW resolution precedes the generic mmap hook so fork-private
            // pages become writable instead of being reported as EFAULT.
            if !fault_handler.resolve_cow(token, addr) {
                return Err(Errno::EFAULT);
            }
            pte = translate(page_table).ok_or(Errno::EFAULT)?;
        } else if fault_handler.can_fault() {
            if !matches!(
                fault_handler.resolve(addr, access),
                UserFaultOutcome::Resolved
            ) {
                return Err(Errno::EFAULT);
            }
            pte = translate(page_table).ok_or(Errno::EFAULT)?;
        }
        if !user_pte_allows(pte, access, reject_zero_ppn) {
            return Err(Errno::EFAULT);
        }
    }
    Ok(pte)
}

fn try_same_page_user_slice(
    token: usize,
    ptr: *const u8,
    len: usize,
    access: UserBufferAccess,
    fault_handler: &UserFaultResolver,
) -> Option<KResult<PinnedUserSlice>> {
    // This is only an allocation-saving fast path for short ABI scalars. It
    // still goes through checked_user_pte(), so permission, COW, and optional
    // mmap-fault behavior match the multi-page copy path.
    if len == 0 || len > USER_COPY_SAME_PAGE_FAST_MAX {
        return None;
    }
    let start = ptr as usize;
    let end = match start.checked_add(len) {
        Some(end) => end,
        None => return Some(Err(Errno::EFAULT)),
    };
    let start_va = VirtAddr::from(start);
    if start_va.floor() != VirtAddr::from(end - 1).floor() {
        return None;
    }
    let (pte, pin) = match checked_and_pin_user_pte(token, start, access, fault_handler) {
        Ok(result) => result,
        Err(err) => return Some(Err(err)),
    };
    let offset = start_va.page_offset();
    Some(Ok(PinnedUserSlice {
        buffer: &mut pte.ppn().get_bytes_array()[offset..offset + len],
        _pin: pin,
    }))
}

struct PinnedUserSlice {
    buffer: &'static mut [u8],
    _pin: FrameTracker,
}

impl Deref for PinnedUserSlice {
    type Target = [u8];

    fn deref(&self) -> &Self::Target {
        self.buffer
    }
}

impl DerefMut for PinnedUserSlice {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.buffer
    }
}

fn try_copy_from_user_same_page_ctx(
    ctx: &SyscallContext,
    ptr: *const u8,
    dst: &mut [u8],
    fault_handler: Option<UserFaultHandler>,
) -> Option<KResult<()>> {
    if dst.is_empty() {
        return None;
    }
    let start = ptr as usize;
    let end = match start.checked_add(dst.len()) {
        Some(end) => end,
        None => return Some(Err(Errno::EFAULT)),
    };
    let start_va = VirtAddr::from(start);
    if start_va.floor() != VirtAddr::from(end - 1).floor() {
        return None;
    }
    let offset = start_va.page_offset();
    Some(with_user_page_ctx(
        ctx,
        start,
        UserBufferAccess::Read,
        fault_handler,
        |page| dst.copy_from_slice(&page[offset..offset + dst.len()]),
    ))
}

fn try_copy_to_user_same_page_ctx(
    ctx: &SyscallContext,
    ptr: *mut u8,
    src: &[u8],
    fault_handler: Option<UserFaultHandler>,
) -> Option<KResult<()>> {
    if src.is_empty() {
        return None;
    }
    let start = ptr as usize;
    let end = match start.checked_add(src.len()) {
        Some(end) => end,
        None => return Some(Err(Errno::EFAULT)),
    };
    let start_va = VirtAddr::from(start);
    if start_va.floor() != VirtAddr::from(end - 1).floor() {
        return None;
    }
    let offset = start_va.page_offset();
    Some(with_user_page_ctx(
        ctx,
        start,
        UserBufferAccess::Write,
        fault_handler,
        |page| page[offset..offset + src.len()].copy_from_slice(src),
    ))
}

fn resolve_current_cow_page(token: usize, addr: usize) -> bool {
    // CONTEXT: COW fault resolution may take the current process memory lock
    // and update its page table. Cross-process writers such as ptrace must use
    // memory-set aware copy helpers instead of this current-token fast path.
    if token != crate::task::current_user_token() {
        return false;
    }
    let process = crate::task::current_process();
    if process.inner_owned_by_current() {
        return false;
    }
    process
        .inner_exclusive_access()
        .memory_set
        .resolve_cow_page_fault(addr)
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

/// Tries the allocation-free pathname subset used by dirfd metadata probes.
/// A non-component shape returns `None` and lets the complete pathname reader
/// preserve byte conversion, long-name, and multi-component semantics.
pub(crate) fn try_read_direct_path_component_ctx(
    ctx: &SyscallContext,
    ptr: *const u8,
) -> KResult<Option<DirectPathComponent>> {
    if ptr.is_null() {
        return Err(Errno::EFAULT);
    }
    let mut component = DirectPathComponent {
        bytes: [0; DIRECT_PATH_COMPONENT_MAX],
        len: 0,
    };
    perf::record_user_c_string_call();
    enum PageScan {
        Complete,
        Continue,
        Fallback,
    }
    loop {
        let addr = (ptr as usize)
            .checked_add(component.len)
            .ok_or(Errno::EFAULT)?;
        let start = VirtAddr::from(addr).page_offset();
        let scan = with_user_page_ctx(
            ctx,
            addr,
            UserBufferAccess::Read,
            Some(mmap_user_fault),
            |page| {
                for &byte in &page[start..] {
                    if byte == 0 {
                        return PageScan::Complete;
                    }
                    if byte == b'/'
                        || !byte.is_ascii()
                        || component.len == DIRECT_PATH_COMPONENT_MAX
                    {
                        return PageScan::Fallback;
                    }
                    component.bytes[component.len] = byte;
                    component.len += 1;
                }
                PageScan::Continue
            },
        )?;
        match scan {
            PageScan::Complete => {
                perf::record_user_c_string_chunk(component.len + 1, component.len, true);
                if component.len == 0 || component.as_str() == "." || component.as_str() == ".." {
                    return Ok(None);
                }
                return Ok(Some(component));
            }
            PageScan::Continue => {}
            PageScan::Fallback => return Ok(None),
        }
    }
}

/// Reads a NUL-terminated string from user memory with an explicit length cap.
///
/// Returns `EFAULT` for invalid user memory and `ENAMETOOLONG` when no NUL byte
/// is found within `max_len`, matching Linux pathname-style ABI boundaries.
pub(crate) fn read_user_c_string(token: usize, ptr: *const u8, max_len: usize) -> KResult<String> {
    if ptr.is_null() {
        return Err(Errno::EFAULT);
    }

    let mut string = String::with_capacity(64);
    let mut offset = 0usize;
    perf::record_user_c_string_call();
    while offset < max_len {
        let addr = (ptr as usize).checked_add(offset).ok_or(Errno::EFAULT)?;
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
    Err(Errno::ENAMETOOLONG)
}

pub(crate) fn read_user_c_string_ctx(
    ctx: &SyscallContext,
    ptr: *const u8,
    max_len: usize,
) -> KResult<String> {
    if ptr.is_null() {
        return Err(Errno::EFAULT);
    }

    let mut string = String::with_capacity(64);
    let mut offset = 0usize;
    perf::record_user_c_string_call();
    while offset < max_len {
        let addr = (ptr as usize).checked_add(offset).ok_or(Errno::EFAULT)?;
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
    Err(Errno::ENAMETOOLONG)
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

pub(crate) fn read_user_usize_ctx(ctx: &SyscallContext, addr: usize) -> KResult<usize> {
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
) -> KResult<T> {
    read_user_value_with_site(
        token,
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
) -> KResult<Vec<T>> {
    if count == 0 {
        return Ok(Vec::new());
    }
    if ptr.is_null() {
        return Err(Errno::EFAULT);
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
) -> KResult<Vec<T>> {
    if count == 0 {
        return Ok(Vec::new());
    }
    if ptr.is_null() {
        return Err(Errno::EFAULT);
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
pub(crate) fn write_user_array<T: Copy>(token: usize, ptr: *mut T, values: &[T]) -> KResult<()> {
    if values.is_empty() {
        return Ok(());
    }
    if ptr.is_null() {
        return Err(Errno::EFAULT);
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

fn user_array_item_addr<T>(ptr: *const T, index: usize) -> KResult<usize> {
    (ptr as usize)
        .checked_add(user_array_byte_len::<T>(index)?)
        .ok_or(Errno::EFAULT)
}

fn user_array_byte_len<T>(count: usize) -> KResult<usize> {
    count.checked_mul(size_of::<T>()).ok_or(Errno::EFAULT)
}

fn copy_from_user(
    token: usize,
    ptr: *const u8,
    dst: &mut [u8],
    fault_handler: Option<UserFaultHandler>,
) -> KResult<()> {
    let fault_handler = effective_user_fault_resolver(token, fault_handler);
    copy_from_user_with_resolver(token, ptr, dst, fault_handler)
}

fn copy_from_user_ctx(
    ctx: &SyscallContext,
    ptr: *const u8,
    dst: &mut [u8],
    fault_handler: Option<UserFaultHandler>,
) -> KResult<()> {
    let token = ctx.user_token();
    if let Some(result) = try_copy_from_user_same_page_ctx(ctx, ptr, dst, fault_handler) {
        result?;
        perf::record_usercopy_same_page_fast(perf::UsercopyAccess::Read, dst.len());
        return Ok(());
    }
    let fault_handler = effective_user_fault_resolver_for_ctx(ctx, token, fault_handler);
    copy_from_user_with_resolver(token, ptr, dst, fault_handler)
}

fn copy_from_user_with_resolver(
    token: usize,
    ptr: *const u8,
    dst: &mut [u8],
    fault_handler: UserFaultResolver,
) -> KResult<()> {
    if dst.is_empty() {
        return Ok(());
    }
    if let Some(buffer) = try_same_page_user_slice(
        token,
        ptr,
        dst.len(),
        UserBufferAccess::Read,
        &fault_handler,
    ) {
        let buffer = buffer?;
        dst.copy_from_slice(&buffer);
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

fn copy_to_user_buffers(buffers: TranslatedUserBuffer, src: &[u8]) {
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
) -> KResult<()> {
    if len == 0 {
        return Ok(());
    }
    let mut start = ptr as usize;
    let end = start.checked_add(len).ok_or(Errno::EFAULT)?;
    while start < end {
        let start_va = VirtAddr::from(start);
        let vpn = start_va.floor();
        let pte = memory_set.translate(vpn).ok_or(Errno::EFAULT)?;
        if pte.cow() && !pte.writable() && !memory_set.resolve_cow_page_fault(start) {
            return Err(Errno::EFAULT);
        }
        let pte = memory_set.translate(vpn).ok_or(Errno::EFAULT)?;
        if !user_pte_allows(pte, UserBufferAccess::Write, false) {
            return Err(Errno::EFAULT);
        }
        let mut next_vpn = vpn;
        next_vpn.step();
        let next_va: VirtAddr = next_vpn.into();
        start = usize::from(next_va).min(end);
    }
    Ok(())
}

/// Copies kernel bytes into a user buffer after validating write permission.
pub(crate) fn copy_to_user(token: usize, ptr: *mut u8, src: &[u8]) -> KResult<()> {
    copy_to_user_with_site(token, ptr, src, None, perf::UsercopySite::CopyToUser)
}

pub(crate) fn copy_to_user_ctx(ctx: &SyscallContext, ptr: *mut u8, src: &[u8]) -> KResult<()> {
    copy_to_user_with_site_ctx(ctx, ptr, src, None, perf::UsercopySite::CopyToUser)
}

/// Copies kernel bytes into a user buffer, resolving valid mmap-backed pages.
///
/// Syscalls whose output buffer may point at a lazily populated mapping must
/// use this variant. The normal context helper intentionally only resolves the
/// anonymous framed pages used by stacks and similar process-owned regions.
pub(crate) fn copy_to_user_with_mmap_fault_ctx(
    ctx: &SyscallContext,
    ptr: *mut u8,
    src: &[u8],
) -> KResult<()> {
    copy_to_user_with_site_ctx(
        ctx,
        ptr,
        src,
        Some(mmap_user_fault),
        perf::UsercopySite::CopyToUser,
    )
}

fn copy_to_user_with_site(
    token: usize,
    ptr: *mut u8,
    src: &[u8],
    fault_handler: Option<UserFaultHandler>,
    site: perf::UsercopySite,
) -> KResult<()> {
    let fault_handler = effective_user_fault_resolver(token, fault_handler);
    copy_to_user_with_resolver(token, ptr, src, fault_handler, site)
}

fn copy_to_user_with_site_ctx(
    ctx: &SyscallContext,
    ptr: *mut u8,
    src: &[u8],
    fault_handler: Option<UserFaultHandler>,
    site: perf::UsercopySite,
) -> KResult<()> {
    let token = ctx.user_token();
    perf::record_usercopy_site(site, src.len());
    if src.is_empty() {
        return Ok(());
    }
    if let Some(result) = try_copy_to_user_same_page_ctx(ctx, ptr, src, fault_handler) {
        result?;
        perf::record_usercopy_same_page_fast(perf::UsercopyAccess::Write, src.len());
        return Ok(());
    }
    let fault_handler = effective_user_fault_resolver_for_ctx(ctx, token, fault_handler);
    copy_to_user_with_resolver_impl(token, ptr, src, fault_handler, site, false)
}

fn copy_to_user_with_resolver(
    token: usize,
    ptr: *mut u8,
    src: &[u8],
    fault_handler: UserFaultResolver,
    site: perf::UsercopySite,
) -> KResult<()> {
    copy_to_user_with_resolver_impl(token, ptr, src, fault_handler, site, true)
}

fn copy_to_user_with_resolver_impl(
    token: usize,
    ptr: *mut u8,
    src: &[u8],
    fault_handler: UserFaultResolver,
    site: perf::UsercopySite,
    record_site: bool,
) -> KResult<()> {
    if record_site {
        perf::record_usercopy_site(site, src.len());
    }
    if src.is_empty() {
        return Ok(());
    }
    if let Some(buffer) = try_same_page_user_slice(
        token,
        ptr.cast_const(),
        src.len(),
        UserBufferAccess::Write,
        &fault_handler,
    ) {
        let mut buffer = buffer?;
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
) -> KResult<()> {
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
pub(crate) fn read_user_value<T: Copy>(token: usize, ptr: *const T) -> KResult<T> {
    read_user_value_with_site(token, ptr, None, perf::UsercopySite::ReadValue)
}

pub(crate) fn read_user_value_ctx<T: Copy>(ctx: &SyscallContext, ptr: *const T) -> KResult<T> {
    read_user_value_with_site_ctx(ctx, ptr, None, perf::UsercopySite::ReadValue)
}

pub(crate) fn read_user_value_with_mmap_fault<T: Copy>(token: usize, ptr: *const T) -> KResult<T> {
    read_user_value_with_site(
        token,
        ptr,
        Some(mmap_user_fault),
        perf::UsercopySite::ReadValue,
    )
}

pub(crate) fn read_user_value_with_mmap_fault_ctx<T: Copy>(
    ctx: &SyscallContext,
    ptr: *const T,
) -> KResult<T> {
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
) -> KResult<T> {
    read_user_value_with_site(token, ptr, fault_handler, perf::UsercopySite::ReadValue)
}

fn read_user_value_with_site<T: Copy>(
    token: usize,
    ptr: *const T,
    fault_handler: Option<UserFaultHandler>,
    site: perf::UsercopySite,
) -> KResult<T> {
    let mut value = MaybeUninit::<T>::uninit();
    let bytes =
        unsafe { core::slice::from_raw_parts_mut(value.as_mut_ptr().cast::<u8>(), size_of::<T>()) };
    perf::record_usercopy_site(site, bytes.len());
    copy_from_user(token, ptr.cast::<u8>(), bytes, fault_handler)?;
    Ok(unsafe { value.assume_init() })
}

fn read_user_value_with_site_ctx<T: Copy>(
    ctx: &SyscallContext,
    ptr: *const T,
    fault_handler: Option<UserFaultHandler>,
    site: perf::UsercopySite,
) -> KResult<T> {
    let mut value = MaybeUninit::<T>::uninit();
    let bytes =
        unsafe { core::slice::from_raw_parts_mut(value.as_mut_ptr().cast::<u8>(), size_of::<T>()) };
    perf::record_usercopy_site(site, bytes.len());
    copy_from_user_ctx(ctx, ptr.cast::<u8>(), bytes, fault_handler)?;
    Ok(unsafe { value.assume_init() })
}

/// Writes one plain ABI value into user memory after checking access rights.
pub(crate) fn write_user_value<T: Copy>(token: usize, ptr: *mut T, value: &T) -> KResult<()> {
    write_user_value_with_site(token, ptr, value, None, perf::UsercopySite::WriteValue)
}

pub(crate) fn write_user_value_ctx<T: Copy>(
    ctx: &SyscallContext,
    ptr: *mut T,
    value: &T,
) -> KResult<()> {
    write_user_value_with_site_ctx(ctx, ptr, value, None, perf::UsercopySite::WriteValue)
}

pub(crate) fn write_user_value_in_memory_set<T: Copy>(
    memory_set: &mut MemorySet,
    ptr: *mut T,
    value: &T,
) -> KResult<()> {
    let bytes =
        unsafe { core::slice::from_raw_parts((value as *const T).cast::<u8>(), size_of::<T>()) };
    copy_to_user_in_memory_set(memory_set, ptr.cast::<u8>(), bytes)
}

pub(crate) fn write_user_value_with_mmap_fault<T: Copy>(
    token: usize,
    ptr: *mut T,
    value: &T,
) -> KResult<()> {
    write_user_value_with_site(
        token,
        ptr,
        value,
        Some(mmap_user_fault),
        perf::UsercopySite::WriteValue,
    )
}

pub(crate) fn write_user_value_with_mmap_fault_ctx<T: Copy>(
    ctx: &SyscallContext,
    ptr: *mut T,
    value: &T,
) -> KResult<()> {
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
) -> KResult<()> {
    write_user_value_with_site(
        token,
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
) -> KResult<()> {
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
) -> KResult<()> {
    let bytes =
        unsafe { core::slice::from_raw_parts((value as *const T).cast::<u8>(), size_of::<T>()) };
    copy_to_user_with_site_ctx(ctx, ptr.cast::<u8>(), bytes, fault_handler, site)
}
