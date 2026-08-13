use crate::DEV_NON_BLOCKING_ACCESS;
use crate::board::{BlockDeviceConfig, BlockDeviceImpl};
use crate::drivers::block_cache;
use crate::drivers::virtio::{VirtioHal, VirtioTransport, mmio_transport};
use crate::sync::{Condvar, SpinNoIrqLock};
use crate::task::schedule;
use alloc::collections::BTreeMap;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicUsize, Ordering};
use lazy_static::*;
use log::info;
use virtio_drivers::device::blk::{BlkReq, BlkResp, VirtIOBlk};

#[cfg(feature = "perf-counters")]
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct BlockIoStats {
    pub(crate) nonblocking_requested: usize,
    pub(crate) nb_read_submits: usize,
    pub(crate) nb_write_submits: usize,
    pub(crate) nb_read_waits: usize,
    pub(crate) nb_write_waits: usize,
    pub(crate) nb_read_completions: usize,
    pub(crate) nb_write_completions: usize,
    pub(crate) fallback_sync_reads: usize,
    pub(crate) fallback_sync_writes: usize,
    pub(crate) fallback_unsafe_reads: usize,
    pub(crate) fallback_unsafe_writes: usize,
    pub(crate) fallback_no_ready_reads: usize,
    pub(crate) fallback_no_ready_writes: usize,
    pub(crate) sync_read_submits: usize,
    pub(crate) sync_write_submits: usize,
    pub(crate) irq_acks: usize,
    pub(crate) completion_signals: usize,
    pub(crate) completion_wakeups: usize,
    pub(crate) completion_poll_observations: usize,
    pub(crate) completion_poll_wakeups: usize,
    pub(crate) device_inflight: usize,
    pub(crate) device_inflight_high_watermark: usize,
}

#[cfg(feature = "perf-counters")]
mod block_io_perf {
    use core::sync::atomic::{AtomicUsize, Ordering};

    pub(super) static NB_READ_SUBMITS: AtomicUsize = AtomicUsize::new(0);
    pub(super) static NB_WRITE_SUBMITS: AtomicUsize = AtomicUsize::new(0);
    pub(super) static NB_READ_WAITS: AtomicUsize = AtomicUsize::new(0);
    pub(super) static NB_WRITE_WAITS: AtomicUsize = AtomicUsize::new(0);
    pub(super) static NB_READ_COMPLETIONS: AtomicUsize = AtomicUsize::new(0);
    pub(super) static NB_WRITE_COMPLETIONS: AtomicUsize = AtomicUsize::new(0);
    pub(super) static FALLBACK_SYNC_READS: AtomicUsize = AtomicUsize::new(0);
    pub(super) static FALLBACK_SYNC_WRITES: AtomicUsize = AtomicUsize::new(0);
    pub(super) static FALLBACK_UNSAFE_READS: AtomicUsize = AtomicUsize::new(0);
    pub(super) static FALLBACK_UNSAFE_WRITES: AtomicUsize = AtomicUsize::new(0);
    pub(super) static FALLBACK_NO_READY_READS: AtomicUsize = AtomicUsize::new(0);
    pub(super) static FALLBACK_NO_READY_WRITES: AtomicUsize = AtomicUsize::new(0);
    pub(super) static SYNC_READ_SUBMITS: AtomicUsize = AtomicUsize::new(0);
    pub(super) static SYNC_WRITE_SUBMITS: AtomicUsize = AtomicUsize::new(0);
    pub(super) static IRQ_ACKS: AtomicUsize = AtomicUsize::new(0);
    pub(super) static COMPLETION_SIGNALS: AtomicUsize = AtomicUsize::new(0);
    pub(super) static COMPLETION_WAKEUPS: AtomicUsize = AtomicUsize::new(0);
    pub(super) static COMPLETION_POLL_OBSERVATIONS: AtomicUsize = AtomicUsize::new(0);
    pub(super) static COMPLETION_POLL_WAKEUPS: AtomicUsize = AtomicUsize::new(0);
    pub(super) static DEVICE_INFLIGHT: AtomicUsize = AtomicUsize::new(0);
    pub(super) static DEVICE_INFLIGHT_HIGH_WATERMARK: AtomicUsize = AtomicUsize::new(0);

