use crate::DEV_NON_BLOCKING_ACCESS;
use crate::board::{BlockDeviceConfig, BlockDeviceImpl};
use crate::drivers::block_cache;
use crate::drivers::virtio::{VirtioHal, VirtioTransport, mmio_transport};
use crate::sync::{Condvar, UPIntrFreeCell};
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
}

static BLOCK_IO_NB_READ_SUBMITS: AtomicUsize = AtomicUsize::new(0);
static BLOCK_IO_NB_WRITE_SUBMITS: AtomicUsize = AtomicUsize::new(0);
static BLOCK_IO_NB_READ_WAITS: AtomicUsize = AtomicUsize::new(0);
static BLOCK_IO_NB_WRITE_WAITS: AtomicUsize = AtomicUsize::new(0);
static BLOCK_IO_NB_READ_COMPLETIONS: AtomicUsize = AtomicUsize::new(0);
static BLOCK_IO_NB_WRITE_COMPLETIONS: AtomicUsize = AtomicUsize::new(0);
static BLOCK_IO_FALLBACK_SYNC_READS: AtomicUsize = AtomicUsize::new(0);
static BLOCK_IO_FALLBACK_SYNC_WRITES: AtomicUsize = AtomicUsize::new(0);
static BLOCK_IO_FALLBACK_UNSAFE_READS: AtomicUsize = AtomicUsize::new(0);
static BLOCK_IO_FALLBACK_UNSAFE_WRITES: AtomicUsize = AtomicUsize::new(0);
static BLOCK_IO_FALLBACK_NO_READY_READS: AtomicUsize = AtomicUsize::new(0);
static BLOCK_IO_FALLBACK_NO_READY_WRITES: AtomicUsize = AtomicUsize::new(0);
static BLOCK_IO_SYNC_READ_SUBMITS: AtomicUsize = AtomicUsize::new(0);
static BLOCK_IO_SYNC_WRITE_SUBMITS: AtomicUsize = AtomicUsize::new(0);
#[cfg(any(target_arch = "riscv64", feature = "perf-counters"))]
static BLOCK_IO_IRQ_ACKS: AtomicUsize = AtomicUsize::new(0);
static BLOCK_IO_COMPLETION_SIGNALS: AtomicUsize = AtomicUsize::new(0);
static BLOCK_IO_COMPLETION_WAKEUPS: AtomicUsize = AtomicUsize::new(0);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum BlockIoPath {
    Nonblocking,
    Sync,
    FallbackUnsafe,
    FallbackNoReady,
}

pub struct VirtIOBlock {
    virtio_blk: UPIntrFreeCell<VirtIOBlk<VirtioHal, VirtioTransport>>,
    base_addr: usize,
    cache_key: usize,
    irq: usize,
    capacity_blocks: usize,
    condvars: BTreeMap<u16, Condvar>,
}

impl VirtIOBlock {
    pub fn read_block(&self, block_id: usize, buf: &mut [u8]) {
        self.read_blocks(block_id, buf);
    }

    pub fn read_blocks(&self, block_id: usize, buf: &mut [u8]) {
        block_cache::read_with_cache(self.cache_key(), block_id, buf, |block_id, buf| {
            self.read_blocks_uncached(block_id, buf);
        });
    }

