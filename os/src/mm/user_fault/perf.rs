use super::{FaultOrigin, FaultRetryReason};

#[cfg(feature = "perf-counters")]
mod enabled {
    use super::{FaultOrigin, FaultRetryReason};
    use alloc::string::String;
    use core::fmt::Write;
    use core::sync::atomic::{AtomicUsize, Ordering};

    static HARDWARE_RETRIES: AtomicUsize = AtomicUsize::new(0);
    static USERCOPY_RETRIES: AtomicUsize = AtomicUsize::new(0);
    static GENERATION_UNSTABLE_RETRIES: AtomicUsize = AtomicUsize::new(0);
    static GENERATION_CHANGED_RETRIES: AtomicUsize = AtomicUsize::new(0);
    static STALE_INSTALL_RETRIES: AtomicUsize = AtomicUsize::new(0);
    static VMA_CHANGED_RETRIES: AtomicUsize = AtomicUsize::new(0);
    static DUPLICATE_FAULT_RETRIES: AtomicUsize = AtomicUsize::new(0);
    static INSTALL_RACE_RETRIES: AtomicUsize = AtomicUsize::new(0);
    static HARDWARE_RETRY_WAITS: AtomicUsize = AtomicUsize::new(0);
    static USERCOPY_RETRY_WAITS: AtomicUsize = AtomicUsize::new(0);
    static HARDWARE_RETRY_WAIT_US: AtomicUsize = AtomicUsize::new(0);
    static USERCOPY_RETRY_WAIT_US: AtomicUsize = AtomicUsize::new(0);
    static HARDWARE_RETRY_YIELDS: AtomicUsize = AtomicUsize::new(0);
    static USERCOPY_RETRY_YIELDS: AtomicUsize = AtomicUsize::new(0);
    static HARDWARE_MAX_CONSECUTIVE_RETRY: AtomicUsize = AtomicUsize::new(0);
    static USERCOPY_MAX_CONSECUTIVE_RETRY: AtomicUsize = AtomicUsize::new(0);
    static USERCOPY_RETRY_RESOLVED: AtomicUsize = AtomicUsize::new(0);
    static USERCOPY_RETRY_FATAL: AtomicUsize = AtomicUsize::new(0);

    fn update_max(counter: &AtomicUsize, value: usize) {
        counter.fetch_max(value, Ordering::Relaxed);
    }

    pub(crate) fn record_retry(origin: FaultOrigin, reason: FaultRetryReason) {
        match origin {
            FaultOrigin::Hardware => HARDWARE_RETRIES.fetch_add(1, Ordering::Relaxed),
            FaultOrigin::Usercopy => USERCOPY_RETRIES.fetch_add(1, Ordering::Relaxed),
        };
        match reason {
            FaultRetryReason::GenerationUnstable => {
                GENERATION_UNSTABLE_RETRIES.fetch_add(1, Ordering::Relaxed)
            }
            FaultRetryReason::GenerationChanged => {
                GENERATION_CHANGED_RETRIES.fetch_add(1, Ordering::Relaxed)
            }
            FaultRetryReason::StaleInstall => STALE_INSTALL_RETRIES.fetch_add(1, Ordering::Relaxed),
            FaultRetryReason::VmaChanged => VMA_CHANGED_RETRIES.fetch_add(1, Ordering::Relaxed),
            FaultRetryReason::DuplicateFault => {
                DUPLICATE_FAULT_RETRIES.fetch_add(1, Ordering::Relaxed)
            }
            FaultRetryReason::InstallRace => INSTALL_RACE_RETRIES.fetch_add(1, Ordering::Relaxed),
        };
    }

    pub(crate) fn record_wait(origin: FaultOrigin, waited_us: usize) {
        let (waits, total_us) = match origin {
            FaultOrigin::Hardware => (&HARDWARE_RETRY_WAITS, &HARDWARE_RETRY_WAIT_US),
            FaultOrigin::Usercopy => (&USERCOPY_RETRY_WAITS, &USERCOPY_RETRY_WAIT_US),
        };
        waits.fetch_add(1, Ordering::Relaxed);
        total_us.fetch_add(waited_us, Ordering::Relaxed);
    }

