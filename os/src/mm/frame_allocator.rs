use super::{PhysAddr, PhysPageNum};
use crate::config::memory_end;
use crate::perf;
use crate::sync::UPIntrFreeCell;
use alloc::vec::Vec;
use core::fmt::{self, Debug, Formatter};
use lazy_static::*;

pub struct FrameTracker {
    pub ppn: PhysPageNum,
}

impl FrameTracker {
    pub fn new_zeroed(ppn: PhysPageNum) -> Self {
        let _profile_scope = perf::time_scope(perf::ProfilePoint::FrameZeroFill);
        ppn.get_bytes_array().fill(0);
        perf::record_frame_alloc(true);
        Self { ppn }
    }

    pub fn new_uninit(ppn: PhysPageNum) -> Self {
        perf::record_frame_alloc(false);
        Self { ppn }
    }

    pub fn from_retained(ppn: PhysPageNum) -> Option<Self> {
        frame_retain(ppn).then_some(Self { ppn })
    }
}

impl Debug for FrameTracker {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.write_fmt(format_args!("FrameTracker:PPN={:#x}", self.ppn.0))
    }
}

impl Drop for FrameTracker {
    fn drop(&mut self) {
        frame_dealloc(self.ppn);
    }
}

trait FrameAllocator {
    fn new() -> Self;
    fn alloc(&mut self) -> Option<PhysPageNum>;
    fn alloc_more(&mut self, pages: usize) -> Option<Vec<PhysPageNum>>;
    fn dealloc(&mut self, ppn: PhysPageNum);
}

pub struct StackFrameAllocator {
    start: usize,
    current: usize,
    end: usize,
    recycled: Vec<usize>,
    ref_counts: Vec<usize>,
}

impl StackFrameAllocator {
    pub fn init(&mut self, l: PhysPageNum, r: PhysPageNum) {
        self.start = l.0;
        self.current = l.0;
        self.end = r.0;
        self.ref_counts.clear();
        self.ref_counts
            .resize(self.end.saturating_sub(self.start), 0);
        // println!("last {} Physical Frames.", self.end - self.current);
    }

    pub fn stats(&self) -> (usize, usize) {
        let total = self.end.saturating_sub(self.start);
        let free = self
            .end
            .saturating_sub(self.current)
            .saturating_add(self.recycled.len());
        (total, free)
    }
}
impl FrameAllocator for StackFrameAllocator {
    fn new() -> Self {
        Self {
            start: 0,
            current: 0,
            end: 0,
            recycled: Vec::new(),
            ref_counts: Vec::new(),
        }
    }
    fn alloc(&mut self) -> Option<PhysPageNum> {
        let ppn = if let Some(ppn) = self.recycled.pop() {
            ppn
        } else if self.current == self.end {
            return None;
        } else {
            self.current += 1;
            self.current - 1
        };
        self.ref_counts[ppn - self.start] = 1;
        Some(ppn.into())
    }
    fn alloc_more(&mut self, pages: usize) -> Option<Vec<PhysPageNum>> {
        if self.current + pages >= self.end {
            None
        } else {
            self.current += pages;
            let arr: Vec<usize> = (1..pages + 1).collect();
            let v = arr
                .iter()
                .map(|x| {
                    let ppn = self.current - x;
                    self.ref_counts[ppn - self.start] = 1;
                    ppn.into()
                })
                .collect();
            Some(v)
        }
    }
    fn dealloc(&mut self, ppn: PhysPageNum) {
        let ppn = ppn.0;
        if ppn < self.start || ppn >= self.current {
            panic!("Frame ppn={ppn:#x} has not been allocated!");
        }
        let count = &mut self.ref_counts[ppn - self.start];
        if *count == 0 {
            panic!("Frame ppn={ppn:#x} has no reference count!");
        }
        if *count > 1 {
            *count -= 1;
            perf::record_frame_dealloc(false, true, 0, self.recycled.len());
            return;
        }
        *count = 0;
        // Refcount zero is the double-free guard; scanning the free list makes
        // every dealloc proportional to the number of recycled frames.
        self.recycled.push(ppn);
        perf::record_frame_dealloc(true, false, 0, self.recycled.len());
    }
}

impl StackFrameAllocator {
    fn retain(&mut self, ppn: PhysPageNum) -> bool {
        let ppn = ppn.0;
        if ppn < self.start || ppn >= self.current {
            return false;
        }
        let count = &mut self.ref_counts[ppn - self.start];
        if *count == 0 {
            return false;
        }
        *count += 1;
        true
    }

    fn ref_count(&self, ppn: PhysPageNum) -> Option<usize> {
        let ppn = ppn.0;
        if ppn < self.start || ppn >= self.current {
            return None;
        }
        let count = self.ref_counts[ppn - self.start];
        (count > 0).then_some(count)
    }
}

type FrameAllocatorImpl = StackFrameAllocator;

// The allocator is initialized after DTB memory discovery; UPIntrFreeCell keeps
// frame metadata updates atomic with interrupts masked on this uniprocessor.
lazy_static! {
    pub static ref FRAME_ALLOCATOR: UPIntrFreeCell<FrameAllocatorImpl> =
        unsafe { UPIntrFreeCell::new(FrameAllocatorImpl::new()) };
}

/// what the hell
pub fn init_frame_allocator() {
    unsafe extern "C" {
        safe fn ekernel();
    }
    FRAME_ALLOCATOR.exclusive_access().init(
        PhysAddr::from(ekernel as usize).ceil(),
        PhysAddr::from(memory_end()).floor(),
    );
}

pub fn frame_alloc() -> Option<FrameTracker> {
    let _profile_scope = perf::time_scope(perf::ProfilePoint::FrameAllocZeroed);
    FRAME_ALLOCATOR
        .exclusive_access()
        .alloc()
        .map(FrameTracker::new_zeroed)
}

pub fn frame_alloc_uninit() -> Option<FrameTracker> {
    let _profile_scope = perf::time_scope(perf::ProfilePoint::FrameAllocUninit);
    FRAME_ALLOCATOR
        .exclusive_access()
        .alloc()
        .map(FrameTracker::new_uninit)
}

/// Allocates a contiguous fresh page run for device DMA queues.
///
/// This path intentionally does not satisfy requests from recycled single
/// pages; callers such as VirtIO pass the first physical address to hardware.
pub fn frame_alloc_more(num: usize) -> Option<Vec<FrameTracker>> {
    let _profile_scope = perf::time_scope(perf::ProfilePoint::FrameAllocDma);
    FRAME_ALLOCATOR
        .exclusive_access()
        .alloc_more(num)
        .map(|x| x.into_iter().map(FrameTracker::new_zeroed).collect())
}

pub fn frame_dealloc(ppn: PhysPageNum) {
    FRAME_ALLOCATOR.exclusive_access().dealloc(ppn);
}

pub fn frame_retain(ppn: PhysPageNum) -> bool {
    FRAME_ALLOCATOR.exclusive_access().retain(ppn)
}

pub fn frame_ref_count(ppn: PhysPageNum) -> Option<usize> {
    FRAME_ALLOCATOR.exclusive_access().ref_count(ppn)
}

pub fn frame_stats() -> (usize, usize) {
    FRAME_ALLOCATOR.exclusive_access().stats()
}