    fn read_blocks_uncached(&self, block_id: usize, buf: &mut [u8]) {
        match choose_block_io_path() {
            BlockIoPath::Nonblocking => self.read_blocks_nonblocking_uncached(block_id, buf),
            BlockIoPath::Sync => self.read_blocks_sync_uncached(block_id, buf),
            BlockIoPath::FallbackUnsafe => {
                record_fallback_read(&BLOCK_IO_FALLBACK_UNSAFE_READS);
                self.read_blocks_sync_uncached(block_id, buf);
            }
            BlockIoPath::FallbackNoReady => {
                record_fallback_read(&BLOCK_IO_FALLBACK_NO_READY_READS);
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
        let mut token = 0;
        BLOCK_IO_NB_READ_SUBMITS.fetch_add(1, Ordering::Relaxed);
        let task_cx_ptr = self.virtio_blk.exclusive_session(|blk| {
            token = unsafe {
                blk.read_blocks_nb(block_id, &mut req, buf, &mut resp)
                    .unwrap()
            };
            BLOCK_IO_NB_READ_WAITS.fetch_add(1, Ordering::Relaxed);
            self.condvars.get(&token).unwrap().wait_no_sched()
        });
        schedule(task_cx_ptr);
        self.virtio_blk.exclusive_session(|blk| {
            unsafe {
                blk.complete_read_blocks(token, &req, buf, &mut resp)
                    .expect("Error when reading VirtIOBlk");
            }
            BLOCK_IO_NB_READ_COMPLETIONS.fetch_add(1, Ordering::Relaxed);
            self.signal_next_completed(blk);
        });
    }

    fn read_blocks_sync_uncached(&self, block_id: usize, buf: &mut [u8]) {
        BLOCK_IO_SYNC_READ_SUBMITS.fetch_add(1, Ordering::Relaxed);
        self.virtio_blk
            .exclusive_access()
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

    pub fn write_block(&self, block_id: usize, buf: &[u8]) {
        self.write_blocks(block_id, buf);
    }

    pub fn write_blocks(&self, block_id: usize, buf: &[u8]) {
        block_cache::write_with_cache(self.cache_key(), block_id, buf, |block_id, buf| {
            self.write_blocks_uncached(block_id, buf);
        });
    }

    fn write_blocks_uncached(&self, block_id: usize, buf: &[u8]) {
        match choose_block_io_path() {
            BlockIoPath::Nonblocking => self.write_blocks_nonblocking_uncached(block_id, buf),
            BlockIoPath::Sync => self.write_blocks_sync_uncached(block_id, buf),
            BlockIoPath::FallbackUnsafe => {
                record_fallback_write(&BLOCK_IO_FALLBACK_UNSAFE_WRITES);
                self.write_blocks_sync_uncached(block_id, buf);
            }
            BlockIoPath::FallbackNoReady => {
                record_fallback_write(&BLOCK_IO_FALLBACK_NO_READY_WRITES);
                self.write_blocks_sync_uncached(block_id, buf);
            }
        }
    }

    fn write_blocks_nonblocking_uncached(&self, block_id: usize, buf: &[u8]) {
        // Same lifetime contract as the read path: req/buf/resp remain
        // owned by this blocked task until complete_write_blocks() returns.
        let mut req = BlkReq::default();
        let mut resp = BlkResp::default();
        let mut token = 0;
        BLOCK_IO_NB_WRITE_SUBMITS.fetch_add(1, Ordering::Relaxed);
        let task_cx_ptr = self.virtio_blk.exclusive_session(|blk| {
            token = unsafe {
                blk.write_blocks_nb(block_id, &mut req, buf, &mut resp)
                    .unwrap()
            };
            BLOCK_IO_NB_WRITE_WAITS.fetch_add(1, Ordering::Relaxed);
            self.condvars.get(&token).unwrap().wait_no_sched()
        });
        schedule(task_cx_ptr);
        self.virtio_blk.exclusive_session(|blk| {
            unsafe {
                blk.complete_write_blocks(token, &req, buf, &mut resp)
                    .expect("Error when writing VirtIOBlk");
            }
            BLOCK_IO_NB_WRITE_COMPLETIONS.fetch_add(1, Ordering::Relaxed);
            self.signal_next_completed(blk);
        });
    }

    fn write_blocks_sync_uncached(&self, block_id: usize, buf: &[u8]) {
        BLOCK_IO_SYNC_WRITE_SUBMITS.fetch_add(1, Ordering::Relaxed);
        self.virtio_blk
            .exclusive_access()
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

    #[cfg(target_arch = "riscv64")]
    pub fn handle_irq(&self) {
        self.virtio_blk.exclusive_session(|blk| {
            let _ = blk.ack_interrupt();
            BLOCK_IO_IRQ_ACKS.fetch_add(1, Ordering::Relaxed);
            self.signal_next_completed(blk);
        });
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

    fn signal_next_completed(&self, blk: &mut VirtIOBlk<VirtioHal, VirtioTransport>) {
        // CONTEXT: Completion is serialized through the virtqueue used ring.
        // Wake only the descriptor head reported by the device; unrelated
        // sleepers must stay blocked until their own token reaches used.
        if let Some(token) = blk.peek_used() {
            BLOCK_IO_COMPLETION_SIGNALS.fetch_add(1, Ordering::Relaxed);
            if self.condvars.get(&token).unwrap().signal() {
                BLOCK_IO_COMPLETION_WAKEUPS.fetch_add(1, Ordering::Relaxed);
            }
        }
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
        };
        let virtio_blk = VirtIOBlk::<VirtioHal, _>::new(transport).unwrap();
        let capacity_blocks = virtio_blk.capacity() as usize;
        let channels = virtio_blk.virt_queue_size();
        let virtio_blk = unsafe { UPIntrFreeCell::new(virtio_blk) };
        let mut condvars = BTreeMap::new();
        // Nonblocking tokens are virtqueue descriptor-head indexes, so the
        // wait-channel count follows virt_queue_size(), not disk capacity.
        for i in 0..channels {
            let condvar = Condvar::new();
            condvars.insert(i, condvar);
        }
        Self {
            virtio_blk,
            base_addr,
            cache_key,
            irq,
            capacity_blocks,
            condvars,
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
}

#[allow(dead_code)]
pub fn block_device(index: usize) -> Option<Arc<BlockDeviceImpl>> {
    BLOCK_DEVICES.get(index).cloned()
}

#[cfg(feature = "perf-counters")]
pub(crate) fn block_io_stats_snapshot() -> BlockIoStats {
    BlockIoStats {
        nonblocking_requested: block_io_nonblocking_requested() as usize,
        nb_read_submits: BLOCK_IO_NB_READ_SUBMITS.load(Ordering::Relaxed),
        nb_write_submits: BLOCK_IO_NB_WRITE_SUBMITS.load(Ordering::Relaxed),
        nb_read_waits: BLOCK_IO_NB_READ_WAITS.load(Ordering::Relaxed),
        nb_write_waits: BLOCK_IO_NB_WRITE_WAITS.load(Ordering::Relaxed),
        nb_read_completions: BLOCK_IO_NB_READ_COMPLETIONS.load(Ordering::Relaxed),
        nb_write_completions: BLOCK_IO_NB_WRITE_COMPLETIONS.load(Ordering::Relaxed),
        fallback_sync_reads: BLOCK_IO_FALLBACK_SYNC_READS.load(Ordering::Relaxed),
        fallback_sync_writes: BLOCK_IO_FALLBACK_SYNC_WRITES.load(Ordering::Relaxed),
        fallback_unsafe_reads: BLOCK_IO_FALLBACK_UNSAFE_READS.load(Ordering::Relaxed),
        fallback_unsafe_writes: BLOCK_IO_FALLBACK_UNSAFE_WRITES.load(Ordering::Relaxed),
        fallback_no_ready_reads: BLOCK_IO_FALLBACK_NO_READY_READS.load(Ordering::Relaxed),
        fallback_no_ready_writes: BLOCK_IO_FALLBACK_NO_READY_WRITES.load(Ordering::Relaxed),
        sync_read_submits: BLOCK_IO_SYNC_READ_SUBMITS.load(Ordering::Relaxed),
        sync_write_submits: BLOCK_IO_SYNC_WRITE_SUBMITS.load(Ordering::Relaxed),
        irq_acks: BLOCK_IO_IRQ_ACKS.load(Ordering::Relaxed),
        completion_signals: BLOCK_IO_COMPLETION_SIGNALS.load(Ordering::Relaxed),
        completion_wakeups: BLOCK_IO_COMPLETION_WAKEUPS.load(Ordering::Relaxed),
    }
}

fn block_io_nonblocking_requested() -> bool {
    *DEV_NON_BLOCKING_ACCESS.exclusive_access()
}

fn choose_block_io_path() -> BlockIoPath {
    if !block_io_nonblocking_requested() {
        return BlockIoPath::Sync;
    }
    if !can_sleep_for_nonblocking_block_io() {
        return BlockIoPath::FallbackUnsafe;
    }
    if !crate::task::has_ready_task() {
        return BlockIoPath::FallbackNoReady;
    }
    BlockIoPath::Nonblocking
}

fn can_sleep_for_nonblocking_block_io() -> bool {
    #[cfg(target_arch = "riscv64")]
    {
        crate::arch::interrupt::supervisor_interrupt_enabled()
            && crate::task::current_task().is_some()
    }
    #[cfg(not(target_arch = "riscv64"))]
    {
        false
    }
}

fn record_fallback_read(reason_counter: &AtomicUsize) {
    BLOCK_IO_FALLBACK_SYNC_READS.fetch_add(1, Ordering::Relaxed);
    reason_counter.fetch_add(1, Ordering::Relaxed);
}

fn record_fallback_write(reason_counter: &AtomicUsize) {
    BLOCK_IO_FALLBACK_SYNC_WRITES.fetch_add(1, Ordering::Relaxed);
    reason_counter.fetch_add(1, Ordering::Relaxed);
}

#[allow(dead_code)]
pub fn block_count() -> usize {
    BLOCK_DEVICES.len()
}

#[cfg(target_arch = "riscv64")]
pub fn handle_irq(irq: usize) -> bool {
    if let Some(device) = BLOCK_DEVICES.iter().find(|device| device.irq() == irq) {
        device.handle_irq();
        true
    } else {
        false
    }
}

#[allow(unused)]
pub fn block_device_test() {
    let block_device = BLOCK_DEVICE.clone();
    let mut write_buffer = [0u8; 512];
    let mut read_buffer = [0u8; 512];
    for i in 0..512 {
        for byte in write_buffer.iter_mut() {
            *byte = i as u8;
        }
        block_device.write_block(i as usize, &write_buffer);
        block_device.read_block(i as usize, &mut read_buffer);
        assert_eq!(write_buffer, read_buffer);
    }
    println!("block device test passed!");
}
