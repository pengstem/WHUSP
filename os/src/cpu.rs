use crate::config::MAX_CPUS;
use crate::sync::{LocalIrqGuard, SpinLock};
use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, AtomicUsize, Ordering};
use fdt::Fdt;
use log::info;

pub type CpuId = usize;

// Keep all external device interrupts on the boot scheduler CPU until Phase 4
// has made each driver queue safe for distributed interrupt handling.
pub const EXTERNAL_IRQ_OWNER_CPU: CpuId = 0;

// Global sleep, real-time, and POSIX timer heaps have one expiry owner. Every
// CPU still programs its local timer interrupt for scheduler preemption.
pub const TIMER_EXPIRY_OWNER_CPU: CpuId = 0;

pub const PHASE1_IPI_ROUNDS: usize = 32;
pub const PHASE2_LOCK_INCREMENTS: usize = 4096;

const CPU_STATE_OFFLINE: u8 = 0;
const CPU_STATE_START_REQUESTED: u8 = 1;
const CPU_STATE_EARLY: u8 = 2;
const CPU_STATE_ONLINE: u8 = 3;
const CPU_STATE_FAILED: u8 = 4;
const STARTUP_ERROR_NONE: usize = 0;
const STARTUP_ERROR_ID_MISMATCH: usize = 1;
const STARTUP_ERROR_BAD_TRANSITION: usize = 2;
const STARTUP_ERROR_TIMEOUT: usize = 3;

#[repr(C, align(64))]
struct CpuBootLocal {
    state: AtomicU8,
    startup_error: AtomicUsize,
}

#[repr(C, align(64))]
pub struct CpuLocal {
    logical_id: AtomicUsize,
    hardware_id: AtomicUsize,
    installed: AtomicBool,
    mmu: CpuMmuFastState,
}

#[repr(C)]
pub(crate) struct CpuMmuFastState {
    last_return_user_token: AtomicUsize,
    #[cfg(target_arch = "riscv64")]
    last_entry_kernel_token: AtomicUsize,
    return_tlb_dirty: AtomicBool,
    #[cfg(target_arch = "riscv64")]
    kernel_tlb_dirty: AtomicBool,
    observed_address_space_id: AtomicUsize,
    observed_tlb_generation: AtomicUsize,
}

impl CpuMmuFastState {
    const fn new() -> Self {
        Self {
            last_return_user_token: AtomicUsize::new(0),
            #[cfg(target_arch = "riscv64")]
            last_entry_kernel_token: AtomicUsize::new(0),
            return_tlb_dirty: AtomicBool::new(true),
            #[cfg(target_arch = "riscv64")]
            kernel_tlb_dirty: AtomicBool::new(true),
            observed_address_space_id: AtomicUsize::new(0),
            observed_tlb_generation: AtomicUsize::new(0),
        }
    }

    pub(crate) fn swap_last_return_user_token(&self, token: usize) -> usize {
        self.last_return_user_token.swap(token, Ordering::Relaxed)
    }

    #[cfg(target_arch = "riscv64")]
    pub(crate) fn swap_last_entry_kernel_token(&self, token: usize) -> usize {
        self.last_entry_kernel_token.swap(token, Ordering::Relaxed)
    }

    pub(crate) fn take_return_tlb_dirty(&self) -> bool {
        self.return_tlb_dirty.swap(false, Ordering::Relaxed)
    }

    #[cfg(target_arch = "riscv64")]
    pub(crate) fn take_kernel_tlb_dirty(&self) -> bool {
        self.kernel_tlb_dirty.swap(false, Ordering::Relaxed)
    }

    pub(crate) fn mark_return_tlb_dirty(&self) {
        self.return_tlb_dirty.store(true, Ordering::Relaxed);
    }

    #[cfg(target_arch = "riscv64")]
    pub(crate) fn mark_kernel_tlb_dirty(&self) {
        self.kernel_tlb_dirty.store(true, Ordering::Relaxed);
    }

    pub(crate) fn observe_address_space(&self, id: usize, generation: usize) {
        let previous_id = self.observed_address_space_id.swap(id, Ordering::Relaxed);
        let previous_generation = self
            .observed_tlb_generation
            .swap(generation, Ordering::Relaxed);
        assert!(
            previous_id != id || previous_generation <= generation,
            "address-space TLB generation regressed: id={id} previous={previous_generation} current={generation}",
        );
        if previous_id != id || previous_generation < generation {
            self.mark_return_tlb_dirty();
        }
    }
}

impl CpuLocal {
    const fn new() -> Self {
        Self {
            logical_id: AtomicUsize::new(usize::MAX),
            hardware_id: AtomicUsize::new(usize::MAX),
            installed: AtomicBool::new(false),
            mmu: CpuMmuFastState::new(),
        }
    }

    pub fn logical_id(&self) -> CpuId {
        self.logical_id.load(Ordering::Relaxed)
    }

    pub fn hardware_id(&self) -> usize {
        self.hardware_id.load(Ordering::Relaxed)
    }

    pub(crate) fn mmu(&self) -> &CpuMmuFastState {
        &self.mmu
    }

    fn mmu_ptr(&self) -> usize {
        &self.mmu as *const CpuMmuFastState as usize
    }
}

#[repr(C, align(64))]
struct Phase2ProbePerf {
    contended: AtomicUsize,
    spins: AtomicUsize,
    max_wait_ticks: AtomicUsize,
    max_hold_ticks: AtomicUsize,
    elapsed_ticks: AtomicUsize,
}

