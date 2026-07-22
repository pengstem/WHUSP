use crate::cpu::{AtomicCpuMask, CpuId, CpuMask};
use alloc::sync::Arc;
use core::sync::atomic::{AtomicUsize, Ordering};

const NO_ADDRESS_SPACE_ID: usize = 0;
static NEXT_ADDRESS_SPACE_ID: AtomicUsize = AtomicUsize::new(1);

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
        assert_eq!(
            previous.bits(),
            0,
            "shared address space entered multiple CPUs before shootdown: id={} active={:#x} entering={cpu}",
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
