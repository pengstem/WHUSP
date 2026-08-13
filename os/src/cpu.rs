use crate::config::MAX_CPUS;
use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, AtomicUsize, Ordering};
use fdt::Fdt;
use log::info;

pub type CpuId = usize;

// Keep all external device interrupts on the boot scheduler CPU until Phase 4
// has made each driver queue safe for distributed interrupt handling.
pub const EXTERNAL_IRQ_OWNER_CPU: CpuId = 0;

// The global sleep heap and any enabled compatibility timer heaps have one
// expiry owner. Every CPU still programs its local timer for preemption.
pub const TIMER_EXPIRY_OWNER_CPU: CpuId = 0;

const CPU_STATE_OFFLINE: u8 = 0;
const CPU_STATE_START_REQUESTED: u8 = 1;
const CPU_STATE_EARLY: u8 = 2;
const CPU_STATE_ONLINE: u8 = 3;
const CPU_STATE_FAILED: u8 = 4;
const STARTUP_ERROR_NONE: usize = 0;
const STARTUP_ERROR_ID_MISMATCH: usize = 1;
const STARTUP_ERROR_BAD_TRANSITION: usize = 2;
const STARTUP_ERROR_TIMEOUT: usize = 3;
const REMOTE_SYNC_NONE: usize = 0;
const REMOTE_SYNC_MEMORY: usize = 1;
const REMOTE_SYNC_INSTRUCTION: usize = 2;

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
    return_tlb_dirty: AtomicBool,
    observed_address_space_id: AtomicUsize,
    observed_tlb_generation: AtomicUsize,
    observed_instruction_space_id: AtomicUsize,
    observed_instruction_generation: AtomicUsize,
}

impl CpuMmuFastState {
    const fn new() -> Self {
        Self {
            last_return_user_token: AtomicUsize::new(0),
            return_tlb_dirty: AtomicBool::new(true),
            observed_address_space_id: AtomicUsize::new(0),
            observed_tlb_generation: AtomicUsize::new(0),
            observed_instruction_space_id: AtomicUsize::new(0),
            observed_instruction_generation: AtomicUsize::new(0),
        }
    }

    pub(crate) fn swap_last_return_user_token(&self, token: usize) -> usize {
        self.last_return_user_token.swap(token, Ordering::Relaxed)
    }

    pub(crate) fn take_return_tlb_dirty(&self) -> bool {
        self.return_tlb_dirty.swap(false, Ordering::Relaxed)
    }