impl Phase2ProbePerf {
    const fn new() -> Self {
        Self {
            contended: AtomicUsize::new(0),
            spins: AtomicUsize::new(0),
            max_wait_ticks: AtomicUsize::new(0),
            max_hold_ticks: AtomicUsize::new(0),
            elapsed_ticks: AtomicUsize::new(0),
        }
    }

    fn reset(&self) {
        self.contended.store(0, Ordering::Relaxed);
        self.spins.store(0, Ordering::Relaxed);
        self.max_wait_ticks.store(0, Ordering::Relaxed);
        self.max_hold_ticks.store(0, Ordering::Relaxed);
        self.elapsed_ticks.store(0, Ordering::Relaxed);
    }
}

impl CpuBootLocal {
    const fn new() -> Self {
        Self {
            state: AtomicU8::new(CPU_STATE_OFFLINE),
            startup_error: AtomicUsize::new(STARTUP_ERROR_NONE),
        }
    }

    fn state_name(&self) -> &'static str {
        match self.state.load(Ordering::Acquire) {
            CPU_STATE_OFFLINE => "offline",
            CPU_STATE_START_REQUESTED => "start-requested",
            CPU_STATE_EARLY => "early",
            CPU_STATE_ONLINE => "online",
            CPU_STATE_FAILED => "failed",
            _ => "invalid",
        }
    }
}

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

    pub const fn from_bits(bits: u64) -> Self {
        Self(bits)
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

    #[cfg(target_arch = "loongarch64")]
    pub fn swap(&self, mask: CpuMask, order: Ordering) -> CpuMask {
        CpuMask(self.0.swap(mask.bits(), order))
    }

    #[allow(dead_code)]
    pub fn insert(&self, cpu: CpuId, order: Ordering) {
        assert!(cpu < MAX_CPUS, "CPU ID exceeds MAX_CPUS");
        self.0.fetch_or(1u64 << cpu, order);
    }

    pub fn fetch_insert(&self, cpu: CpuId, order: Ordering) -> CpuMask {
        assert!(cpu < MAX_CPUS, "CPU ID exceeds MAX_CPUS");
        CpuMask(self.0.fetch_or(1u64 << cpu, order))
    }

    #[allow(dead_code)]
    pub fn remove(&self, cpu: CpuId, order: Ordering) {
        assert!(cpu < MAX_CPUS, "CPU ID exceeds MAX_CPUS");
        self.0.fetch_and(!(1u64 << cpu), order);
    }

    pub fn fetch_remove(&self, cpu: CpuId, order: Ordering) -> CpuMask {
        assert!(cpu < MAX_CPUS, "CPU ID exceeds MAX_CPUS");
        CpuMask(self.0.fetch_and(!(1u64 << cpu), order))
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
static BOOT_ENTRY_COUNT: AtomicUsize = AtomicUsize::new(0);
static GLOBAL_INIT_COUNT: AtomicUsize = AtomicUsize::new(0);
static CPU_BOOT_LOCALS: [CpuBootLocal; MAX_CPUS] = [const { CpuBootLocal::new() }; MAX_CPUS];
static CPU_LOCALS: [CpuLocal; MAX_CPUS] = [const { CpuLocal::new() }; MAX_CPUS];

// LoongArch secondary entry has no SBI-style opaque argument. These symbols
// are consumed directly by entry.asm after it installs the high DMW alias.
#[unsafe(no_mangle)]
pub static CPU_EARLY_COUNT: AtomicUsize = AtomicUsize::new(0);
#[unsafe(no_mangle)]
pub static CPU_EARLY_HW_IDS: [AtomicUsize; MAX_CPUS] =
    [const { AtomicUsize::new(usize::MAX) }; MAX_CPUS];

static PROBE_ACTIVE: AtomicBool = AtomicBool::new(false);
static PROBE_COMMAND_SEQ: [AtomicUsize; MAX_CPUS] = [const { AtomicUsize::new(0) }; MAX_CPUS];
static PROBE_COMMAND_DONE: [AtomicUsize; MAX_CPUS] = [const { AtomicUsize::new(0) }; MAX_CPUS];
static PROBE_COMMAND_TARGET: [AtomicUsize; MAX_CPUS] = [const { AtomicUsize::new(0) }; MAX_CPUS];
static PROBE_RECEIVE_EXPECTED: [AtomicUsize; MAX_CPUS] = [const { AtomicUsize::new(0) }; MAX_CPUS];
static PROBE_RECEIVE_SEEN: [AtomicUsize; MAX_CPUS] = [const { AtomicUsize::new(0) }; MAX_CPUS];
static PROBE_SENT: [AtomicUsize; MAX_CPUS] = [const { AtomicUsize::new(0) }; MAX_CPUS];
static PROBE_RECEIVED: [AtomicUsize; MAX_CPUS] = [const { AtomicUsize::new(0) }; MAX_CPUS];
static PROBE_FAILURES: AtomicUsize = AtomicUsize::new(0);
static PROBE_DUPLICATES: AtomicUsize = AtomicUsize::new(0);
static PROBE_UNEXPECTED: AtomicUsize = AtomicUsize::new(0);

static PHASE2_PROBE_ACTIVE: AtomicBool = AtomicBool::new(false);
static PHASE2_PROBE_GO: AtomicBool = AtomicBool::new(false);
static PHASE2_PROBE_PENDING: [AtomicBool; MAX_CPUS] = [const { AtomicBool::new(false) }; MAX_CPUS];
static PHASE2_PROBE_DONE: AtomicCpuMask = AtomicCpuMask::new(CpuMask::empty());
static PHASE2_LOCK_COUNTER: SpinLock<usize> = SpinLock::new(0);
static PHASE2_IRQ_RESTORED: [AtomicBool; MAX_CPUS] = [const { AtomicBool::new(false) }; MAX_CPUS];
static PHASE2_PROBE_FAILURES: AtomicUsize = AtomicUsize::new(0);
static PHASE2_PROBE_PERF: [Phase2ProbePerf; MAX_CPUS] =
    [const { Phase2ProbePerf::new() }; MAX_CPUS];
static TLB_CROSS_PROBE_ACTIVE: AtomicBool = AtomicBool::new(false);
static TLB_CROSS_PROBE_GO: AtomicBool = AtomicBool::new(false);
static TLB_CROSS_COMMAND_PENDING: [AtomicBool; MAX_CPUS] =
    [const { AtomicBool::new(false) }; MAX_CPUS];
static TLB_CROSS_RUN_PENDING: [AtomicBool; MAX_CPUS] = [const { AtomicBool::new(false) }; MAX_CPUS];
static TLB_CROSS_READY: AtomicCpuMask = AtomicCpuMask::new(CpuMask::empty());
static TLB_CROSS_DONE: AtomicCpuMask = AtomicCpuMask::new(CpuMask::empty());
static SCHEDULER_APS_ACTIVE: AtomicBool = AtomicBool::new(false);
static SCHEDULER_ACTIVE_CPUS: AtomicCpuMask = AtomicCpuMask::new(CpuMask::empty());
static SCHEDULER_ACTIVE_LOGGED: AtomicBool = AtomicBool::new(false);
static SCHEDULER_WAKE_PENDING: [AtomicBool; MAX_CPUS] =
    [const { AtomicBool::new(false) }; MAX_CPUS];
static SCHEDULER_WAKE_CURSOR: AtomicUsize = AtomicUsize::new(0);

pub fn record_boot_entry() {
    let count = BOOT_ENTRY_COUNT.fetch_add(1, Ordering::Relaxed) + 1;
    assert_eq!(
        count, 1,
        "primary kernel boot entry executed more than once"
    );
}

pub fn record_global_init() {
    let count = GLOBAL_INIT_COUNT.fetch_add(1, Ordering::Relaxed) + 1;
    assert_eq!(
        count, 1,
        "global kernel initialization executed more than once"
    );
}

pub fn boot_entry_count() -> usize {
    BOOT_ENTRY_COUNT.load(Ordering::Relaxed)
}

pub fn global_init_count() -> usize {
    GLOBAL_INIT_COUNT.load(Ordering::Relaxed)
}

pub fn init_from_dtb(fdt: &Fdt<'_>, boot_hw_id: usize) {
    let topology = CpuTopology::discover(fdt, boot_hw_id);
    for (logical_id, hardware_id) in topology.hardware_ids().iter().copied().enumerate() {
        CPU_EARLY_HW_IDS[logical_id].store(hardware_id, Ordering::Relaxed);
    }
    CPU_EARLY_COUNT.store(topology.possible_count(), Ordering::Release);
    CPU_TOPOLOGY.init(topology);
    // Phase 0 deliberately leaves application processors parked in firmware.
    // The actual boot CPU is normalized to logical CPU 0.
    ONLINE_CPUS.store(CpuMask::single(0), Ordering::Release);
}

pub fn topology() -> &'static CpuTopology {
    CPU_TOPOLOGY.get()
}

pub fn install_current(logical_id: CpuId, hardware_id: usize) {
    assert!(
        logical_id < topology().possible_count(),
        "CPU-local ID is not possible"
    );
    assert_eq!(
        topology().hardware_id(logical_id),
        hardware_id,
        "CPU-local hardware ID disagrees with topology"
    );
    let local = &CPU_LOCALS[logical_id];
    assert!(
        !local.installed.load(Ordering::Acquire),
        "CPU-local state installed twice for logical CPU {logical_id}"
    );
    local.logical_id.store(logical_id, Ordering::Relaxed);
    local.hardware_id.store(hardware_id, Ordering::Relaxed);
    local.installed.store(true, Ordering::Release);
    crate::arch::smp::install_cpu_local(local as *const CpuLocal as usize);
    assert!(
        core::ptr::eq(current(), local),
        "architecture CPU-local pointer did not round-trip"
    );
}

pub fn current() -> &'static CpuLocal {
    let logical_id = try_current_id().expect("CPU-local pointer is not installed or invalid");
    &CPU_LOCALS[logical_id]
}

