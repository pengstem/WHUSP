use crate::perf;
use crate::sync::SpinNoIrqLock;
use crate::task::suspend_current_and_run_next;
#[cfg(feature = "perf-counters")]
use crate::timer;
use alloc::collections::{BTreeMap, BTreeSet};
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicUsize, Ordering};

/// Multi-writer sequence used where readers copy data into private buffers.
///
/// An uncontended read performs two atomic loads and never enters a scheduler
/// or IRQ-masked lock. Low bits count active writers; the last writer advances
/// the epoch. A reader that overlaps any writer discards its private copy and
/// yields until the active count returns to zero. Writers do not serialize on
/// this counter, which keeps the sequence compatible with future independent
/// mapped-overwrite plans.
pub(super) struct Ext4Sequence {
    value: AtomicUsize,
}

const EXT4_SEQUENCE_WRITER_BITS: usize = 16;
const EXT4_SEQUENCE_WRITER_LIMIT: usize = 1 << EXT4_SEQUENCE_WRITER_BITS;
const EXT4_SEQUENCE_WRITER_MASK: usize = EXT4_SEQUENCE_WRITER_LIMIT - 1;

/// Generation observed by the one shared read-only metadata cache.
///
/// Writers advance it only after publishing device bytes. A stale buffer is
/// refilled in place when unowned, or detached and retired after its last old
/// reader when still referenced. This gives new readers a separate payload
/// without a mount-wide cache-flush writer phase.
pub(super) struct Ext4CacheEpoch {
    value: AtomicUsize,
}

impl Ext4CacheEpoch {
    pub(super) fn new() -> Self {
        Self {
            value: AtomicUsize::new(1),
        }
    }

    pub(super) fn current(&self) -> u64 {
        self.value.load(Ordering::Acquire) as u64
    }

    pub(super) fn advance(&self) {
        let previous = self.value.fetch_add(1, Ordering::AcqRel);
        assert_ne!(previous, usize::MAX, "ext4 cache epoch wrapped");
    }
}

impl Ext4Sequence {
    pub(super) fn new() -> Self {
        Self {
            value: AtomicUsize::new(0),
        }
    }

    pub(super) fn begin_write(&self) -> Ext4SequenceWriteGuard<'_> {
        let previous = self.value.fetch_add(1, Ordering::AcqRel);
        assert_ne!(
            previous & EXT4_SEQUENCE_WRITER_MASK,
            EXT4_SEQUENCE_WRITER_MASK,
            "ext4 sequence active-writer count exhausted"
        );
        perf::record_ext4_sequence_writer_entry((previous & EXT4_SEQUENCE_WRITER_MASK) + 1);
        Ext4SequenceWriteGuard { sequence: self }
    }

    fn stable_value(&self) -> usize {
        #[cfg(feature = "perf-counters")]
        let mut wait_start = None;
        loop {
            let value = self.value.load(Ordering::Acquire);
            if value & EXT4_SEQUENCE_WRITER_MASK == 0 {
                #[cfg(feature = "perf-counters")]
                if let Some(wait_start) = wait_start {
                    perf::record_ext4_sequence_wait_ticks(
                        timer::get_time().wrapping_sub(wait_start),
                    );
                }
                return value;
            }
            // The writer may be asleep in VirtIO I/O. Yielding here avoids
            // turning a real read/write conflict into an SMP spin convoy.
            #[cfg(feature = "perf-counters")]
            if wait_start.is_none() {
                wait_start = Some(timer::get_time());
            }
            perf::record_ext4_sequence_reader_wait_yield();
            suspend_current_and_run_next();
        }
    }

    pub(super) fn read_stable<V>(
        &self,
        blocks: usize,
        bytes: usize,
        mut read: impl FnMut() -> V,
    ) -> V {
        perf::record_ext4_sequence_read(bytes);
        loop {
            let before = self.stable_value();
            let result = read();
            if self.value.load(Ordering::Acquire) == before {
                return result;
            }
            perf::record_ext4_sequence_reader_retry(blocks, bytes);
        }
    }
}

pub(super) struct Ext4SequenceWriteGuard<'a> {
    sequence: &'a Ext4Sequence,
}

impl Drop for Ext4SequenceWriteGuard<'_> {
    fn drop(&mut self) {
        // Adding `2^N - 1` decrements the low-bit writer count and advances
        // the high-bit epoch in one atomic RMW. This remains correct when
        // writers overlap: readers keep waiting for a zero low-bit count, and
        // every completed writer changes the stable epoch they validate.
        let previous = self
            .sequence
            .value
            .fetch_add(EXT4_SEQUENCE_WRITER_LIMIT - 1, Ordering::Release);
        assert_ne!(
            previous & EXT4_SEQUENCE_WRITER_MASK,
            0,
            "ext4 sequence writer lost ownership"
        );
    }
}

/// Exact physical-sector ownership shared by every EXT4 write path.
///
/// Callers reserve sorted integer LBAs only. No FFI pointer, bcache guard, or
/// filesystem-core guard may be held while waiting for a conflicting range.
pub(super) struct Ext4PhysicalLeaseTable {
    blocks: SpinNoIrqLock<BTreeSet<u64>>,
}

impl Ext4PhysicalLeaseTable {
    pub(super) fn new() -> Self {
        Self {
            blocks: SpinNoIrqLock::new(BTreeSet::new()),
        }
    }

    pub(super) fn reserve_wait<I>(self: &Arc<Self>, blocks: I) -> Ext4PhysicalLease
    where
        I: IntoIterator<Item = u64>,
    {
        let unique = blocks.into_iter().collect::<BTreeSet<_>>();
        loop {
            let mut reserved = self.blocks.lock();
            if unique.iter().all(|block| !reserved.contains(block)) {
                reserved.extend(unique.iter().copied());
                drop(reserved);
                return Ext4PhysicalLease {
                    table: self.clone(),
                    blocks: unique.iter().copied().collect(),
                };
            }
            drop(reserved);
            suspend_current_and_run_next();
        }
    }
}

pub(super) struct Ext4PhysicalLease {
    table: Arc<Ext4PhysicalLeaseTable>,
    blocks: Vec<u64>,
}

impl Drop for Ext4PhysicalLease {
    fn drop(&mut self) {
        let mut reserved = self.table.blocks.lock();
        for block in self.blocks.drain(..) {
            assert!(
                reserved.remove(&block),
                "ext4 physical LBA lease lost ownership"
            );
        }
    }
}

#[derive(Default)]
struct Ext4BlockVersionState {
    next: usize,
    versions: BTreeMap<u64, usize>,
}

/// Per-physical-sector commit versions used by direct data write plans.
pub(super) struct Ext4BlockVersions {
    state: SpinNoIrqLock<Ext4BlockVersionState>,
}

impl Ext4BlockVersions {
    pub(super) fn new() -> Self {
        Self {
            state: SpinNoIrqLock::new(Ext4BlockVersionState::default()),
        }
    }

    pub(super) fn bump_range(&self, first: u64, count: usize) {
        if count == 0 {
            return;
        }
        let mut state = self.state.lock();
        state.next = state.next.wrapping_add(1);
        assert_ne!(state.next, 0, "ext4 block version counter wrapped");
        let version = state.next;
        for delta in 0..count {
            state.versions.insert(first + delta as u64, version);
        }
    }
}