    pub(crate) fn mark_return_tlb_dirty(&self) {
        self.return_tlb_dirty.store(true, Ordering::Relaxed);
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

    pub(crate) fn instruction_barrier_required(&self, id: usize, generation: usize) -> bool {
        let previous_id = self
            .observed_instruction_space_id
            .swap(id, Ordering::Relaxed);
        let previous_generation = self
            .observed_instruction_generation
            .swap(generation, Ordering::Relaxed);
        assert!(
            previous_id != id || previous_generation <= generation,
            "address-space instruction generation regressed: id={id} previous={previous_generation} current={generation}",
        );
        previous_id != id || previous_generation < generation
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

    pub const fn contains(self, cpu: CpuId) -> bool {
        cpu < u64::BITS as usize && self.0 & (1u64 << cpu) != 0
    }

    pub const fn count(self) -> usize {
        self.0.count_ones() as usize
    }

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

    pub fn swap(&self, mask: CpuMask, order: Ordering) -> CpuMask {
        CpuMask(self.0.swap(mask.bits(), order))
    }

    pub fn insert(&self, cpu: CpuId, order: Ordering) {
        assert!(cpu < MAX_CPUS, "CPU ID exceeds MAX_CPUS");
        self.0.fetch_or(1u64 << cpu, order);
    }

    pub fn fetch_insert(&self, cpu: CpuId, order: Ordering) -> CpuMask {
        assert!(cpu < MAX_CPUS, "CPU ID exceeds MAX_CPUS");
        CpuMask(self.0.fetch_or(1u64 << cpu, order))
    }

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
            if !cpu_is_kernel_compatible(cpu) {
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

    pub fn hardware_id(&self, cpu: CpuId) -> usize {
        assert!(cpu < self.possible_count, "logical CPU ID is not possible");
        self.logical_to_hw_id[cpu]
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

fn cpu_is_kernel_compatible(cpu: fdt::standard_nodes::Cpu<'_, '_>) -> bool {
    if !cpu_is_enabled(cpu) {
        return false;
    }

    #[cfg(target_arch = "riscv64")]
    {
        // CONTEXT: A RISC-V /cpus node may describe management harts that
        // cannot run an S-mode paged kernel. JH7110 hart 0 is such a SiFive S7
        // hart, while its U74 application harts advertise an riscv,sv* MMU.
        let Some(mmu_type) = cpu.property("mmu-type") else {
            return false;
        };
        let Ok(mmu_type) = core::str::from_utf8(mmu_type.value) else {
            return false;
        };
        if !mmu_type.trim_end_matches('\0').starts_with("riscv,sv") {
            return false;
        }

        // CONTEXT: The 2023 VisionFive 2 vendor DTB incorrectly marks its
        // management hart as a U74 with an Sv39 MMU. Its legacy riscv,isa
        // value still identifies the hart as U-only (rv64imacu), unlike the
        // S/U-capable application harts. Modern bindings omit both privilege
        // letters, so only reject the contradictory U-without-S form.
        if let Some(isa) = cpu.property("riscv,isa") {
            let Ok(isa) = core::str::from_utf8(isa.value) else {
                return false;
            };
            let base_isa = isa
                .trim_end_matches('\0')
                .split('_')
                .next()
                .unwrap_or_default();
            let base_extensions = base_isa
                .strip_prefix("rv64")
                .or_else(|| base_isa.strip_prefix("rv32"))
                .unwrap_or_default();
            if base_extensions.contains('u') && !base_extensions.contains('s') {
                return false;
            }
        }
        return true;
    }

    #[cfg(not(target_arch = "riscv64"))]
    true
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

static SCHEDULER_APS_ACTIVE: AtomicBool = AtomicBool::new(false);
static SCHEDULER_ACTIVE_CPUS: AtomicCpuMask = AtomicCpuMask::new(CpuMask::empty());
static SCHEDULER_ACTIVE_LOGGED: AtomicBool = AtomicBool::new(false);
static SCHEDULER_PARALLEL_PRESSURE: AtomicUsize = AtomicUsize::new(0);
static SCHEDULER_PARALLEL_LATCHED: AtomicBool = AtomicBool::new(false);
static SCHEDULER_MAX_RUNNING_TASKS: AtomicUsize = AtomicUsize::new(0);
const SCHEDULER_NO_CURRENT_PRIORITY: usize = usize::MAX;
const SCHEDULER_PARALLEL_PRESSURE_LIMIT: usize = 1024;

#[repr(C, align(64))]
struct SchedulerCpuSignals {
    wake_pending: AtomicBool,
    need_resched: AtomicBool,
    current_rt_priority: AtomicUsize,
}

impl SchedulerCpuSignals {
    const fn new() -> Self {
        Self {
            wake_pending: AtomicBool::new(false),
            need_resched: AtomicBool::new(false),
            current_rt_priority: AtomicUsize::new(SCHEDULER_NO_CURRENT_PRIORITY),
        }
    }
}

static SCHEDULER_SIGNALS: [SchedulerCpuSignals; MAX_CPUS] =
    [const { SchedulerCpuSignals::new() }; MAX_CPUS];

struct RemoteSyncRequest {
    active: AtomicBool,
    sequence: AtomicUsize,
    action: AtomicUsize,
    remaining: AtomicCpuMask,
}

impl RemoteSyncRequest {
    const fn new() -> Self {
        Self {
            active: AtomicBool::new(false),
            sequence: AtomicUsize::new(0),
            action: AtomicUsize::new(REMOTE_SYNC_NONE),
            remaining: AtomicCpuMask::new(CpuMask::empty()),
        }
    }
}

static REMOTE_SYNC_REQUESTS: [RemoteSyncRequest; MAX_CPUS] =
    [const { RemoteSyncRequest::new() }; MAX_CPUS];
static REMOTE_SYNC_PENDING_SOURCES: [AtomicCpuMask; MAX_CPUS] =
    [const { AtomicCpuMask::new(CpuMask::empty()) }; MAX_CPUS];
static REMOTE_SYNC_OBSERVED_SEQUENCE: [[AtomicUsize; MAX_CPUS]; MAX_CPUS] =
    [const { [const { AtomicUsize::new(0) }; MAX_CPUS] }; MAX_CPUS];

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
        SCHEDULER_SIGNALS[cpu]
            .wake_pending
            .store(true, Ordering::Release);
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

fn wake_should_preempt(cpu: CpuId, wakee_rt_priority: usize) -> bool {
    let current_rt_priority = SCHEDULER_SIGNALS[cpu]
        .current_rt_priority
        .load(Ordering::Acquire);
    wakee_rt_priority != 0
        && (current_rt_priority == SCHEDULER_NO_CURRENT_PRIORITY
            || current_rt_priority == 0
            || wakee_rt_priority > current_rt_priority)
}

fn request_need_resched(cpu: CpuId, needed: bool) -> bool {
    let newly_set = needed
        && !SCHEDULER_SIGNALS[cpu]
            .need_resched
            .swap(true, Ordering::AcqRel);
    crate::perf::record_scheduler_need_resched(needed, newly_set);
    newly_set
}

fn wake_scheduler_cpu_remote(cpu: CpuId, wakee_rt_priority: Option<usize>) -> bool {
    let signals = &SCHEDULER_SIGNALS[cpu];
    let already_pending = signals.wake_pending.swap(true, Ordering::AcqRel);
    let need_resched = wakee_rt_priority
        .map(|priority| request_need_resched(cpu, wake_should_preempt(cpu, priority)))
        .unwrap_or(false);
    let mut sent_ipi = false;
    if need_resched || (!already_pending && crate::task::processor_is_idle(cpu)) {
        if let Err(error) = crate::arch::smp::send_ipi(cpu) {
            if !already_pending {
                signals.wake_pending.store(false, Ordering::Release);
            }
            if need_resched {
                signals.need_resched.store(false, Ordering::Release);
            }
            panic!("scheduler wake IPI to CPU {cpu} failed: {error:#x}");
        }
        sent_ipi = true;
    }
    if need_resched && sent_ipi {
        crate::perf::record_scheduler_need_resched_ipi();
    }
    sent_ipi
}

pub fn wake_scheduler_cpu_exact(cpu: CpuId) -> bool {
    if !scheduler_aps_active() {
        return false;
    }
    let current = current_id();
    if cpu == current || !online_mask().contains(cpu) {
        return false;
    }
    wake_scheduler_cpu_remote(cpu, None)
}

pub fn wake_scheduler_cpu_for_task(cpu: CpuId, wakee_rt_priority: usize) -> bool {
    if !scheduler_aps_active() {
        return false;
    }
    let current = current_id();
    if !online_mask().contains(cpu) {
        return false;
    }
    if cpu == current {
        request_need_resched(cpu, wake_should_preempt(cpu, wakee_rt_priority));
        return false;
    }
    wake_scheduler_cpu_remote(cpu, Some(wakee_rt_priority))
}

pub fn wake_scheduler_cpu(allowed: CpuMask) {
    if !scheduler_aps_active() {
        return;
    }
    let current = current_id();
    let cpu_count = topology().possible_count();
    let eligible = CpuMask::from_bits(allowed.bits() & online_mask().bits());
    for offset in 1..cpu_count {
        let cpu = (current + offset) % cpu_count;
        if eligible.contains(cpu) {
            wake_scheduler_cpu_remote(cpu, None);
            return;
        }
    }
}

pub(crate) fn request_scheduler_preemption(targets: CpuMask) {
    if !scheduler_aps_active() {
        return;
    }
    let current = current_id();
    let targets = CpuMask::from_bits(targets.bits() & online_mask().bits());
    for cpu in 0..topology().possible_count() {
        if cpu == current || !targets.contains(cpu) {
            continue;
        }
        SCHEDULER_SIGNALS[cpu]
            .wake_pending
            .store(true, Ordering::Release);
        crate::arch::smp::send_ipi(cpu).unwrap_or_else(|error| {
            panic!("scheduler preemption IPI to CPU {cpu} failed: {error:#x}")
        });
    }
}

pub fn take_scheduler_wake(cpu: CpuId) -> bool {
    SCHEDULER_SIGNALS[cpu]
        .wake_pending
        .swap(false, Ordering::AcqRel)
        || crate::task::remote_wake_pending(cpu)
}

pub(crate) fn scheduler_publish_current_priority(rt_priority: usize) {
    assert!(rt_priority <= 99, "invalid current RT priority");
    SCHEDULER_SIGNALS[current_id()]
        .current_rt_priority
        .store(rt_priority, Ordering::Release);
}

pub(crate) fn scheduler_clear_current_priority() {
    SCHEDULER_SIGNALS[current_id()]
        .current_rt_priority
        .store(SCHEDULER_NO_CURRENT_PRIORITY, Ordering::Release);
}

pub(crate) fn scheduler_low_concurrency_mode() -> bool {
    let online_count = online_mask().count();
    if online_count <= 1 {
        return false;
    }
    let mut running = 0usize;
    for cpu in 0..topology().possible_count() {
        if SCHEDULER_SIGNALS[cpu]
            .current_rt_priority
            .load(Ordering::Acquire)
            != SCHEDULER_NO_CURRENT_PRIORITY
        {
            running += 1;
        }
    }
    SCHEDULER_MAX_RUNNING_TASKS.fetch_max(running, Ordering::AcqRel);
    if SCHEDULER_PARALLEL_LATCHED.load(Ordering::Acquire) {
        return false;
    }
    if running >= online_count.min(4) {
        let pressure = SCHEDULER_PARALLEL_PRESSURE
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |value| {
                Some(
                    value
                        .saturating_add(1)
                        .min(SCHEDULER_PARALLEL_PRESSURE_LIMIT),
                )
            })
            .unwrap_or(SCHEDULER_PARALLEL_PRESSURE_LIMIT);
        if pressure + 1 >= SCHEDULER_PARALLEL_PRESSURE_LIMIT {
            SCHEDULER_PARALLEL_LATCHED.store(true, Ordering::Release);
            return false;
        }
    } else if running <= 2 {
        let _ = SCHEDULER_PARALLEL_PRESSURE.fetch_update(
            Ordering::AcqRel,
            Ordering::Acquire,
            |value| Some(value.saturating_sub(1)),
        );
    }
    running <= 2
}

pub(crate) fn scheduler_surplus_capacity_mode() -> bool {
    SCHEDULER_PARALLEL_LATCHED.load(Ordering::Acquire)
        && SCHEDULER_MAX_RUNNING_TASKS.load(Ordering::Acquire) < online_mask().count()
}

pub(crate) fn scheduler_need_resched(cpu: CpuId) -> bool {
    SCHEDULER_SIGNALS[cpu].need_resched.load(Ordering::Acquire)
}

pub(crate) fn take_scheduler_need_resched(cpu: CpuId) -> bool {
    let needed = SCHEDULER_SIGNALS[cpu]
        .need_resched
        .swap(false, Ordering::AcqRel);
    if needed {
        crate::perf::record_scheduler_need_resched_consumed();
    }
    needed
}

pub(crate) fn synchronize_remote_memory(targets: CpuMask) -> Result<(), usize> {
    submit_remote_sync(targets, REMOTE_SYNC_MEMORY)
}

pub(crate) fn synchronize_remote_instruction(targets: CpuMask) -> Result<(), usize> {
    submit_remote_sync(targets, REMOTE_SYNC_INSTRUCTION)
}

fn submit_remote_sync(targets: CpuMask, action: usize) -> Result<(), usize> {
    assert!(
        matches!(action, REMOTE_SYNC_MEMORY | REMOTE_SYNC_INSTRUCTION),
        "invalid remote synchronization action {action}"
    );
    assert_eq!(
        targets.bits() & !online_mask().bits(),
        0,
        "remote synchronization targets an offline CPU"
    );
    let source = current_id();
    assert!(
        !targets.contains(source),
        "remote synchronization target mask contains the caller"
    );
    if targets.bits() == 0 {
        return Ok(());
    }

    crate::arch::mm::memory_barrier();
    let request = &REMOTE_SYNC_REQUESTS[source];
    assert!(
        !request.active.swap(true, Ordering::AcqRel),
        "CPU {source} started a nested remote synchronization"
    );
    assert_eq!(
        request.remaining.load(Ordering::Acquire).bits(),
        0,
        "CPU {source} reused an incomplete remote synchronization"
    );
    request.action.store(action, Ordering::Relaxed);
    let sequence = request.sequence.load(Ordering::Relaxed).wrapping_add(1);
    assert_ne!(sequence, 0, "remote synchronization sequence wrapped");
    request.sequence.store(sequence, Ordering::Release);

    let mut send_error = None;
    for target in 0..topology().possible_count() {
        if !targets.contains(target) {
            continue;
        }
        let old_remaining = request.remaining.fetch_insert(target, Ordering::AcqRel);
        assert!(
            !old_remaining.contains(target),
            "remote synchronization target {target} was already pending from {source}"
        );
        let old_sources =
            REMOTE_SYNC_PENDING_SOURCES[target].fetch_insert(source, Ordering::AcqRel);
        assert!(
            !old_sources.contains(source),
            "remote synchronization source {source} was already pending on {target}"
        );
        if old_sources.bits() == 0
            && let Err(error) = crate::arch::smp::send_ipi(target)
        {
            let pending =
                REMOTE_SYNC_PENDING_SOURCES[target].fetch_remove(source, Ordering::AcqRel);
            if pending.contains(source) {
                let remaining = request.remaining.fetch_remove(target, Ordering::AcqRel);
                assert!(remaining.contains(target));
            }
            send_error = Some(error);
            break;
        }
    }

    wait_for_remote_sync_completion(source, sequence, request);
    request.action.store(REMOTE_SYNC_NONE, Ordering::Relaxed);
    request.active.store(false, Ordering::Release);
    send_error.map_or(Ok(()), Err)
}

fn wait_for_remote_sync_completion(source: CpuId, sequence: usize, request: &RemoteSyncRequest) {
    let start = crate::timer::get_time();
    let timeout = crate::config::clock_freq().saturating_mul(2);
    loop {
        let remaining = request.remaining.load(Ordering::Acquire);
        if remaining.bits() == 0 {
            return;
        }
        // Close the cross-rendezvous deadlock when two IRQ-masked CPUs target
        // one another while holding unrelated process/address-space locks.
        handle_remote_sync_ipi();
        if crate::timer::get_time().wrapping_sub(start) >= timeout {
            panic!(
                "remote synchronization timeout: source={source} sequence={sequence} remaining={:#x}",
                remaining.bits()
            );
        }
        core::hint::spin_loop();
    }
}

pub(crate) fn handle_remote_sync_ipi() -> bool {
    let target = current_id();
    let mut handled = false;
    loop {
        let sources = REMOTE_SYNC_PENDING_SOURCES[target].swap(CpuMask::empty(), Ordering::AcqRel);
        if sources.bits() == 0 {
            return handled;
        }
        handled = true;
        for source in 0..topology().possible_count() {
            if !sources.contains(source) {
                continue;
            }
            assert_ne!(
                source, target,
                "remote synchronization targeted its source CPU"
            );
            let request = &REMOTE_SYNC_REQUESTS[source];
            assert!(
                request.active.load(Ordering::Acquire),
                "target {target} observed an inactive synchronization from {source}"
            );
            let sequence = request.sequence.load(Ordering::Acquire);
            let previous = REMOTE_SYNC_OBSERVED_SEQUENCE[target][source].load(Ordering::Relaxed);
            assert!(
                sequence > previous,
                "stale remote synchronization: source={source} target={target} sequence={sequence} previous={previous}"
            );
            match request.action.load(Ordering::Relaxed) {
                REMOTE_SYNC_MEMORY => crate::arch::mm::memory_barrier(),
                REMOTE_SYNC_INSTRUCTION => {
                    crate::arch::mm::memory_barrier();
                    crate::arch::mm::instruction_barrier();
                }
                action => {
                    panic!("target {target} observed invalid synchronization action {action}")
                }
            }
            REMOTE_SYNC_OBSERVED_SEQUENCE[target][source].store(sequence, Ordering::Release);
            let remaining = request.remaining.fetch_remove(target, Ordering::AcqRel);
            assert!(
                remaining.contains(target),
                "target {target} acknowledged a completed synchronization from {source}"
            );
        }
    }
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
}

pub fn handle_ipi() {
    let logical_id = current_id();
    handle_remote_sync_ipi();
    crate::arch::smp::handle_tlb_ipi();
    take_scheduler_wake(logical_id);
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