pub fn try_current_id() -> Option<CpuId> {
    let pointer = crate::arch::smp::cpu_local_ptr();
    let base = &CPU_LOCALS[0] as *const CpuLocal as usize;
    let stride = core::mem::size_of::<CpuLocal>();
    let bytes = stride.checked_mul(MAX_CPUS)?;
    let offset = pointer.checked_sub(base)?;
    if offset >= bytes || offset % stride != 0 {
        return None;
    }
    let logical_id = offset / stride;
    let local = &CPU_LOCALS[logical_id];
    (local.installed.load(Ordering::Acquire) && local.logical_id() == logical_id)
        .then_some(logical_id)
}

pub fn current_id() -> CpuId {
    current().logical_id()
}

pub fn external_irq_owner_hardware_id() -> usize {
    topology().hardware_id(EXTERNAL_IRQ_OWNER_CPU)
}

pub fn is_timer_expiry_owner() -> bool {
    current_id() == TIMER_EXPIRY_OWNER_CPU
}

/// Verify the Phase 4 policy that the current-core interrupt controller
/// context belongs to the one CPU configured for external device interrupts.
pub fn assert_current_external_irq_owner() -> usize {
    let current = current();
    assert_eq!(
        current.logical_id(),
        EXTERNAL_IRQ_OWNER_CPU,
        "external device IRQ reached a non-owner logical CPU"
    );
    let expected_hardware_id = external_irq_owner_hardware_id();
    assert_eq!(
        current.hardware_id(),
        expected_hardware_id,
        "external device IRQ used the wrong hardware CPU context"
    );
    expected_hardware_id
}

