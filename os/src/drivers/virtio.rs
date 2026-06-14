use crate::mm::{
    FrameTracker, PageTable, PhysAddr, PhysPageNum, VirtAddr, frame_alloc_more, kernel_token,
};
use crate::sync::UPIntrFreeCell;
use alloc::collections::BTreeMap;
use alloc::vec::Vec;
use core::ptr::NonNull;
use lazy_static::*;
use virtio_drivers::{
    BufferDirection, Hal, PhysAddr as VirtioPhysAddr,
    transport::{
        SomeTransport,
        mmio::{MmioTransport, VirtIOHeader},
    },
};

pub type VirtioTransport = SomeTransport<'static>;

pub fn mmio_transport(base_addr: usize, size: usize) -> VirtioTransport {
    let header = NonNull::new(base_addr as *mut VirtIOHeader).unwrap();
    unsafe { MmioTransport::new(header, size).expect("failed to create virtio MMIO transport") }
        .into()
}

lazy_static! {
    // DMA queue frames stay owned here until virtio dealloc; returning them to
    // the general allocator earlier would leave device-visible memory aliased.
    static ref QUEUE_FRAMES: UPIntrFreeCell<Vec<FrameTracker>> =
        unsafe { UPIntrFreeCell::new(Vec::new()) };
    static ref SHARED_BOUNCES: UPIntrFreeCell<BTreeMap<VirtioPhysAddr, Vec<FrameTracker>>> =
        unsafe { UPIntrFreeCell::new(BTreeMap::new()) };
}

pub struct VirtioHal;

unsafe impl Hal for VirtioHal {
    fn dma_alloc(pages: usize, _direction: BufferDirection) -> (VirtioPhysAddr, NonNull<u8>) {
        let mut trackers = frame_alloc_more(pages).expect("failed to allocate virtio DMA frames");
        // frame_alloc_more returns a contiguous run; the virtio HAL ABI passes
        // only the base physical address for the whole DMA allocation.
        let ppn_base = trackers
            .iter()
            .map(|tracker| tracker.ppn)
            .min()
            .expect("virtio DMA allocation returned no frames");
        let pa: PhysAddr = ppn_base.into();
        let ptr = NonNull::new({ pa.0 } as *mut u8).unwrap();
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
        NonNull::new({ paddr as usize } as *mut u8).unwrap()
    }

    unsafe fn share(buffer: NonNull<[u8]>, direction: BufferDirection) -> VirtioPhysAddr {
        // Virtio buffers are kernel virtual addresses. Translate through the
        // kernel page table instead of assuming direct virtual=physical layout.
        let vaddr = buffer.as_ptr() as *mut u8 as usize;
        let len = buffer.len();
        if virt_range_is_phys_contiguous(vaddr, len) {
            return virt_to_phys(vaddr) as VirtioPhysAddr;
        }

        let pages = len.div_ceil(crate::config::PAGE_SIZE);
        let trackers = frame_alloc_more(pages).expect("failed to allocate virtio bounce frames");
        let ppn_base = trackers
            .iter()
            .map(|tracker| tracker.ppn)
            .min()
            .expect("virtio bounce allocation returned no frames");
        let pa: PhysAddr = ppn_base.into();
        let bounce = unsafe { core::slice::from_raw_parts_mut(pa.0 as *mut u8, len) };
        let mut buffer = buffer;
        let original = unsafe { buffer.as_mut() };
        if matches!(
            direction,
            BufferDirection::DriverToDevice | BufferDirection::Both
        ) {
            bounce.copy_from_slice(original);
        }
        SHARED_BOUNCES
            .exclusive_access()
            .insert(pa.0 as VirtioPhysAddr, trackers);
        pa.0 as VirtioPhysAddr
    }

    unsafe fn unshare(paddr: VirtioPhysAddr, buffer: NonNull<[u8]>, direction: BufferDirection) {
        let frames = SHARED_BOUNCES.exclusive_access().remove(&paddr);
        let Some(frames) = frames else {
            return;
        };
        let len = buffer.len();
        if matches!(
            direction,
            BufferDirection::DeviceToDriver | BufferDirection::Both
        ) {
            let bounce = unsafe { core::slice::from_raw_parts(paddr as *const u8, len) };
            let mut buffer = buffer;
            let original = unsafe { buffer.as_mut() };
            original.copy_from_slice(bounce);
        }
        drop(frames);
    }
}

fn virt_to_phys(vaddr: usize) -> usize {
    PageTable::from_token(kernel_token())
        .translate_va(VirtAddr::from(vaddr))
        .unwrap()
        .0
}

fn virt_range_is_phys_contiguous(vaddr: usize, len: usize) -> bool {
    if len == 0 {
        return true;
    }
    let page_size = crate::config::PAGE_SIZE;
    let first_page = vaddr & !(page_size - 1);
    let last_page = (vaddr + len - 1) & !(page_size - 1);
    let mut page = first_page;
    let mut expected_pa = virt_to_phys(first_page);
    while page <= last_page {
        if virt_to_phys(page) != expected_pa {
            return false;
        }
        page += page_size;
        expected_pa += page_size;
    }
    true
}
