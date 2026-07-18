use crate::config::MAX_CPUS;
use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use fdt::Fdt;

pub type CpuId = usize;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CpuMask(u64);

impl CpuMask {
    pub const fn empty() -> Self {
        Self(0)
    }

    pub const fn first(count: usize) -> Self {
        if count == 0 {
            Self::empty()
        } else if count >= u64::BITS as usize {
            Self(u64::MAX)
        } else {
            Self((1u64 << count) - 1)
        }
    }

    pub const fn single(cpu: CpuId) -> Self {
        assert!(cpu < u64::BITS as usize, "CPU ID does not fit CpuMask");
        Self(1u64 << cpu)
    }

    pub const fn bits(self) -> u64 {
        self.0
    }

    #[allow(dead_code)]
    pub const fn contains(self, cpu: CpuId) -> bool {
        cpu < u64::BITS as usize && self.0 & (1u64 << cpu) != 0
    }

    pub const fn count(self) -> usize {
        self.0.count_ones() as usize
    }

    #[allow(dead_code)]
    pub fn insert(&mut self, cpu: CpuId) {
        assert!(cpu < MAX_CPUS, "CPU ID exceeds MAX_CPUS");
        self.0 |= 1u64 << cpu;
    }

    #[allow(dead_code)]
    pub fn remove(&mut self, cpu: CpuId) {
        assert!(cpu < MAX_CPUS, "CPU ID exceeds MAX_CPUS");
        self.0 &= !(1u64 << cpu);
    }
}

pub struct AtomicCpuMask(AtomicU64);

impl AtomicCpuMask {
    pub const fn new(mask: CpuMask) -> Self {
        Self(AtomicU64::new(mask.bits()))
    }

    pub fn load(&self, order: Ordering) -> CpuMask {
        CpuMask(self.0.load(order))
    }

    pub fn store(&self, mask: CpuMask, order: Ordering) {
        self.0.store(mask.bits(), order);
    }

    #[allow(dead_code)]
    pub fn insert(&self, cpu: CpuId, order: Ordering) {
        assert!(cpu < MAX_CPUS, "CPU ID exceeds MAX_CPUS");
        self.0.fetch_or(1u64 << cpu, order);
    }

    #[allow(dead_code)]
    pub fn remove(&self, cpu: CpuId, order: Ordering) {
        assert!(cpu < MAX_CPUS, "CPU ID exceeds MAX_CPUS");
        self.0.fetch_and(!(1u64 << cpu), order);
    }
}

#[derive(Clone, Copy)]
pub struct CpuTopology {
    boot_hw_id: usize,
    logical_to_hw_id: [usize; MAX_CPUS],
    possible_count: usize,
}

impl CpuTopology {
    const fn empty() -> Self {
        Self {
            boot_hw_id: 0,
            logical_to_hw_id: [0; MAX_CPUS],
            possible_count: 0,
        }
    }

    fn discover(fdt: &Fdt<'_>, boot_hw_id: usize) -> Self {
        assert!(MAX_CPUS <= u64::BITS as usize, "MAX_CPUS exceeds CpuMask");

        let mut topology = Self {
            boot_hw_id,
            ..Self::empty()
        };
        for cpu in fdt.cpus() {
            if !cpu_is_enabled(cpu) {
                continue;
            }
            for hw_id in cpu.ids().all() {
                topology.push_hardware_id(hw_id);
            }
        }

        assert_ne!(topology.possible_count, 0, "DTB has no enabled CPUs");
        let boot_index = topology.logical_to_hw_id[..topology.possible_count]
            .iter()
            .position(|hw_id| *hw_id == boot_hw_id)
            .unwrap_or_else(|| panic!("boot CPU hardware ID {boot_hw_id} is absent from DTB"));
        topology.logical_to_hw_id.swap(0, boot_index);
        topology
    }

    fn push_hardware_id(&mut self, hw_id: usize) {
        assert!(
            !self.logical_to_hw_id[..self.possible_count].contains(&hw_id),
            "duplicate CPU hardware ID {hw_id} in DTB"
        );
        assert!(
            self.possible_count < MAX_CPUS,
            "DTB CPU count exceeds MAX_CPUS={MAX_CPUS}"
        );
        self.logical_to_hw_id[self.possible_count] = hw_id;
        self.possible_count += 1;
    }

    pub fn boot_hw_id(&self) -> usize {
        self.boot_hw_id
    }

    pub fn possible_count(&self) -> usize {
        self.possible_count
    }

    pub fn possible_mask(&self) -> CpuMask {
        CpuMask::first(self.possible_count)
    }

    pub fn hardware_ids(&self) -> &[usize] {
        &self.logical_to_hw_id[..self.possible_count]
    }

    #[allow(dead_code)]
    pub fn hardware_id(&self, cpu: CpuId) -> usize {
        assert!(cpu < self.possible_count, "logical CPU ID is not possible");
        self.logical_to_hw_id[cpu]
    }

    #[allow(dead_code)]
    pub fn logical_id(&self, hw_id: usize) -> Option<CpuId> {
        self.hardware_ids().iter().position(|id| *id == hw_id)
    }
}

fn cpu_is_enabled(cpu: fdt::standard_nodes::Cpu<'_, '_>) -> bool {
    let Some(status) = cpu.property("status") else {
        return true;
    };
    let Ok(status) = core::str::from_utf8(status.value) else {
        return false;
    };
    matches!(status.trim_end_matches('\0'), "ok" | "okay")
}

struct CpuTopologyCell {
    initialized: AtomicBool,
    inner: UnsafeCell<CpuTopology>,
}

unsafe impl Sync for CpuTopologyCell {}

impl CpuTopologyCell {
    const fn new() -> Self {
        Self {
            initialized: AtomicBool::new(false),
            inner: UnsafeCell::new(CpuTopology::empty()),
        }
    }

    fn init(&self, topology: CpuTopology) {
        assert!(
            !self.initialized.load(Ordering::Relaxed),
            "CPU topology initialized twice"
        );
        unsafe {
            *self.inner.get() = topology;
        }
        self.initialized.store(true, Ordering::Release);
    }

    fn get(&self) -> &'static CpuTopology {
        assert!(
            self.initialized.load(Ordering::Acquire),
            "CPU topology accessed before DTB init"
        );
        unsafe { &*self.inner.get() }
    }
}

static CPU_TOPOLOGY: CpuTopologyCell = CpuTopologyCell::new();
static ONLINE_CPUS: AtomicCpuMask = AtomicCpuMask::new(CpuMask::empty());

pub fn init_from_dtb(fdt: &Fdt<'_>, boot_hw_id: usize) {
    let topology = CpuTopology::discover(fdt, boot_hw_id);
    CPU_TOPOLOGY.init(topology);
    // Phase 0 deliberately leaves application processors parked in firmware.
    // The actual boot CPU is normalized to logical CPU 0.
    ONLINE_CPUS.store(CpuMask::single(0), Ordering::Release);
}

pub fn topology() -> &'static CpuTopology {
    CPU_TOPOLOGY.get()
}

pub fn online_mask() -> CpuMask {
    ONLINE_CPUS.load(Ordering::Acquire)
}