#[cfg(target_arch = "riscv64")]
pub fn current_ptr() -> usize {
    current() as *const CpuLocal as usize
}

pub fn online_mask() -> CpuMask {
    ONLINE_CPUS.load(Ordering::Acquire)
}

pub fn secondary_mark_early(hardware_id: usize, logical_id: CpuId) -> bool {
    let possible = CPU_EARLY_COUNT.load(Ordering::Acquire);
    if logical_id >= possible || CPU_EARLY_HW_IDS[logical_id].load(Ordering::Relaxed) != hardware_id
    {
        if logical_id < MAX_CPUS {
            CPU_BOOT_LOCALS[logical_id]
                .startup_error
                .store(STARTUP_ERROR_ID_MISMATCH, Ordering::Relaxed);
            CPU_BOOT_LOCALS[logical_id]
                .state
                .store(CPU_STATE_FAILED, Ordering::Release);
        }
        return false;
    }
    if CPU_BOOT_LOCALS[logical_id]
        .state
        .compare_exchange(
            CPU_STATE_START_REQUESTED,
            CPU_STATE_EARLY,
            Ordering::AcqRel,
            Ordering::Acquire,
        )
        .is_err()
    {
        CPU_BOOT_LOCALS[logical_id]
            .startup_error
            .store(STARTUP_ERROR_BAD_TRANSITION, Ordering::Relaxed);
        CPU_BOOT_LOCALS[logical_id]
            .state
            .store(CPU_STATE_FAILED, Ordering::Release);
        return false;
    }
    true
}

pub fn secondary_publish_online(logical_id: CpuId) {
    assert_eq!(
        CPU_BOOT_LOCALS[logical_id].state.load(Ordering::Acquire),
        CPU_STATE_EARLY,
        "secondary CPU published online from the wrong state"
    );
    CPU_BOOT_LOCALS[logical_id]
        .state
        .store(CPU_STATE_ONLINE, Ordering::Release);
    ONLINE_CPUS.insert(logical_id, Ordering::Release);
}

pub fn is_parked_secondary() -> bool {
    current_id() != 0 && !SCHEDULER_APS_ACTIVE.load(Ordering::Acquire)
}

pub fn scheduler_aps_active() -> bool {
    SCHEDULER_APS_ACTIVE.load(Ordering::Acquire)
}

pub fn activate_scheduler_aps() {
    SCHEDULER_APS_ACTIVE.store(true, Ordering::Release);
    for cpu in 1..topology().possible_count() {
        SCHEDULER_WAKE_PENDING[cpu].store(true, Ordering::Release);
        crate::arch::smp::send_ipi(cpu).unwrap_or_else(|error| {
            panic!("scheduler activation IPI to CPU {cpu} failed: {error:#x}")
        });
    }
}

pub fn scheduler_publish_active(cpu: CpuId) {
    let before = SCHEDULER_ACTIVE_CPUS.load(Ordering::Acquire);
    assert!(!before.contains(cpu), "CPU entered scheduler twice");
    SCHEDULER_ACTIVE_CPUS.insert(cpu, Ordering::AcqRel);
    let active = SCHEDULER_ACTIVE_CPUS.load(Ordering::Acquire);
    if active == online_mask()
        && SCHEDULER_ACTIVE_LOGGED
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    {
        info!(
            "smp schedulers: active_mask={:#x} count={}",
            active.bits(),
            active.count()
        );
    }
}

pub fn wake_scheduler_cpu(allowed: CpuMask) {
    if !scheduler_aps_active() {
        return;
    }
    let current = current_id();
    let cpu_count = topology().possible_count();
    let start = SCHEDULER_WAKE_CURSOR.fetch_add(1, Ordering::Relaxed) % cpu_count;
    for offset in 0..cpu_count {
        let cpu = (start + offset) % cpu_count;
        if cpu == current || !allowed.contains(cpu) || !online_mask().contains(cpu) {
            continue;
        }
        let already_pending = SCHEDULER_WAKE_PENDING[cpu].swap(true, Ordering::AcqRel);
        let mut sent_ipi = false;
        if !already_pending && crate::task::processor_is_idle(cpu) {
            if let Err(error) = crate::arch::smp::send_ipi(cpu) {
                SCHEDULER_WAKE_PENDING[cpu].store(false, Ordering::Release);
                panic!("scheduler wake IPI to CPU {cpu} failed: {error:#x}");
            }
            sent_ipi = true;
        }
        crate::task::record_smp_cpu_probe_scheduler_wake(true, sent_ipi);
        return;
    }
    crate::task::record_smp_cpu_probe_scheduler_wake(false, false);
}

pub fn take_scheduler_wake(cpu: CpuId) -> bool {
    SCHEDULER_WAKE_PENDING[cpu].swap(false, Ordering::AcqRel)
}

