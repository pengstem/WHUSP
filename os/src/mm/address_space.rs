use crate::cpu::{AtomicCpuMask, CpuId, CpuMask};
use alloc::sync::Arc;
use core::sync::atomic::{AtomicUsize, Ordering};

const NO_ADDRESS_SPACE_ID: usize = 0;
const TLB_RANGE_FULL_FLUSH_THRESHOLD: usize = 64;
static NEXT_ADDRESS_SPACE_ID: AtomicUsize = AtomicUsize::new(1);

pub(crate) fn invalidate_global_tlb_range(start: usize, size: usize) {
    assert_ne!(size, 0, "empty global TLB invalidation");
    assert_eq!(
        start % crate::config::PAGE_SIZE,
        0,
        "global TLB invalidation start is not page aligned"
    );
    assert_eq!(
        size % crate::config::PAGE_SIZE,
        0,
        "global TLB invalidation size is not page aligned"
    );
    let pages = size / crate::config::PAGE_SIZE;
    let (start, size) = if pages > TLB_RANGE_FULL_FLUSH_THRESHOLD {
        crate::perf::record_tlb_flush_all();
        (0, usize::MAX)
    } else {
        crate::perf::record_tlb_flush_range(pages);
        (start, size)
    };

    crate::arch::mm::publish_pte_barrier();
    let current = crate::cpu::current_id();
    let online = crate::cpu::online_mask();
    assert!(
        online.contains(current),
        "global TLB invalidation initiated by an offline CPU"
    );
    crate::arch::mm::flush_tlb_range(start, size);
    let remote = CpuMask::from_bits(online.bits() & !CpuMask::single(current).bits());
    if remote.bits() != 0 {
        crate::arch::smp::remote_tlb_flush(remote, start, size).unwrap_or_else(|error| {
            panic!(
                "global TLB shootdown failed: targets={:#x} error={error:#x}",
                remote.bits(),
            )
        });
    }
    // A CPU that is not yet online is excluded intentionally. Secondary boot
    // activates the published kernel root and performs a full local flush
    // before setting its online bit, so it cannot retain the pre-edit entry.
}

pub(crate) struct AddressSpaceControl {
    id: usize,
    active_cpus: AtomicCpuMask,
    tlb_generation: AtomicUsize,
}

impl AddressSpaceControl {
    pub(crate) fn new() -> Arc<Self> {
        let id = NEXT_ADDRESS_SPACE_ID.fetch_add(1, Ordering::Relaxed);
        assert_ne!(id, NO_ADDRESS_SPACE_ID, "address-space ID wrapped");
        Arc::new(Self {
            id,
            active_cpus: AtomicCpuMask::new(CpuMask::empty()),
            // Generation zero is reserved for a CPU that has not observed an
            // address space. Later mutation units increment from this value.
            tlb_generation: AtomicUsize::new(1),
        })
    }

    pub(crate) fn enter_cpu(self: &Arc<Self>, cpu: CpuId) -> ActiveAddressSpace {
        let previous = self.active_cpus.fetch_insert(cpu, Ordering::SeqCst);
        assert!(
            !previous.contains(cpu),
            "CPU entered address space twice: id={} active={:#x} entering={cpu}",
            self.id,
            previous.bits(),
        );
        ActiveAddressSpace {
            control: Arc::clone(self),
            cpu,
        }
    }

    fn generation(&self) -> usize {
        self.tlb_generation.load(Ordering::SeqCst)
    }

    pub(crate) fn invalidate_tlb_all(&self) {
        crate::perf::record_tlb_flush_all();
        self.invalidate_tlb_range_inner(0, usize::MAX);
    }

    pub(crate) fn invalidate_tlb_page(&self, virtual_address: usize) {
        assert_eq!(
            virtual_address % crate::config::PAGE_SIZE,
            0,
            "address-space TLB invalidation is not page aligned"
        );
        crate::perf::record_tlb_flush_range(1);
        self.invalidate_tlb_range_inner(virtual_address, crate::config::PAGE_SIZE);
    }

    pub(crate) fn invalidate_tlb_range(&self, start: usize, size: usize) {
        assert_ne!(size, 0, "empty address-space TLB invalidation");
        assert_eq!(
            start % crate::config::PAGE_SIZE,
            0,
            "address-space TLB invalidation start is not page aligned"
        );
        assert_eq!(
            size % crate::config::PAGE_SIZE,
            0,
            "address-space TLB invalidation size is not page aligned"
        );
        let pages = size / crate::config::PAGE_SIZE;
        if pages > TLB_RANGE_FULL_FLUSH_THRESHOLD {
            self.invalidate_tlb_all();
        } else {
            crate::perf::record_tlb_flush_range(pages);
            self.invalidate_tlb_range_inner(start, size);
        }
    }

    fn invalidate_tlb_range_inner(&self, start: usize, size: usize) {
        // Publish the PTE writes before making the new generation visible.
        // enter_cpu() and this snapshot are SeqCst: an enter ordered before
        // the snapshot is targeted below, while an enter ordered afterward
        // must observe this generation in prepare_user_return() and flush
        // locally before executing user instructions.
        crate::arch::mm::publish_pte_barrier();
        let generation = self
            .tlb_generation
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |generation| {
                generation.checked_add(1)
            })
            .unwrap_or_else(|_| panic!("address-space TLB generation wrapped: id={}", self.id))
            + 1;
        assert_ne!(generation, 0, "address-space TLB generation is zero");

        let active = self.active_cpus.load(Ordering::SeqCst);
        if active.bits() == 0 {
            return;
        }
        let current = crate::cpu::current_id();
        if active.contains(current) {
            crate::arch::mm::flush_tlb_range(start, size);
        }

        let remote = CpuMask::from_bits(active.bits() & !CpuMask::single(current).bits());
        if remote.bits() != 0 {
            crate::arch::smp::remote_tlb_flush(remote, start, size).unwrap_or_else(|error| {
                panic!(
                    "address-space TLB shootdown failed: id={} generation={} targets={:#x} error={error:#x}",
                    self.id,
                    generation,
                    remote.bits(),
                )
            });
        }
    }
}

impl Drop for AddressSpaceControl {
    fn drop(&mut self) {
        assert_eq!(
            self.active_cpus.load(Ordering::Acquire).bits(),
            0,
            "dropping active address space {}",
            self.id,
        );
    }
}

pub(crate) struct ActiveAddressSpace {
    control: Arc<AddressSpaceControl>,
    cpu: CpuId,
}

impl ActiveAddressSpace {
    pub(crate) fn belongs_to(&self, control: &Arc<AddressSpaceControl>) -> bool {
        Arc::ptr_eq(&self.control, control)
    }

    pub(crate) fn prepare_user_return(&self) {
        assert_eq!(
            self.cpu,
            crate::cpu::current_id(),
            "address-space return prepared on the wrong CPU",
        );
        crate::cpu::current()
            .mmu()
            .observe_address_space(self.control.id, self.control.generation());
    }
}

impl Drop for ActiveAddressSpace {
    fn drop(&mut self) {
        let previous = self
            .control
            .active_cpus
            .fetch_remove(self.cpu, Ordering::SeqCst);
        assert!(
            previous.contains(self.cpu),
            "CPU {} left inactive address space {}",
            self.cpu,
            self.control.id,
        );
    }
}