    #[inline(always)]
    pub(super) fn inc(counter: &AtomicUsize) {
        counter.fetch_add(1, Ordering::Relaxed);
    }
}

struct DeviceIoInflightGuard;

impl DeviceIoInflightGuard {
    #[inline(always)]
    fn new() -> Self {
        #[cfg(feature = "perf-counters")]
        {
            let inflight = block_io_perf::DEVICE_INFLIGHT.fetch_add(1, Ordering::Relaxed) + 1;
            let mut current = block_io_perf::DEVICE_INFLIGHT_HIGH_WATERMARK.load(Ordering::Relaxed);
            while inflight > current {
                match block_io_perf::DEVICE_INFLIGHT_HIGH_WATERMARK.compare_exchange_weak(
                    current,
                    inflight,
                    Ordering::Relaxed,
                    Ordering::Relaxed,
                ) {
                    Ok(_) => break,
                    Err(next) => current = next,
                }
            }
        }
        Self
    }
}

impl Drop for DeviceIoInflightGuard {
    #[inline(always)]
    fn drop(&mut self) {
        #[cfg(feature = "perf-counters")]
        {
            let previous = block_io_perf::DEVICE_INFLIGHT.fetch_sub(1, Ordering::Relaxed);
            assert_ne!(previous, 0, "block device inflight counter underflow");
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum BlockIoPath {
    Nonblocking,
    Sync,
    FallbackUnsafe,
}

pub struct VirtIOBlock {
    state: SpinNoIrqLock<VirtIOBlockState>,
    async_inflight_hint: AtomicUsize,
    base_addr: usize,
    cache_key: usize,
    irq: usize,
    capacity_blocks: usize,
    condvars: BTreeMap<u16, Condvar>,
}

struct VirtIOBlockState {
    device: VirtIOBlk<VirtioHal, VirtioTransport>,
    async_inflight: usize,
}

impl VirtIOBlock {
    pub fn read_blocks(&self, block_id: usize, buf: &mut [u8]) {
        block_cache::read_with_cache(self.cache_key(), block_id, buf, |block_id, buf| {
            self.read_blocks_uncached(block_id, buf);
        });
    }

    pub(crate) fn read_blocks_versioned_fill_for_file_plan(
        &self,
        block_id: usize,
        buf: &mut [u8],
    ) -> block_cache::VersionedReadStats {
        assert_eq!(buf.len() % 512, 0, "file read plan must use full blocks");
        assert!(
            block_id
                .checked_add(buf.len() / 512)
                .is_some_and(|end| end <= self.capacity_blocks),
            "file read plan exceeds block device capacity"
        );
        block_cache::read_with_cache_versioned_fill(
            self.cache_key(),
            block_id,
            buf,
            |block_id, buf| self.read_blocks_uncached(block_id, buf),
        )
    }

    fn read_blocks_uncached(&self, block_id: usize, buf: &mut [u8]) {
        let _inflight = DeviceIoInflightGuard::new();
        match choose_block_io_path() {
            BlockIoPath::Nonblocking => self.read_blocks_nonblocking_uncached(block_id, buf),
            BlockIoPath::Sync => self.read_blocks_sync_uncached(block_id, buf),
            BlockIoPath::FallbackUnsafe => {
                record_fallback_unsafe_read();
                self.read_blocks_sync_uncached(block_id, buf);
            }
        }
    }

    fn read_blocks_nonblocking_uncached(&self, block_id: usize, buf: &mut [u8]) {
        // The nonblocking virtio API borrows req/buf/resp until completion.
        // Keep them in the blocked task frame so the device completion path
        // never observes pointers into a returned stack frame.
        let mut req = BlkReq::default();
        let mut resp = BlkResp::default();
        record_nb_read_submit();
        let (task_cx_ptr, token) = {
            let mut state = self.state.lock();
            let token = unsafe {
                state
                    .device
                    .read_blocks_nb(block_id, &mut req, buf, &mut resp)
                    .unwrap()
            };
            state.async_inflight += 1;
            self.async_inflight_hint.fetch_add(1, Ordering::Release);
            record_nb_read_wait();
            (self.condvars.get(&token).unwrap().wait_no_sched(), token)
        };
        schedule(task_cx_ptr);
        let next_completed = {
            let mut state = self.state.lock();
            unsafe {
                state
                    .device
                    .complete_read_blocks(token, &req, buf, &mut resp)
                    .expect("Error when reading VirtIOBlk");
            }
            assert_ne!(state.async_inflight, 0, "VirtIO read inflight underflow");
            state.async_inflight -= 1;
            let previous = self.async_inflight_hint.fetch_sub(1, Ordering::AcqRel);
            assert_ne!(previous, 0, "VirtIO read inflight hint underflow");
            record_nb_read_completion();
            state.device.peek_used()
        };
        self.signal_completed(next_completed);
    }

    fn read_blocks_sync_uncached(&self, block_id: usize, buf: &mut [u8]) {
        record_sync_read_submit();
        let mut state = self.state.lock();
        assert_eq!(
            state.async_inflight, 0,
            "synchronous VirtIO read mixed with asynchronous requests"
        );
        state
            .device
            .read_blocks(block_id, buf)
            .unwrap_or_else(|err| {
                panic!(
                    "Error when reading VirtIOBlk: block_id={}, blocks={}, capacity_blocks={}, err={:?}",
                    block_id,
                    buf.len() / 512,
                    self.capacity_blocks,
                    err
                )
            });
    }

    pub fn write_blocks(&self, block_id: usize, buf: &[u8]) {
        block_cache::write_with_cache(self.cache_key(), block_id, buf, |block_id, buf| {
            self.write_blocks_uncached(block_id, buf);
        });
    }

    pub(crate) fn write_blocks_for_file_plan(
        &self,
        block_id: usize,
        buf: &[u8],
    ) -> block_cache::WriteStats {
        assert_eq!(buf.len() % 512, 0, "file write plan must use full blocks");
        assert!(
            block_id
                .checked_add(buf.len() / 512)
                .is_some_and(|end| end <= self.capacity_blocks),
            "file write plan exceeds block device capacity"
        );
        block_cache::write_with_cache(self.cache_key(), block_id, buf, |block_id, buf| {
            self.write_blocks_uncached(block_id, buf);
        })
    }

    fn write_blocks_uncached(&self, block_id: usize, buf: &[u8]) {
        let _inflight = DeviceIoInflightGuard::new();
        match choose_block_io_path() {
            BlockIoPath::Nonblocking => self.write_blocks_nonblocking_uncached(block_id, buf),
            BlockIoPath::Sync => self.write_blocks_sync_uncached(block_id, buf),
            BlockIoPath::FallbackUnsafe => {
                record_fallback_unsafe_write();
                self.write_blocks_sync_uncached(block_id, buf);
            }
        }
    }

    fn write_blocks_nonblocking_uncached(&self, block_id: usize, buf: &[u8]) {
        // Same lifetime contract as the read path: req/buf/resp remain
        // owned by this blocked task until complete_write_blocks() returns.
        let mut req = BlkReq::default();
        let mut resp = BlkResp::default();
        record_nb_write_submit();
        let (task_cx_ptr, token) = {
            let mut state = self.state.lock();
            let token = unsafe {
                state
                    .device
                    .write_blocks_nb(block_id, &mut req, buf, &mut resp)
                    .unwrap()
            };
            state.async_inflight += 1;
            self.async_inflight_hint.fetch_add(1, Ordering::Release);
            record_nb_write_wait();
            (self.condvars.get(&token).unwrap().wait_no_sched(), token)
        };
        schedule(task_cx_ptr);
        let next_completed = {
            let mut state = self.state.lock();
            unsafe {
                state
                    .device
                    .complete_write_blocks(token, &req, buf, &mut resp)
                    .expect("Error when writing VirtIOBlk");
            }
            assert_ne!(state.async_inflight, 0, "VirtIO write inflight underflow");
            state.async_inflight -= 1;
            let previous = self.async_inflight_hint.fetch_sub(1, Ordering::AcqRel);
            assert_ne!(previous, 0, "VirtIO write inflight hint underflow");
            record_nb_write_completion();
            state.device.peek_used()
        };
        self.signal_completed(next_completed);
    }

    fn write_blocks_sync_uncached(&self, block_id: usize, buf: &[u8]) {
        record_sync_write_submit();
        let mut state = self.state.lock();
        assert_eq!(
            state.async_inflight, 0,
            "synchronous VirtIO write mixed with asynchronous requests"
        );
        state
            .device
            .write_blocks(block_id, buf)
            .unwrap_or_else(|err| {
                panic!(
                    "Error when writing VirtIOBlk: block_id={}, blocks={}, capacity_blocks={}, err={:?}",
                    block_id,
                    buf.len() / 512,
                    self.capacity_blocks,
                    err
                )
            });
    }

    #[cfg(any(target_arch = "riscv64", target_arch = "loongarch64"))]
    pub fn handle_irq(&self) {
        let next_completed = {
            let mut state = self.state.lock();
            let _ = state.device.ack_interrupt();
            record_irq_ack();
            state.device.peek_used()
        };
        self.signal_completed(next_completed);
    }

    #[cfg(target_arch = "loongarch64")]
    fn poll_completion(&self) {
        if self.async_inflight_hint.load(Ordering::Acquire) == 0 {
            return;
        }
        // Timer fallback must never wait behind an interrupted submitter. It
        // only observes the used-ring head; the owning task still retires the
        // descriptor and performs HAL buffer teardown after being woken.
        let completed = self
            .state
            .try_lock()
            .and_then(|mut state| state.device.peek_used());
        if completed.is_some() {
            record_completion_poll_observation();
        }
        if self.signal_completed(completed) {
            record_completion_poll_wakeup();
        }
    }

    pub fn num_blocks(&self) -> u64 {
        self.capacity_blocks as u64
    }

    pub fn irq(&self) -> usize {
        self.irq
    }

    pub fn base_addr(&self) -> usize {
        self.base_addr
    }

    fn cache_key(&self) -> usize {
        self.cache_key
    }

    fn signal_completed(&self, token: Option<u16>) -> bool {
        // CONTEXT: Completion is serialized through the virtqueue used ring.
        // Wake only the descriptor head reported by the device; unrelated
        // sleepers must stay blocked until their own token reaches used. Do
        // the scheduler wake only after releasing the IRQ-safe queue lock.
        if let Some(token) = token {
            record_completion_signal();
            if self.condvars.get(&token).unwrap().signal_front() {
                record_completion_wakeup();
                return true;
            }
        }
        false
    }

    pub fn new(device: BlockDeviceConfig) -> Self {
        let (transport, base_addr, cache_key, irq) = match device {
            BlockDeviceConfig::Mmio(device) => (
                mmio_transport(device.base, device.size),
                device.base,
                device.base,
                device.irq,
            ),
            BlockDeviceConfig::Pci(device) => {
                // CONTEXT: PCI block devices can share the same ECAM window.
                // Include BDF in the cache key so separate disks never alias in
                // the block cache.
                let bdf_key = ((device.bus as usize) << 16)
                    | ((device.device as usize) << 8)
                    | device.function as usize;
                (
                    crate::board::pci_transport(device).into(),
                    device.ecam_base,
                    device.ecam_base.wrapping_add(bdf_key),
                    device.irq,
                )
            }
            #[cfg(target_arch = "riscv64")]
            BlockDeviceConfig::StarFiveMmc(_) => {
                unreachable!("JH7110 MMC cannot be constructed as VirtIO")
            }
            #[cfg(target_arch = "riscv64")]
            BlockDeviceConfig::RamDisk { .. } => {
                unreachable!("boot ramdisk cannot be constructed as VirtIO")
            }
        };
        let virtio_blk = VirtIOBlk::<VirtioHal, _>::new(transport).unwrap();
        let capacity_blocks = virtio_blk.capacity() as usize;
        let channels = virtio_blk.virt_queue_size();
        let state = SpinNoIrqLock::new(VirtIOBlockState {
            device: virtio_blk,
            async_inflight: 0,
        });
        let mut condvars = BTreeMap::new();
        // Nonblocking tokens are virtqueue descriptor-head indexes, so the
        // wait-channel count follows virt_queue_size(), not disk capacity.
        for i in 0..channels {
            let condvar = Condvar::new();
            condvars.insert(i, condvar);
        }
        Self {
            state,
            async_inflight_hint: AtomicUsize::new(0),
            base_addr,
            cache_key,
            irq,
            capacity_blocks,
            condvars,
        }
    }
}

pub enum KernelBlockDevice {
    VirtIo(VirtIOBlock),
    #[cfg(target_arch = "riscv64")]
    StarFiveMmc(crate::drivers::dw_mmc::Jh7110MmcBlock),
    #[cfg(target_arch = "riscv64")]
    RamDisk(crate::drivers::ramdisk::RamDiskBlock),
}

impl KernelBlockDevice {
    pub fn new(device: BlockDeviceConfig) -> Self {
        match device {
            BlockDeviceConfig::Mmio(_) | BlockDeviceConfig::Pci(_) => {
                Self::VirtIo(VirtIOBlock::new(device))
            }
            #[cfg(target_arch = "riscv64")]
            BlockDeviceConfig::StarFiveMmc(device) => {
                Self::StarFiveMmc(crate::drivers::dw_mmc::Jh7110MmcBlock::new(device))
            }
            #[cfg(target_arch = "riscv64")]
            BlockDeviceConfig::RamDisk { base, size } => {
                Self::RamDisk(crate::drivers::ramdisk::RamDiskBlock::new(base, size))
            }
        }
    }

    pub fn read_block(&self, block_id: usize, buf: &mut [u8]) {
        self.read_blocks(block_id, buf);
    }

    pub fn read_blocks(&self, block_id: usize, buf: &mut [u8]) {
        match self {
            Self::VirtIo(device) => device.read_blocks(block_id, buf),
            #[cfg(target_arch = "riscv64")]
            Self::StarFiveMmc(device) => device.read_blocks(block_id, buf),
            #[cfg(target_arch = "riscv64")]
            Self::RamDisk(device) => device.read_blocks(block_id, buf),
        }
    }

    pub fn write_block(&self, block_id: usize, buf: &[u8]) {
        self.write_blocks(block_id, buf);
    }

    pub fn write_blocks(&self, block_id: usize, buf: &[u8]) {
        match self {
            Self::VirtIo(device) => device.write_blocks(block_id, buf),
            #[cfg(target_arch = "riscv64")]
            Self::StarFiveMmc(device) => device.write_blocks(block_id, buf),
            #[cfg(target_arch = "riscv64")]
            Self::RamDisk(device) => device.write_blocks(block_id, buf),
        }
    }

    pub(crate) fn read_blocks_versioned_fill_for_file_plan(
        &self,
        block_id: usize,
        buf: &mut [u8],
    ) -> block_cache::VersionedReadStats {
        match self {
            Self::VirtIo(device) => device.read_blocks_versioned_fill_for_file_plan(block_id, buf),
            #[cfg(target_arch = "riscv64")]
            Self::StarFiveMmc(device) => {
                device.read_blocks_versioned_fill_for_file_plan(block_id, buf)
            }
            #[cfg(target_arch = "riscv64")]
            Self::RamDisk(device) => device.read_blocks_versioned_fill_for_file_plan(block_id, buf),
        }
    }

    pub(crate) fn write_blocks_for_file_plan(
        &self,
        block_id: usize,
        buf: &[u8],
    ) -> block_cache::WriteStats {
        match self {
            Self::VirtIo(device) => device.write_blocks_for_file_plan(block_id, buf),
            #[cfg(target_arch = "riscv64")]
            Self::StarFiveMmc(device) => device.write_blocks_for_file_plan(block_id, buf),
            #[cfg(target_arch = "riscv64")]
            Self::RamDisk(device) => device.write_blocks_for_file_plan(block_id, buf),
        }
    }

    pub fn num_blocks(&self) -> u64 {
        match self {
            Self::VirtIo(device) => device.num_blocks(),
            #[cfg(target_arch = "riscv64")]
            Self::StarFiveMmc(device) => device.num_blocks(),
            #[cfg(target_arch = "riscv64")]
            Self::RamDisk(device) => device.num_blocks(),
        }
    }

    pub fn irq(&self) -> usize {
        match self {
            Self::VirtIo(device) => device.irq(),
            #[cfg(target_arch = "riscv64")]
            Self::StarFiveMmc(_) | Self::RamDisk(_) => 0,
        }
    }

    pub fn base_addr(&self) -> usize {
        match self {
            Self::VirtIo(device) => device.base_addr(),
            #[cfg(target_arch = "riscv64")]
            Self::StarFiveMmc(device) => device.base_addr(),
            #[cfg(target_arch = "riscv64")]
            Self::RamDisk(device) => device.base_addr(),
        }
    }

    fn handle_irq(&self) {
        match self {
            Self::VirtIo(device) => device.handle_irq(),
            #[cfg(target_arch = "riscv64")]
            Self::StarFiveMmc(_) | Self::RamDisk(_) => {}
        }
    }

    #[cfg(target_arch = "loongarch64")]
    fn poll_completion(&self) {
        match self {
            Self::VirtIo(device) => device.poll_completion(),
        }
    }
}

lazy_static! {
    // CONTEXT: The first DTB-discovered block device is the contest root disk
    // mounted as x0; additional entries stay addressable for explicit mounts.
    pub static ref BLOCK_DEVICES: Vec<Arc<BlockDeviceImpl>> = crate::board::block_devices()
        .iter()
        .enumerate()
        .map(|(index, device)| {
            let block_device = Arc::new(BlockDeviceImpl::new(*device));
            info!(
                "block device[{}]: base={:#x}, irq={}, sectors={}",
                index,
                block_device.base_addr(),
                block_device.irq(),
                block_device.num_blocks(),
            );
            block_device
        })
        .collect();
    pub static ref BLOCK_DEVICE: Arc<BlockDeviceImpl> = BLOCK_DEVICES
        .first()
        .expect("DTB is missing a block device")
        .clone();
    // Legacy callers that still use BLOCK_DEVICE must keep seeing x0, not the
    // most recently discovered disk. Multi-disk paths should use BLOCK_DEVICES.
}

#[cfg(feature = "perf-counters")]
pub(crate) fn block_io_stats_snapshot() -> BlockIoStats {
    use block_io_perf::{
        COMPLETION_POLL_OBSERVATIONS, COMPLETION_POLL_WAKEUPS, COMPLETION_SIGNALS,
        COMPLETION_WAKEUPS, DEVICE_INFLIGHT, DEVICE_INFLIGHT_HIGH_WATERMARK,
        FALLBACK_NO_READY_READS, FALLBACK_NO_READY_WRITES, FALLBACK_SYNC_READS,
        FALLBACK_SYNC_WRITES, FALLBACK_UNSAFE_READS, FALLBACK_UNSAFE_WRITES, IRQ_ACKS,
        NB_READ_COMPLETIONS, NB_READ_SUBMITS, NB_READ_WAITS, NB_WRITE_COMPLETIONS,
        NB_WRITE_SUBMITS, NB_WRITE_WAITS, SYNC_READ_SUBMITS, SYNC_WRITE_SUBMITS,
    };
    BlockIoStats {
        nonblocking_requested: block_io_nonblocking_requested() as usize,
        nb_read_submits: NB_READ_SUBMITS.load(Ordering::Relaxed),
        nb_write_submits: NB_WRITE_SUBMITS.load(Ordering::Relaxed),
        nb_read_waits: NB_READ_WAITS.load(Ordering::Relaxed),
        nb_write_waits: NB_WRITE_WAITS.load(Ordering::Relaxed),
        nb_read_completions: NB_READ_COMPLETIONS.load(Ordering::Relaxed),
        nb_write_completions: NB_WRITE_COMPLETIONS.load(Ordering::Relaxed),
        fallback_sync_reads: FALLBACK_SYNC_READS.load(Ordering::Relaxed),
        fallback_sync_writes: FALLBACK_SYNC_WRITES.load(Ordering::Relaxed),
        fallback_unsafe_reads: FALLBACK_UNSAFE_READS.load(Ordering::Relaxed),
        fallback_unsafe_writes: FALLBACK_UNSAFE_WRITES.load(Ordering::Relaxed),
        fallback_no_ready_reads: FALLBACK_NO_READY_READS.load(Ordering::Relaxed),
        fallback_no_ready_writes: FALLBACK_NO_READY_WRITES.load(Ordering::Relaxed),
        sync_read_submits: SYNC_READ_SUBMITS.load(Ordering::Relaxed),
        sync_write_submits: SYNC_WRITE_SUBMITS.load(Ordering::Relaxed),
        irq_acks: IRQ_ACKS.load(Ordering::Relaxed),
        completion_signals: COMPLETION_SIGNALS.load(Ordering::Relaxed),
        completion_wakeups: COMPLETION_WAKEUPS.load(Ordering::Relaxed),
        completion_poll_observations: COMPLETION_POLL_OBSERVATIONS.load(Ordering::Relaxed),
        completion_poll_wakeups: COMPLETION_POLL_WAKEUPS.load(Ordering::Relaxed),
        device_inflight: DEVICE_INFLIGHT.load(Ordering::Relaxed),
        device_inflight_high_watermark: DEVICE_INFLIGHT_HIGH_WATERMARK.load(Ordering::Relaxed),
    }
}

fn block_io_nonblocking_requested() -> bool {
    *DEV_NON_BLOCKING_ACCESS.lock()
}

fn choose_block_io_path() -> BlockIoPath {
    if !block_io_nonblocking_requested() {
        return BlockIoPath::Sync;
    }
    // Nonblocking virtio waits may schedule only from task context with
    // supervisor interrupts enabled. Otherwise use synchronous I/O so boot
    // and IRQ-sensitive paths never sleep here.
    if !can_sleep_for_nonblocking_block_io() {
        return BlockIoPath::FallbackUnsafe;
    }
    // A single CPU cannot overlap the blocked caller with useful kernel work;
    // the async submit/sleep/wake path only adds scheduler and IRQ latency.
    // Keep the lower-overhead synchronous path for UP while making SMP I/O
    // waitable whenever the caller can safely sleep.
    if crate::cpu::topology().possible_count() == 1 {
        return BlockIoPath::Sync;
    }
    BlockIoPath::Nonblocking
}

fn can_sleep_for_nonblocking_block_io() -> bool {
    #[cfg(any(target_arch = "riscv64", target_arch = "loongarch64"))]
    {
        // Syscalls and user page-fault handlers explicitly enable supervisor
        // interrupts before entering VFS. A caller that still has interrupts
        // disabled may own an IRQ-safe lock and must not strand it by sleeping.
        crate::arch::interrupt::supervisor_interrupt_enabled()
            && crate::task::current_task().is_some()
    }
    #[cfg(not(any(target_arch = "riscv64", target_arch = "loongarch64")))]
    {
        false
    }
}

#[cfg(feature = "perf-counters")]
#[inline(always)]
fn record_nb_read_submit() {
    block_io_perf::inc(&block_io_perf::NB_READ_SUBMITS);
}

#[cfg(not(feature = "perf-counters"))]
#[inline(always)]
fn record_nb_read_submit() {}

#[cfg(feature = "perf-counters")]
#[inline(always)]
fn record_nb_write_submit() {
    block_io_perf::inc(&block_io_perf::NB_WRITE_SUBMITS);
}

#[cfg(not(feature = "perf-counters"))]
#[inline(always)]
fn record_nb_write_submit() {}

#[cfg(feature = "perf-counters")]
#[inline(always)]
fn record_nb_read_wait() {
    block_io_perf::inc(&block_io_perf::NB_READ_WAITS);
}

#[cfg(not(feature = "perf-counters"))]
#[inline(always)]
fn record_nb_read_wait() {}

#[cfg(feature = "perf-counters")]
#[inline(always)]
fn record_nb_write_wait() {
    block_io_perf::inc(&block_io_perf::NB_WRITE_WAITS);
}

#[cfg(not(feature = "perf-counters"))]
#[inline(always)]
fn record_nb_write_wait() {}

#[cfg(feature = "perf-counters")]
#[inline(always)]
fn record_nb_read_completion() {
    block_io_perf::inc(&block_io_perf::NB_READ_COMPLETIONS);
}

#[cfg(not(feature = "perf-counters"))]
#[inline(always)]
fn record_nb_read_completion() {}

#[cfg(feature = "perf-counters")]
#[inline(always)]
fn record_nb_write_completion() {
    block_io_perf::inc(&block_io_perf::NB_WRITE_COMPLETIONS);
}

#[cfg(not(feature = "perf-counters"))]
#[inline(always)]
fn record_nb_write_completion() {}

#[cfg(feature = "perf-counters")]
#[inline(always)]
fn record_sync_read_submit() {
    block_io_perf::inc(&block_io_perf::SYNC_READ_SUBMITS);
}

#[cfg(not(feature = "perf-counters"))]
#[inline(always)]
fn record_sync_read_submit() {}

#[cfg(feature = "perf-counters")]
#[inline(always)]
fn record_sync_write_submit() {
    block_io_perf::inc(&block_io_perf::SYNC_WRITE_SUBMITS);
}

#[cfg(not(feature = "perf-counters"))]
#[inline(always)]
fn record_sync_write_submit() {}

#[cfg(feature = "perf-counters")]
#[inline(always)]
fn record_fallback_unsafe_read() {
    block_io_perf::inc(&block_io_perf::FALLBACK_SYNC_READS);
    block_io_perf::inc(&block_io_perf::FALLBACK_UNSAFE_READS);
}

#[cfg(not(feature = "perf-counters"))]
#[inline(always)]
fn record_fallback_unsafe_read() {}

#[cfg(feature = "perf-counters")]
#[inline(always)]
fn record_fallback_unsafe_write() {
    block_io_perf::inc(&block_io_perf::FALLBACK_SYNC_WRITES);
    block_io_perf::inc(&block_io_perf::FALLBACK_UNSAFE_WRITES);
}

#[cfg(not(feature = "perf-counters"))]
#[inline(always)]
fn record_fallback_unsafe_write() {}

#[cfg(all(
    any(target_arch = "riscv64", target_arch = "loongarch64"),
    feature = "perf-counters"
))]
#[inline(always)]
fn record_irq_ack() {
    block_io_perf::inc(&block_io_perf::IRQ_ACKS);
}