pub fn start_parked_secondaries() {
    let topology = topology();
    CPU_BOOT_LOCALS[0]
        .state
        .store(CPU_STATE_ONLINE, Ordering::Release);

    if topology.possible_count() > 1 {
        crate::arch::smp::validate_startup_extensions()
            .unwrap_or_else(|reason| panic!("SMP startup transport unavailable: {reason}"));
        for logical_id in 1..topology.possible_count() {
            let local = &CPU_BOOT_LOCALS[logical_id];
            local
                .state
                .store(CPU_STATE_START_REQUESTED, Ordering::Release);
            let hardware_id = topology.hardware_id(logical_id);
            if let Err(error) = crate::arch::smp::start_secondary(logical_id, hardware_id) {
                local.startup_error.store(error, Ordering::Relaxed);
                local.state.store(CPU_STATE_FAILED, Ordering::Release);
                panic!(
                    "failed to start logical CPU {logical_id} hardware CPU {hardware_id}: {error:#x}"
                );
            }
        }
        wait_for_online_barrier();
    }

    log_online_cpus();
    crate::arch::smp::enable_local_ipi();
    crate::arch::interrupt::enable_supervisor_interrupt();
    if topology.possible_count() > 1 {
        run_phase1_ipi_probe();
    }
    run_phase2_lock_irq_probe();
    run_tlb_transport_probe();
}

fn wait_for_online_barrier() {
    let expected = topology().possible_mask();
    let start = crate::timer::get_time();
    let timeout = crate::config::clock_freq().saturating_mul(2);
    loop {
        let observed = online_mask();
        if observed == expected {
            return;
        }
        for logical_id in 1..topology().possible_count() {
            if CPU_BOOT_LOCALS[logical_id].state.load(Ordering::Acquire) == CPU_STATE_FAILED {
                let error = CPU_BOOT_LOCALS[logical_id]
                    .startup_error
                    .load(Ordering::Relaxed);
                panic!("logical CPU {logical_id} failed during early startup: {error:#x}");
            }
        }
        if crate::timer::get_time().wrapping_sub(start) >= timeout {
            for logical_id in 1..topology().possible_count() {
                if !observed.contains(logical_id) {
                    CPU_BOOT_LOCALS[logical_id]
                        .startup_error
                        .store(STARTUP_ERROR_TIMEOUT, Ordering::Relaxed);
                    CPU_BOOT_LOCALS[logical_id]
                        .state
                        .store(CPU_STATE_FAILED, Ordering::Release);
                }
            }
            panic!(
                "SMP online barrier timeout: expected={:#x} observed={:#x}",
                expected.bits(),
                observed.bits()
            );
        }
        core::hint::spin_loop();
    }
}

fn log_online_cpus() {
    for logical_id in 0..topology().possible_count() {
        let hardware_id = topology().hardware_id(logical_id);
        let (stack_bottom, stack_top) = crate::arch::hart::boot_stack_bounds_for(logical_id);
        info!(
            "cpu boot: logical={} hw_id={} stack={:#x}..{:#x} state={} local={:#x} processor={:#x} mmu={:#x}",
            logical_id,
            hardware_id,
            stack_bottom,
            stack_top,
            CPU_BOOT_LOCALS[logical_id].state_name(),
            &CPU_LOCALS[logical_id] as *const CpuLocal as usize,
            crate::task::processor_slot_ptr(logical_id),
            CPU_LOCALS[logical_id].mmu_ptr(),
        );
    }
    let online = online_mask();
    info!(
        "smp online: mask={:#x} count={}",
        online.bits(),
        online.count()
    );
}

fn reset_phase1_ipi_probe() {
    for logical_id in 0..topology().possible_count() {
        PROBE_COMMAND_SEQ[logical_id].store(0, Ordering::Relaxed);
        PROBE_COMMAND_DONE[logical_id].store(0, Ordering::Relaxed);
        PROBE_COMMAND_TARGET[logical_id].store(0, Ordering::Relaxed);
        PROBE_RECEIVE_EXPECTED[logical_id].store(0, Ordering::Relaxed);
        PROBE_RECEIVE_SEEN[logical_id].store(0, Ordering::Relaxed);
        PROBE_SENT[logical_id].store(0, Ordering::Relaxed);
        PROBE_RECEIVED[logical_id].store(0, Ordering::Relaxed);
    }
    PROBE_FAILURES.store(0, Ordering::Relaxed);
    PROBE_DUPLICATES.store(0, Ordering::Relaxed);
    PROBE_UNEXPECTED.store(0, Ordering::Relaxed);
}

fn run_phase1_ipi_probe() {
    reset_phase1_ipi_probe();
    PROBE_ACTIVE.store(true, Ordering::Release);
    let cpu_count = topology().possible_count();
    let mut sequence = 0usize;

    for _round in 0..PHASE1_IPI_ROUNDS {
        for sender in 0..cpu_count {
            for target in 0..cpu_count {
                if sender == target {
                    continue;
                }
                sequence += 1;
                PROBE_RECEIVE_EXPECTED[target].store(sequence, Ordering::Release);
                if sender == 0 {
                    send_probe_ipi(sender, target)
                        .unwrap_or_else(|error| panic!("boot IPI send failed: {error:#x}"));
                } else {
                    PROBE_COMMAND_TARGET[sender].store(target, Ordering::Relaxed);
                    PROBE_COMMAND_SEQ[sender].store(sequence, Ordering::Release);
                    crate::arch::smp::send_ipi(sender).unwrap_or_else(|error| {
                        panic!("IPI command send to logical CPU {sender} failed: {error:#x}")
                    });
                    wait_for_probe_value(&PROBE_COMMAND_DONE[sender], sequence, "command");
                }
                wait_for_probe_value(&PROBE_RECEIVE_SEEN[target], sequence, "receive");
            }
        }
    }

    PROBE_ACTIVE.store(false, Ordering::Release);
    let expected_per_cpu = PHASE1_IPI_ROUNDS * (cpu_count - 1);
    let sent =
        core::array::from_fn::<_, MAX_CPUS, _>(|cpu| PROBE_SENT[cpu].load(Ordering::Acquire));
    let received =
        core::array::from_fn::<_, MAX_CPUS, _>(|cpu| PROBE_RECEIVED[cpu].load(Ordering::Acquire));
    for logical_id in 0..cpu_count {
        assert_eq!(
            sent[logical_id], expected_per_cpu,
            "Phase 1 IPI sent count mismatch on CPU {logical_id}"
        );
        assert_eq!(
            received[logical_id], expected_per_cpu,
            "Phase 1 IPI receive count mismatch on CPU {logical_id}"
        );
    }
    let failures = PROBE_FAILURES.load(Ordering::Acquire);
    let duplicates = PROBE_DUPLICATES.load(Ordering::Acquire);
    let unexpected = PROBE_UNEXPECTED.load(Ordering::Acquire);
    assert_eq!(failures, 0, "Phase 1 IPI transport failure");
    assert_eq!(duplicates, 0, "Phase 1 duplicate IPI delivery");
    assert_eq!(unexpected, 0, "Phase 1 unexpected IPI delivery");
    info!(
        "smp boot-ipi: rounds={} cpus={} expected_per_cpu={} sent={:?} received={:?} failures={} duplicates={} unexpected={}",
        PHASE1_IPI_ROUNDS,
        cpu_count,
        expected_per_cpu,
        &sent[..cpu_count],
        &received[..cpu_count],
        failures,
        duplicates,
        unexpected,
    );
}