    pub(crate) fn record_yield(origin: FaultOrigin) {
        match origin {
            FaultOrigin::Hardware => HARDWARE_RETRY_YIELDS.fetch_add(1, Ordering::Relaxed),
            FaultOrigin::Usercopy => USERCOPY_RETRY_YIELDS.fetch_add(1, Ordering::Relaxed),
        };
    }

    pub(crate) fn record_chain(origin: FaultOrigin, consecutive: usize) {
        match origin {
            FaultOrigin::Hardware => update_max(&HARDWARE_MAX_CONSECUTIVE_RETRY, consecutive),
            FaultOrigin::Usercopy => update_max(&USERCOPY_MAX_CONSECUTIVE_RETRY, consecutive),
        }
    }

    pub(crate) fn record_usercopy_terminal(resolved: bool) {
        if resolved {
            USERCOPY_RETRY_RESOLVED.fetch_add(1, Ordering::Relaxed);
        } else {
            USERCOPY_RETRY_FATAL.fetch_add(1, Ordering::Relaxed);
        }
    }

    pub(crate) fn append_stats(content: &mut String) {
        writeln!(
            content,
            "fault_hardware_retries {}\n\
             fault_usercopy_retries {}\n\
             fault_retry_generation_unstable {}\n\
             fault_retry_generation_changed {}\n\
             fault_retry_stale_install {}\n\
             fault_retry_vma_changed {}\n\
             fault_retry_duplicate_fault {}\n\
             fault_retry_install_race {}\n\
             fault_hardware_retry_waits {}\n\
             fault_usercopy_retry_waits {}\n\
             fault_hardware_retry_wait_us {}\n\
             fault_usercopy_retry_wait_us {}\n\
             fault_hardware_retry_yields {}\n\
             fault_usercopy_retry_yields {}\n\
             fault_hardware_max_consecutive_retry {}\n\
             fault_usercopy_max_consecutive_retry {}\n\
             fault_usercopy_retry_resolved {}\n\
             fault_usercopy_retry_fatal {}",
            HARDWARE_RETRIES.load(Ordering::Relaxed),
            USERCOPY_RETRIES.load(Ordering::Relaxed),
            GENERATION_UNSTABLE_RETRIES.load(Ordering::Relaxed),
            GENERATION_CHANGED_RETRIES.load(Ordering::Relaxed),
            STALE_INSTALL_RETRIES.load(Ordering::Relaxed),
            VMA_CHANGED_RETRIES.load(Ordering::Relaxed),
            DUPLICATE_FAULT_RETRIES.load(Ordering::Relaxed),
            INSTALL_RACE_RETRIES.load(Ordering::Relaxed),
            HARDWARE_RETRY_WAITS.load(Ordering::Relaxed),
            USERCOPY_RETRY_WAITS.load(Ordering::Relaxed),
            HARDWARE_RETRY_WAIT_US.load(Ordering::Relaxed),
            USERCOPY_RETRY_WAIT_US.load(Ordering::Relaxed),
            HARDWARE_RETRY_YIELDS.load(Ordering::Relaxed),
            USERCOPY_RETRY_YIELDS.load(Ordering::Relaxed),
            HARDWARE_MAX_CONSECUTIVE_RETRY.load(Ordering::Relaxed),
            USERCOPY_MAX_CONSECUTIVE_RETRY.load(Ordering::Relaxed),
            USERCOPY_RETRY_RESOLVED.load(Ordering::Relaxed),
            USERCOPY_RETRY_FATAL.load(Ordering::Relaxed),
        )
        .expect("writing fault perf counters to String cannot fail");
    }
}

#[cfg(feature = "perf-counters")]
pub(crate) use enabled::*;

#[cfg(not(feature = "perf-counters"))]
mod disabled {
    use super::{FaultOrigin, FaultRetryReason};

    #[inline(always)]
    pub(crate) fn record_retry(_origin: FaultOrigin, _reason: FaultRetryReason) {}

    #[inline(always)]
    pub(crate) fn record_wait(_origin: FaultOrigin, _waited_us: usize) {}

    #[inline(always)]
    pub(crate) fn record_yield(_origin: FaultOrigin) {}

    #[inline(always)]
    pub(crate) fn record_chain(_origin: FaultOrigin, _consecutive: usize) {}

    #[inline(always)]
    pub(crate) fn record_usercopy_terminal(_resolved: bool) {}
}

#[cfg(not(feature = "perf-counters"))]
pub(crate) use disabled::*;
