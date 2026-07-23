use crate::config::MAX_CPUS;
use crate::cpu::{AtomicCpuMask, CpuId, CpuMask};
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use loongArch64::consts::{
    LOONGARCH_IOCSR_IPI_CLEAR, LOONGARCH_IOCSR_IPI_EN, LOONGARCH_IOCSR_IPI_STATUS,
};
use loongArch64::iocsr::{iocsr_read_w, iocsr_write_w};
use loongArch64::ipi::{csr_mail_send, send_ipi_single};

const BOOT_IPI_ACTION: u32 = 1;
const BOOT_MAILBOX: usize = 0;
const KSAVE_CPU_LOCAL: usize = 0x33;

struct TlbRequest {
    active: AtomicBool,
    sequence: AtomicUsize,
    start: AtomicUsize,
    size: AtomicUsize,
    remaining: AtomicCpuMask,
}

impl TlbRequest {
    const fn new() -> Self {
        Self {
            active: AtomicBool::new(false),
            sequence: AtomicUsize::new(0),
            start: AtomicUsize::new(0),
            size: AtomicUsize::new(0),
            remaining: AtomicCpuMask::new(CpuMask::empty()),
        }
    }
}

static TLB_REQUESTS: [TlbRequest; MAX_CPUS] = [const { TlbRequest::new() }; MAX_CPUS];
static TLB_PENDING_SOURCES: [AtomicCpuMask; MAX_CPUS] =
    [const { AtomicCpuMask::new(CpuMask::empty()) }; MAX_CPUS];
static TLB_OBSERVED_SEQUENCE: [[AtomicUsize; MAX_CPUS]; MAX_CPUS] =
    [const { [const { AtomicUsize::new(0) }; MAX_CPUS] }; MAX_CPUS];

pub fn validate_startup_extensions() -> Result<(), &'static str> {
    if !loongArch64::cpu::get_support_iocsr() {
        return Err("LoongArch IOCSR is unavailable");
    }
    Ok(())
}

pub fn start_secondary(_logical_id: CpuId, hardware_id: usize) -> Result<(), usize> {
    unsafe extern "C" {
        safe fn secondary_entry();
    }
    let entry = crate::arch::loongarch64::mm::virt_to_phys(secondary_entry as usize) as u64;
    csr_mail_send(entry, hardware_id, BOOT_MAILBOX);
    send_ipi_single(hardware_id, BOOT_IPI_ACTION);
    Ok(())
}

pub fn send_ipi(logical_id: CpuId) -> Result<(), usize> {
    let hardware_id = crate::cpu::topology().hardware_id(logical_id);
    send_ipi_single(hardware_id, BOOT_IPI_ACTION);
    Ok(())
}

pub fn enable_local_ipi() {
    clear_local_ipi();
    iocsr_write_w(LOONGARCH_IOCSR_IPI_EN, u32::MAX);
    crate::trap::enable_ipi_interrupt();
}

pub fn clear_local_ipi() {
    let pending = iocsr_read_w(LOONGARCH_IOCSR_IPI_STATUS);
    if pending != 0 {
        iocsr_write_w(LOONGARCH_IOCSR_IPI_CLEAR, pending);
    }
}