fn send_probe_ipi(sender: CpuId, target: CpuId) -> Result<(), usize> {
    match crate::arch::smp::send_ipi(target) {
        Ok(()) => {
            PROBE_SENT[sender].fetch_add(1, Ordering::Relaxed);
            Ok(())
        }
        Err(error) => {
            PROBE_FAILURES.fetch_add(1, Ordering::Relaxed);
            Err(error)
        }
    }
}

fn wait_for_probe_value(value: &AtomicUsize, expected: usize, what: &str) {
    let start = crate::timer::get_time();
    let timeout = (crate::config::clock_freq() / 4).max(1);
    while value.load(Ordering::Acquire) != expected {
        if crate::timer::get_time().wrapping_sub(start) >= timeout {
            panic!("Phase 1 IPI {what} timeout waiting for sequence {expected}");
        }
        core::hint::spin_loop();
    }
}

pub fn handle_ipi() {
    let logical_id = current_id();
    let handled_tlb = crate::arch::smp::handle_tlb_ipi();
    let handled_scheduler_wake = take_scheduler_wake(logical_id);
    let handled_tlb_cross_command = TLB_CROSS_PROBE_ACTIVE.load(Ordering::Acquire)
        && TLB_CROSS_COMMAND_PENDING[logical_id].swap(false, Ordering::AcqRel);
    if handled_tlb_cross_command {
        TLB_CROSS_RUN_PENDING[logical_id].store(true, Ordering::Release);
    }
    if PHASE2_PROBE_ACTIVE.load(Ordering::Acquire) {
        PHASE2_PROBE_PENDING[logical_id].store(true, Ordering::Release);
        return;
    }
    if !PROBE_ACTIVE.load(Ordering::Acquire) {
        if handled_tlb
            || handled_scheduler_wake
            || handled_tlb_cross_command
            || TLB_CROSS_PROBE_ACTIVE.load(Ordering::Acquire)
            || scheduler_aps_active()
        {
            // An idle CPU may consume the pending flag immediately before the
            // already-issued interrupt is delivered. The interrupt is then a
            // harmless coalesced scheduler/TLB wake, not a boot-probe
            // violation.
            return;
        }
        PROBE_UNEXPECTED.fetch_add(1, Ordering::Relaxed);
        return;
    }
    let command = PROBE_COMMAND_SEQ[logical_id].load(Ordering::Acquire);
    if command != PROBE_COMMAND_DONE[logical_id].load(Ordering::Relaxed) {
        let target = PROBE_COMMAND_TARGET[logical_id].load(Ordering::Relaxed);
        let _ = send_probe_ipi(logical_id, target);
        PROBE_COMMAND_DONE[logical_id].store(command, Ordering::Release);
        return;
    }

    let expected = PROBE_RECEIVE_EXPECTED[logical_id].load(Ordering::Acquire);
    let previous = PROBE_RECEIVE_SEEN[logical_id].swap(expected, Ordering::AcqRel);
    if expected == 0 {
        PROBE_UNEXPECTED.fetch_add(1, Ordering::Relaxed);
    } else if previous == expected {
        PROBE_DUPLICATES.fetch_add(1, Ordering::Relaxed);
    } else {
        PROBE_RECEIVED[logical_id].fetch_add(1, Ordering::Relaxed);
    }
}

fn run_tlb_transport_probe() {
    let current = current_id();
    let targets = CpuMask::from_bits(online_mask().bits() & !CpuMask::single(current).bits());
    if targets.bits() == 0 {
        info!(
            "smp tlb-transport: backend={} requests=0 targets_per_request=0 completions=0 failures=0",
            crate::arch::smp::tlb_backend_name()
        );
        return;
    }

    crate::arch::mm::flush_tlb_range(crate::config::PAGE_SIZE * 4, crate::config::PAGE_SIZE);
    crate::arch::smp::remote_tlb_flush(
        targets,
        crate::config::PAGE_SIZE * 4,
        crate::config::PAGE_SIZE,
    )
    .unwrap_or_else(|error| panic!("range TLB transport probe failed: {error:#x}"));
    crate::arch::mm::flush_tlb_range(0, usize::MAX);
    crate::arch::smp::remote_tlb_flush(targets, 0, usize::MAX)
        .unwrap_or_else(|error| panic!("full TLB transport probe failed: {error:#x}"));
    info!(
        "smp tlb-transport: backend={} requests=2 targets_per_request={} completions={} failures=0",
        crate::arch::smp::tlb_backend_name(),
        targets.count(),
        targets.count() * 2,
    );
    run_tlb_cross_probe();
}

