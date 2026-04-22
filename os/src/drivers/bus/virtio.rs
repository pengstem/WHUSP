use crate::mm::{
    FrameTracker, PageTable, PhysAddr, PhysPageNum, VirtAddr, frame_alloc_more, kernel_token,
};
use crate::sync::UPIntrFreeCell;
use alloc::vec::Vec;
use core::ptr::NonNull;
use lazy_static::*;
use virtio_drivers::{
    BufferDirection, Hal, PhysAddr as VirtioPhysAddr,
    transport::mmio::{MmioTransport, VirtIOHeader},
};

pub type VirtioTransport = MmioTransport<'static>;

pub fn mmio_transport(base_addr: usize, size: usize) -> VirtioTransport {
    let header = NonNull::new(base_addr as *mut VirtIOHeader).unwrap();
    unsafe { MmioTransport::new(header, size).expect("failed to create virtio MMIO transport") }
}

lazy_static! {
    static ref QUEUE_FRAMES: UPIntrFreeCell<Vec<FrameTracker>> =
        unsafe { UPIntrFreeCell::new(Vec::new()) };
}

pub struct VirtioHal;

unsafe impl Hal for VirtioHal {
    fn dma_alloc(pages: usize, _direction: BufferDirection) -> (VirtioPhysAddr, NonNull<u8>) {
        let mut trackers = frame_alloc_more(pages).expect("failed to allocate virtio DMA frames");
        let ppn_base = trackers
            .iter()
            .map(|tracker| tracker.ppn)
            .min()
            .expect("virtio DMA allocation returned no frames");
        let pa: PhysAddr = ppn_base.into();
        let ptr = NonNull::new(pa.0 as *mut u8).unwrap();
        QUEUE_FRAMES.exclusive_access().append(&mut trackers);
        (pa.0 as VirtioPhysAddr, ptr)
    }

    unsafe fn dma_dealloc(paddr: VirtioPhysAddr, _vaddr: NonNull<u8>, pages: usize) -> i32 {
        let ppn_base: PhysPageNum = PhysAddr::from(paddr as usize).into();
        let ppn_end = ppn_base.0 + pages;
        let mut frames = QUEUE_FRAMES.exclusive_access();
        let mut index = 0;
        while index < frames.len() {
            let ppn = frames[index].ppn.0;
            if ppn >= ppn_base.0 && ppn < ppn_end {
                frames.swap_remove(index);
            } else {
                index += 1;
            }
        }
        0
    }

    unsafe fn mmio_phys_to_virt(paddr: VirtioPhysAddr, _size: usize) -> NonNull<u8> {
        NonNull::new(paddr as usize as *mut u8).unwrap()
    }

    unsafe fn share(buffer: NonNull<[u8]>, _direction: BufferDirection) -> VirtioPhysAddr {
        virt_to_phys(buffer.as_ptr() as *mut u8 as usize) as VirtioPhysAddr
    }

    unsafe fn unshare(_paddr: VirtioPhysAddr, _buffer: NonNull<[u8]>, _direction: BufferDirection) {
    }
}

fn virt_to_phys(vaddr: usize) -> usize {
    PageTable::from_token(kernel_token())
        .translate_va(VirtAddr::from(vaddr))
        .unwrap()
        .0
}

#[allow(unused)]
fn phys_to_virt(paddr: VirtioPhysAddr) -> usize {
    paddr as usize
}
