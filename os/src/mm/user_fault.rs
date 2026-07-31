use super::page_cache::PageCacheGenerationGate;
use super::{
    MmapFaultAccess, MmapFaultResult, MmapPageCacheInstall, MmapPageCacheResolve, MmapPageInstall,
    PageTable, VirtAddr,
};
use crate::task::ProcessControlBlock;
use alloc::sync::Arc;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum FaultOrigin {
    Hardware,
    Usercopy,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum FaultRetryReason {
    GenerationUnstable,
    GenerationChanged,
    StaleInstall,
    VmaChanged,
    DuplicateFault,
    InstallRace,
}

pub(crate) struct FaultRetry {
    reason: FaultRetryReason,
    observed_seq: Option<usize>,
    waiter: Option<Arc<PageCacheGenerationGate>>,
}

impl FaultRetry {
    pub(crate) fn immediate(reason: FaultRetryReason) -> Self {
        Self {
            reason,
            observed_seq: None,
            waiter: None,
        }
    }

    pub(crate) fn generation_wait(
        observed_seq: usize,
        waiter: Arc<PageCacheGenerationGate>,
    ) -> Self {
        Self {
            reason: FaultRetryReason::GenerationUnstable,
            observed_seq: Some(observed_seq),
            waiter: Some(waiter),
        }
    }

    pub(crate) fn reason(&self) -> FaultRetryReason {
        self.reason
    }

    pub(crate) fn observed_seq(&self) -> Option<usize> {
        self.observed_seq
    }

    pub(crate) fn wait(self) -> Option<usize> {
        let Some(waiter) = self.waiter else {
            return None;
        };
        let started_us = crate::timer::get_time_us();
        waiter.wait();
        Some(crate::timer::get_time_us().saturating_sub(started_us))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum UserFaultFatal {
    Segv,
    ForcedDefaultSegv,
    Bus,
}

pub(crate) enum UserFaultOutcome {
    Resolved,
    Retry(FaultRetry),
    Fatal(UserFaultFatal),
}

pub(crate) fn record_fault_retry(
    origin: FaultOrigin,
    addr: usize,
    access: MmapFaultAccess,
    retry: &FaultRetry,
) {
    perf::record_retry(origin, retry.reason());
    let observed_seq = retry.observed_seq();
    #[cfg(feature = "fault-trace")]
    {
        let pid = crate::task::current_process().getpid();
        let tid = crate::task::current_task()
            .map(|task| task.linux_tid())
            .unwrap_or(0);
        println!(
            "FAULT_RETRY origin={:?} reason={:?} pid={} tid={} vpn={:#x} observed_seq={:?} access={:?}",
            origin,
            retry.reason(),
            pid,
            tid,
            VirtAddr::from(addr).floor().0,
            observed_seq,
            access,
        );
    }
    #[cfg(not(feature = "fault-trace"))]
    {
        let _ = (addr, access, observed_seq);
    }
}

pub(crate) fn record_fault_retry_wait(origin: FaultOrigin, waited_us: usize) {
    perf::record_wait(origin, waited_us);
}

pub(crate) fn record_fault_retry_yield(origin: FaultOrigin) {
    perf::record_yield(origin);
}

pub(crate) fn record_fault_retry_chain(origin: FaultOrigin, consecutive: usize) {
    perf::record_chain(origin, consecutive);
}

pub(crate) fn record_usercopy_fault_retry_terminal(had_retry: bool, resolved: bool) {
    if had_retry {
        perf::record_usercopy_terminal(resolved);
    }
}

#[cfg(feature = "perf-counters")]
pub(crate) fn append_fault_perf_stats(content: &mut alloc::string::String) {
    perf::append_stats(content);
}

fn access_is_resolved(process: &ProcessControlBlock, addr: usize, access: MmapFaultAccess) -> bool {
    let token = crate::task::current_user_token();
    process.with_memory_read(|| {
        let pte = PageTable::from_token(token).translate(VirtAddr::from(addr).floor());
        pte.is_some_and(|pte| {
            pte.is_valid()
                && pte.ppn().0 != 0
                && match access {
                    MmapFaultAccess::Read => pte.readable(),
                    MmapFaultAccess::Write => pte.writable(),
                    MmapFaultAccess::Execute => pte.executable(),
                }
        })
    })
}

fn resolved_or_retry(
    process: &ProcessControlBlock,
    addr: usize,
    access: MmapFaultAccess,
    retry_reason: FaultRetryReason,
) -> UserFaultOutcome {
    if access_is_resolved(process, addr, access) {
        UserFaultOutcome::Resolved
    } else {
        UserFaultOutcome::Retry(FaultRetry::immediate(retry_reason))
    }
}

fn resolve_prepared_mmap_fault(
    process: &ProcessControlBlock,
    addr: usize,
    access: MmapFaultAccess,
    fault: MmapFaultResult,
) -> UserFaultOutcome {
    match fault {
        MmapFaultResult::Resolved => {
            resolved_or_retry(process, addr, access, FaultRetryReason::DuplicateFault)
        }
        MmapFaultResult::Retry(retry) => UserFaultOutcome::Retry(retry),
        MmapFaultResult::FatalSigsegv => UserFaultOutcome::Fatal(UserFaultFatal::ForcedDefaultSegv),
        MmapFaultResult::FatalSigbus => UserFaultOutcome::Fatal(UserFaultFatal::Bus),
        MmapFaultResult::Page(page) => {
            let frame = {
                let _build_scope =
                    crate::perf::time_scope(crate::perf::ProfilePoint::PageFaultMmapBuildFrame);
                page.build_frame()
            };
            let Some(frame) = frame else {
                return UserFaultOutcome::Fatal(UserFaultFatal::Segv);
            };
            let _install_scope =
                crate::perf::time_scope(crate::perf::ProfilePoint::PageFaultMmapInstallFrame);
            let install = process
                .inner_exclusive_access()
                .memory_set
                .install_mmap_fault_page(page, frame);
            match install {
                MmapPageInstall::InstalledOrDuplicate => {
                    resolved_or_retry(process, addr, access, FaultRetryReason::DuplicateFault)
                }
                MmapPageInstall::Retry(retry) => UserFaultOutcome::Retry(retry),
                MmapPageInstall::Failed => UserFaultOutcome::Fatal(UserFaultFatal::Segv),
            }
        }
        MmapFaultResult::PageCache(mut page) => {
            let ppn = {
                let _resolve_scope = crate::perf::time_scope(
                    crate::perf::ProfilePoint::PageFaultMmapResolvePageCache,
                );
                page.resolve_ppn()
            };
            let ppn = match ppn {
                MmapPageCacheResolve::Ready(ppn) => ppn,
                MmapPageCacheResolve::Retry(retry) => return UserFaultOutcome::Retry(retry),
                MmapPageCacheResolve::Failed => {
                    return UserFaultOutcome::Fatal(UserFaultFatal::Segv);
                }
            };
            let _install_scope =
                crate::perf::time_scope(crate::perf::ProfilePoint::PageFaultMmapInstallPageCache);
            let install = process
                .inner_exclusive_access()
                .memory_set
                .install_mmap_page_cache_fault_page(page, ppn);
            match install {
                MmapPageCacheInstall::InstalledOrDuplicate => {
                    resolved_or_retry(process, addr, access, FaultRetryReason::DuplicateFault)
                }
                MmapPageCacheInstall::Retry(retry) => UserFaultOutcome::Retry(retry),
                MmapPageCacheInstall::Failed => UserFaultOutcome::Fatal(UserFaultFatal::Segv),
            }
        }
    }
}

pub(crate) fn resolve_user_page_fault(
    process: &Arc<ProcessControlBlock>,
    addr: usize,
    access: MmapFaultAccess,
) -> UserFaultOutcome {
    let _profile_scope = crate::perf::time_scope(crate::perf::ProfilePoint::PageFault);
    if access == MmapFaultAccess::Write {
        let mut inner = process.inner_exclusive_access();
        {
            let _cow_scope = crate::perf::time_scope(crate::perf::ProfilePoint::PageFaultCow);
            if inner.memory_set.resolve_cow_page_fault(addr) {
                drop(inner);
                return resolved_or_retry(process, addr, access, FaultRetryReason::InstallRace);
            }
        }
        let _prepare_scope =
            crate::perf::time_scope(crate::perf::ProfilePoint::PageFaultMmapPrepare);
        let fault = inner.memory_set.prepare_mmap_page_fault(addr, access);
        drop(inner);
        if let Some(fault) = fault {
            return resolve_prepared_mmap_fault(process, addr, access, fault);
        }
    } else {
        let fault = {
            let _prepare_scope =
                crate::perf::time_scope(crate::perf::ProfilePoint::PageFaultMmapPrepare);
            process
                .inner_exclusive_access()
                .memory_set
                .prepare_mmap_page_fault(addr, access)
        };
        if let Some(fault) = fault {
            return resolve_prepared_mmap_fault(process, addr, access, fault);
        }
    }

    let resolved = {
        let _lazy_scope = crate::perf::time_scope(crate::perf::ProfilePoint::PageFaultLazyFramed);
        process
            .inner_exclusive_access()
            .memory_set
            .resolve_lazy_framed_page_fault(addr, access)
    };
    if resolved {
        resolved_or_retry(process, addr, access, FaultRetryReason::InstallRace)
    } else {
        UserFaultOutcome::Fatal(UserFaultFatal::Segv)
    }
}
mod perf;
