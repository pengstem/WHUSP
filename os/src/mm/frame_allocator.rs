use super::{PhysAddr, PhysPageNum};
use crate::config::memory_end;
use crate::sync::UPIntrFreeCell;
use alloc::vec::Vec;
use core::fmt::{self, Debug, Formatter};
use lazy_static::*;

pub struct FrameTracker {
    pub ppn: PhysPageNum,
}

impl FrameTracker {
    pub fn new(ppn: PhysPageNum) -> Self {
        // page cleaning
        let bytes_array = ppn.get_bytes_array();
        for i in bytes_array {
            *i = 0;
        }
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
            return;
        }
        *count = 0;
        // validity check
        if self.recycled.contains(&ppn) {
            panic!("Frame ppn={ppn:#x} has not been allocated!");
        }
        // recycle
        self.recycled.push(ppn);
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

// TODO: Could use core::sync::LazyLock try to replace it
lazy_static! {
    pub static ref FRAME_ALLOCATOR: UPIntrFreeCell<FrameAllocatorImpl> =
        unsafe { UPIntrFreeCell::new(FrameAllocatorImpl::new()) };
}

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
    FRAME_ALLOCATOR
        .exclusive_access()
        .alloc()
        .map(FrameTracker::new)
}

pub fn frame_alloc_more(num: usize) -> Option<Vec<FrameTracker>> {
    FRAME_ALLOCATOR
        .exclusive_access()
        .alloc_more(num)
        .map(|x| x.iter().map(|&t| FrameTracker::new(t)).collect())
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

#[allow(unused)]
pub fn frame_allocator_test() {
    let mut v: Vec<FrameTracker> = Vec::new();
    for i in 0..5 {
        let frame = frame_alloc().unwrap();
        println!("{:?}", frame);
        v.push(frame);
    }
    v.clear();
    for i in 0..5 {
        let frame = frame_alloc().unwrap();
        println!("{:?}", frame);
        v.push(frame);
    }
    drop(v);
    println!("frame_allocator_test passed!");
}

#[allow(unused)]
pub fn frame_allocator_alloc_more_test() {
    let mut v: Vec<FrameTracker> = Vec::new();
    let frames = frame_alloc_more(5).unwrap();
    for frame in &frames {
        println!("{:?}", frame);
    }
    v.extend(frames);
    v.clear();
    let frames = frame_alloc_more(5).unwrap();
    for frame in &frames {
        println!("{:?}", frame);
    }
    drop(v);
    println!("frame_allocator_test passed!");
}