fn run_tlb_cross_probe() {
    if topology().possible_count() < 2 {
        info!("smp tlb-cross: participants=1 completions=0 failures=0");
        return;
    }

    TLB_CROSS_PROBE_GO.store(false, Ordering::Relaxed);
    TLB_CROSS_READY.store(CpuMask::empty(), Ordering::Relaxed);
    TLB_CROSS_DONE.store(CpuMask::empty(), Ordering::Relaxed);
    for cpu in 0..2 {
        TLB_CROSS_COMMAND_PENDING[cpu].store(false, Ordering::Relaxed);
        TLB_CROSS_RUN_PENDING[cpu].store(false, Ordering::Relaxed);
    }
    TLB_CROSS_PROBE_ACTIVE.store(true, Ordering::Release);
    TLB_CROSS_COMMAND_PENDING[1].store(true, Ordering::Release);
    crate::arch::smp::send_ipi(1)
        .unwrap_or_else(|error| panic!("TLB cross-probe command IPI failed: {error:#x}"));

    wait_for_tlb_cross_mask(&TLB_CROSS_READY, CpuMask::single(1), "ready");
    TLB_CROSS_PROBE_GO.store(true, Ordering::Release);
    run_tlb_cross_probe_on_cpu(0);
    wait_for_tlb_cross_mask(&TLB_CROSS_DONE, CpuMask::first(2), "completion");
    TLB_CROSS_PROBE_ACTIVE.store(false, Ordering::Release);
    info!("smp tlb-cross: participants=2 completions=2 failures=0");
}

fn run_tlb_cross_probe_on_cpu(logical_id: CpuId) {
    assert_eq!(current_id(), logical_id, "TLB cross probe ran on wrong CPU");
    if logical_id == 1 {
        TLB_CROSS_READY.insert(logical_id, Ordering::Release);
        while !TLB_CROSS_PROBE_GO.load(Ordering::Acquire) {
            core::hint::spin_loop();
        }
    }
    let target = 1 - logical_id;
    let irq_guard = LocalIrqGuard::disable();
    assert!(
        !crate::arch::interrupt::supervisor_interrupt_enabled(),
        "TLB cross probe failed to disable local interrupts"
    );
    crate::arch::smp::remote_tlb_flush(
        CpuMask::single(target),
        crate::config::PAGE_SIZE * 8,
        crate::config::PAGE_SIZE,
    )
    .unwrap_or_else(|error| {
        panic!("TLB cross probe from CPU {logical_id} to CPU {target} failed: {error:#x}")
    });
    drop(irq_guard);
    TLB_CROSS_DONE.insert(logical_id, Ordering::Release);
}

fn wait_for_tlb_cross_mask(value: &AtomicCpuMask, expected: CpuMask, what: &str) {
    let start = crate::timer::get_time();
    let timeout = crate::config::clock_freq().saturating_mul(2);
    loop {
        let observed = value.load(Ordering::Acquire);
        if observed.bits() & expected.bits() == expected.bits() {
            return;
        }
        if crate::timer::get_time().wrapping_sub(start) >= timeout {
            panic!(
                "TLB cross-probe {what} timeout: expected={:#x} observed={:#x}",
                expected.bits(),
                observed.bits()
            );
        }
        core::hint::spin_loop();
    }
}

fn run_phase2_lock_irq_probe() {
    let cpu_count = topology().possible_count();
    *PHASE2_LOCK_COUNTER.lock() = 0;
    PHASE2_PROBE_DONE.store(CpuMask::empty(), Ordering::Relaxed);
    PHASE2_PROBE_FAILURES.store(0, Ordering::Relaxed);
    PHASE2_PROBE_GO.store(false, Ordering::Relaxed);
    for logical_id in 0..cpu_count {
        PHASE2_PROBE_PENDING[logical_id].store(false, Ordering::Relaxed);
        PHASE2_IRQ_RESTORED[logical_id].store(false, Ordering::Relaxed);
        PHASE2_PROBE_PERF[logical_id].reset();
    }

    PHASE2_PROBE_ACTIVE.store(true, Ordering::Release);
    for logical_id in 1..cpu_count {
        crate::arch::smp::send_ipi(logical_id).unwrap_or_else(|error| {
            panic!("Phase 2 probe IPI to logical CPU {logical_id} failed: {error:#x}")
        });
    }
    PHASE2_PROBE_GO.store(true, Ordering::Release);
    run_phase2_probe_on_cpu(0);

    let expected_mask = CpuMask::first(cpu_count);
    let start = crate::timer::get_time();
    let timeout = crate::config::clock_freq().saturating_mul(2);
    while PHASE2_PROBE_DONE.load(Ordering::Acquire) != expected_mask {
        if crate::timer::get_time().wrapping_sub(start) >= timeout {
            panic!(
                "Phase 2 lock/IRQ probe timeout: expected={:#x} observed={:#x}",
                expected_mask.bits(),
                PHASE2_PROBE_DONE.load(Ordering::Acquire).bits()
            );
        }
        core::hint::spin_loop();
    }
    PHASE2_PROBE_ACTIVE.store(false, Ordering::Release);

    let total = *PHASE2_LOCK_COUNTER.lock();
    let expected_total = PHASE2_LOCK_INCREMENTS * cpu_count;
    assert_eq!(
        total, expected_total,
        "Phase 2 shared spin-lock count mismatch"
    );
    let restored = core::array::from_fn::<_, MAX_CPUS, _>(|cpu| {
        PHASE2_IRQ_RESTORED[cpu].load(Ordering::Acquire)
    });
    assert!(
        restored[..cpu_count].iter().all(|value| *value),
        "Phase 2 local IRQ nesting/restore mismatch"
    );
    let failures = PHASE2_PROBE_FAILURES.load(Ordering::Acquire);
    assert_eq!(failures, 0, "Phase 2 lock/IRQ invariant failure");
    info!(
        "smp lock-irq: increments_per_cpu={} cpus={} total={} expected={} irq_restored={:?} failures={}",
        PHASE2_LOCK_INCREMENTS,
        cpu_count,
        total,
        expected_total,
        &restored[..cpu_count],
        failures,
    );
    let contended = core::array::from_fn::<_, MAX_CPUS, _>(|cpu| {
        PHASE2_PROBE_PERF[cpu].contended.load(Ordering::Acquire)
    });
    let spins = core::array::from_fn::<_, MAX_CPUS, _>(|cpu| {
        PHASE2_PROBE_PERF[cpu].spins.load(Ordering::Acquire)
    });
    let max_wait = core::array::from_fn::<_, MAX_CPUS, _>(|cpu| {
        PHASE2_PROBE_PERF[cpu]
            .max_wait_ticks
            .load(Ordering::Acquire)
    });
    let max_hold = core::array::from_fn::<_, MAX_CPUS, _>(|cpu| {
        PHASE2_PROBE_PERF[cpu]
            .max_hold_ticks
            .load(Ordering::Acquire)
    });
    let elapsed = core::array::from_fn::<_, MAX_CPUS, _>(|cpu| {
        PHASE2_PROBE_PERF[cpu].elapsed_ticks.load(Ordering::Acquire)
    });
    info!(
        "smp lock-perf: contended={:?} spins={:?} max_wait_ticks={:?} max_hold_ticks={:?} elapsed_ticks={:?}",
        &contended[..cpu_count],
        &spins[..cpu_count],
        &max_wait[..cpu_count],
        &max_hold[..cpu_count],
        &elapsed[..cpu_count],
    );
}