#[cfg(all(
    any(target_arch = "riscv64", target_arch = "loongarch64"),
    not(feature = "perf-counters")
))]
#[inline(always)]
fn record_irq_ack() {}

#[cfg(feature = "perf-counters")]
#[inline(always)]
fn record_completion_signal() {
    block_io_perf::inc(&block_io_perf::COMPLETION_SIGNALS);
}

#[cfg(not(feature = "perf-counters"))]
#[inline(always)]
fn record_completion_signal() {}

#[cfg(feature = "perf-counters")]
#[inline(always)]
fn record_completion_wakeup() {
    block_io_perf::inc(&block_io_perf::COMPLETION_WAKEUPS);
}

#[cfg(not(feature = "perf-counters"))]
#[inline(always)]
fn record_completion_wakeup() {}

#[cfg(all(target_arch = "loongarch64", feature = "perf-counters"))]
#[inline(always)]
fn record_completion_poll_observation() {
    block_io_perf::inc(&block_io_perf::COMPLETION_POLL_OBSERVATIONS);
}

#[cfg(all(target_arch = "loongarch64", not(feature = "perf-counters")))]
#[inline(always)]
fn record_completion_poll_observation() {}

#[cfg(all(target_arch = "loongarch64", feature = "perf-counters"))]
#[inline(always)]
fn record_completion_poll_wakeup() {
    block_io_perf::inc(&block_io_perf::COMPLETION_POLL_WAKEUPS);
}

#[cfg(all(target_arch = "loongarch64", not(feature = "perf-counters")))]
#[inline(always)]
fn record_completion_poll_wakeup() {}

pub fn handle_irq(irq: usize) -> bool {
    // Dispatch across every discovered block device, not just x0. Extra disks
    // can back explicit `/dev/vdX` mounts and have independent virtio IRQs.
    if let Some(device) = BLOCK_DEVICES.iter().find(|device| device.irq() == irq) {
        device.handle_irq();
        true
    } else {
        false
    }
}

#[cfg(target_arch = "loongarch64")]
pub fn poll_completions() {
    for device in BLOCK_DEVICES.iter() {
        device.poll_completion();
    }
}