pub fn remote_tlb_flush(targets: CpuMask, start: usize, size: usize) -> Result<(), usize> {
    assert_eq!(
        targets.bits() & !crate::cpu::online_mask().bits(),
        0,
        "LoongArch TLB shootdown targets an offline CPU"
    );
    let source = crate::cpu::current_id();
    assert!(
        !targets.contains(source),
        "LoongArch TLB shootdown target mask contains the caller"
    );
    assert_eq!(
        start % crate::config::PAGE_SIZE,
        0,
        "LoongArch TLB shootdown start is not page aligned"
    );
    assert!(
        size == usize::MAX || size % crate::config::PAGE_SIZE == 0,
        "LoongArch TLB shootdown size is not page aligned"
    );
    assert!(
        size != 0 || start == 0,
        "zero-size LoongArch shootdown must select the full address space"
    );
    if targets.bits() == 0 {
        return Ok(());
    }

    crate::arch::mm::publish_pte_barrier();

    let request = &TLB_REQUESTS[source];
    assert!(
        !request.active.swap(true, Ordering::AcqRel),
        "CPU {source} started a nested TLB shootdown"
    );
    assert_eq!(
        request.remaining.load(Ordering::Acquire).bits(),
        0,
        "CPU {source} reused an incomplete TLB request"
    );
    request.start.store(start, Ordering::Relaxed);
    request.size.store(size, Ordering::Relaxed);
    let sequence = request.sequence.load(Ordering::Relaxed).wrapping_add(1);
    assert_ne!(sequence, 0, "LoongArch TLB request sequence wrapped");
    request.sequence.store(sequence, Ordering::Release);

    let mut send_error = None;
    for target in 0..crate::cpu::topology().possible_count() {
        if !targets.contains(target) {
            continue;
        }
        let old_remaining = request.remaining.fetch_insert(target, Ordering::AcqRel);
        assert!(
            !old_remaining.contains(target),
            "TLB target {target} was already pending for source {source}"
        );
        let old_sources = TLB_PENDING_SOURCES[target].fetch_insert(source, Ordering::AcqRel);
        assert!(
            !old_sources.contains(source),
            "TLB source {source} was already pending on target {target}"
        );
        if old_sources.bits() == 0
            && let Err(error) = send_ipi(target)
        {
            // A target that is itself waiting may have polled and completed
            // the request before the failed transport result is observed.
            let pending = TLB_PENDING_SOURCES[target].fetch_remove(source, Ordering::AcqRel);
            if pending.contains(source) {
                let remaining = request.remaining.fetch_remove(target, Ordering::AcqRel);
                assert!(remaining.contains(target));
            }
            send_error = Some(error);
            break;
        }
    }

    wait_for_tlb_completion(source, sequence, request);
    request.active.store(false, Ordering::Release);
    send_error.map_or(Ok(()), Err)
}

fn wait_for_tlb_completion(source: CpuId, sequence: usize, request: &TlbRequest) {
    let start = crate::timer::get_time();
    let timeout = crate::config::clock_freq().saturating_mul(2);
    loop {
        let remaining = request.remaining.load(Ordering::Acquire);
        if remaining.bits() == 0 {
            return;
        }
        // Page-table mutation currently disables local interrupts. Polling
        // incoming requests here closes the cross-shootdown deadlock where
        // two CPUs synchronously target one another.
        handle_tlb_ipi();
        if crate::timer::get_time().wrapping_sub(start) >= timeout {
            panic!(
                "LoongArch TLB shootdown timeout: source={source} sequence={sequence} remaining={:#x}",
                remaining.bits()
            );
        }
        core::hint::spin_loop();
    }
}

pub fn handle_tlb_ipi() -> bool {
    let target = crate::cpu::current_id();
    let mut handled = false;
    loop {
        let sources = TLB_PENDING_SOURCES[target].swap(CpuMask::empty(), Ordering::AcqRel);
        if sources.bits() == 0 {
            return handled;
        }
        handled = true;
        for source in 0..crate::cpu::topology().possible_count() {
            if !sources.contains(source) {
                continue;
            }
            assert_ne!(source, target, "TLB request targeted its source CPU");
            let request = &TLB_REQUESTS[source];
            assert!(
                request.active.load(Ordering::Acquire),
                "target {target} observed an inactive TLB request from {source}"
            );
            let sequence = request.sequence.load(Ordering::Acquire);
            let previous = TLB_OBSERVED_SEQUENCE[target][source].load(Ordering::Relaxed);
            assert!(
                sequence > previous,
                "stale LoongArch TLB request: source={source} target={target} sequence={sequence} previous={previous}"
            );
            let start = request.start.load(Ordering::Relaxed);
            let size = request.size.load(Ordering::Relaxed);
            crate::arch::mm::flush_tlb_range(start, size);
            TLB_OBSERVED_SEQUENCE[target][source].store(sequence, Ordering::Release);
            let remaining = request.remaining.fetch_remove(target, Ordering::AcqRel);
            assert!(
                remaining.contains(target),
                "target {target} acknowledged a completed TLB request from {source}"
            );
        }
    }
}

pub const fn tlb_backend_name() -> &'static str {
    "ipi-invtlb-ack"
}

pub fn install_cpu_local(pointer: usize) {
    unsafe {
        core::arch::asm!(
            "csrwr {pointer}, {ksave}",
            pointer = inout(reg) pointer => _,
            ksave = const KSAVE_CPU_LOCAL,
            options(nomem, nostack),
        );
    }
}

pub fn cpu_local_ptr() -> usize {
    let pointer: usize;
    unsafe {
        core::arch::asm!(
            "csrrd {pointer}, {ksave}",
            pointer = out(reg) pointer,
            ksave = const KSAVE_CPU_LOCAL,
            options(nomem, nostack),
        );
    }
    pointer
}

pub fn park_without_interrupts() -> ! {
    loop {
        unsafe {
            core::arch::asm!("idle 0");
        }
    }
}