fn run_phase2_probe_on_cpu(logical_id: CpuId) {
    let current = current();
    if current.logical_id() != logical_id
        || current.hardware_id() != topology().hardware_id(logical_id)
        || !crate::task::current_processor_is_empty()
    {
        PHASE2_PROBE_FAILURES.fetch_add(1, Ordering::Relaxed);
    }
    if !crate::arch::interrupt::supervisor_interrupt_enabled() {
        PHASE2_PROBE_FAILURES.fetch_add(1, Ordering::Relaxed);
    }
    {
        let outer = LocalIrqGuard::disable();
        if !outer.was_enabled() || crate::arch::interrupt::supervisor_interrupt_enabled() {
            PHASE2_PROBE_FAILURES.fetch_add(1, Ordering::Relaxed);
        }
        {
            let inner = LocalIrqGuard::disable();
            if inner.was_enabled() || crate::arch::interrupt::supervisor_interrupt_enabled() {
                PHASE2_PROBE_FAILURES.fetch_add(1, Ordering::Relaxed);
            }
        }
        if crate::arch::interrupt::supervisor_interrupt_enabled() {
            PHASE2_PROBE_FAILURES.fetch_add(1, Ordering::Relaxed);
        }
    }
    let restored = crate::arch::interrupt::supervisor_interrupt_enabled();
    PHASE2_IRQ_RESTORED[logical_id].store(restored, Ordering::Release);
    if !restored {
        PHASE2_PROBE_FAILURES.fetch_add(1, Ordering::Relaxed);
    }

    while !PHASE2_PROBE_GO.load(Ordering::Acquire) {
        core::hint::spin_loop();
    }
    let loop_start = crate::timer::get_time();
    let mut contended = 0usize;
    let mut spins_total = 0usize;
    let mut max_wait_ticks = 0usize;
    let mut max_hold_ticks = 0usize;
    for _ in 0..PHASE2_LOCK_INCREMENTS {
        let wait_start = crate::timer::get_time();
        let (mut counter, spins) = PHASE2_LOCK_COUNTER.lock_counted();
        let acquired = crate::timer::get_time();
        *counter += 1;
        drop(counter);
        let released = crate::timer::get_time();
        contended += usize::from(spins != 0);
        spins_total = spins_total.saturating_add(spins);
        max_wait_ticks = max_wait_ticks.max(acquired.wrapping_sub(wait_start));
        max_hold_ticks = max_hold_ticks.max(released.wrapping_sub(acquired));
    }
    let elapsed_ticks = crate::timer::get_time().wrapping_sub(loop_start);
    let perf = &PHASE2_PROBE_PERF[logical_id];
    perf.contended.store(contended, Ordering::Relaxed);
    perf.spins.store(spins_total, Ordering::Relaxed);
    perf.max_wait_ticks.store(max_wait_ticks, Ordering::Relaxed);
    perf.max_hold_ticks.store(max_hold_ticks, Ordering::Relaxed);
    perf.elapsed_ticks.store(elapsed_ticks, Ordering::Release);
    PHASE2_PROBE_DONE.insert(logical_id, Ordering::Release);
}

/// Runs boot-only work from a parked secondary's normal kernel context.
/// IPI handlers only publish the command, keeping long probes out of IRQ context.
pub fn run_pending_parked_probe(logical_id: CpuId) {
    if PHASE2_PROBE_PENDING[logical_id].swap(false, Ordering::AcqRel) {
        run_phase2_probe_on_cpu(logical_id);
    }
    if TLB_CROSS_RUN_PENDING[logical_id].swap(false, Ordering::AcqRel) {
        run_tlb_cross_probe_on_cpu(logical_id);
    }
}
