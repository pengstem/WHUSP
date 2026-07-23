mod address;
mod address_space;
mod area;
mod elf_loader;
mod frame_allocator;
mod heap_allocator;
mod kernel_space;
mod memory_set;
pub(crate) mod page_cache;
pub mod page_table;
pub(crate) mod shm;
mod user_space;

use core::sync::atomic::{AtomicUsize, Ordering};

pub use address::VPNRange;
pub use address::{PhysAddr, PhysPageNum, StepByOne, VirtAddr, VirtPageNum};
pub(crate) use address_space::{
    ActiveAddressSpace, AddressSpaceControl, invalidate_global_tlb_range,
};
pub(crate) use area::RetiredUserPages;
pub use area::{MapArea, MapPermission, MapType, MmapFlush};
pub use elf_loader::ElfLoadInfo;
pub(crate) use elf_loader::{exec_load_stats_content, record_exec_metadata_read};
pub use frame_allocator::{
    FrameTracker, frame_alloc, frame_alloc_more, frame_alloc_uninit, frame_ref_count, frame_stats,
};
pub use kernel_space::{KERNEL_SPACE, kernel_token};
pub(crate) use kernel_space::{insert_global_kernel_framed_area_uninit, remove_global_kernel_area};
pub use memory_set::MemorySet;
pub(crate) use page_table::TranslatedUserBuffer;
pub use page_table::{PageTable, PageTableEntry, UserBuffer};
pub(crate) use user_space::FutexSharedKey;
pub use user_space::{MemoryProtectError, MmapFaultAccess, MmapFaultResult};

static PUBLISHED_KERNEL_TOKEN: AtomicUsize = AtomicUsize::new(0);
static KERNEL_MAP_ELAPSED_US: AtomicUsize = AtomicUsize::new(0);

#[derive(Clone, Copy, Debug)]
pub struct KernelMappingStats {
    pub elapsed_us: usize,
    pub page_table_frames: usize,
    pub leaves_4k: usize,
    pub leaves_2m: usize,
    pub leaves_1g: usize,
}

pub fn init() {
    heap_allocator::init_heap();
    frame_allocator::init_frame_allocator();
    let start_us = crate::timer::get_time_us();
    let kernel_space = KERNEL_SPACE.exclusive_access();
    kernel_space.activate();
    let token = kernel_space.token();
    drop(kernel_space);
    KERNEL_MAP_ELAPSED_US.store(
        crate::timer::get_time_us().saturating_sub(start_us),
        Ordering::Relaxed,
    );
    PUBLISHED_KERNEL_TOKEN.store(token, Ordering::Release);
}

pub fn kernel_mapping_stats() -> KernelMappingStats {
    let page_table = KERNEL_SPACE.exclusive_access().page_table_stats();
    KernelMappingStats {
        elapsed_us: KERNEL_MAP_ELAPSED_US.load(Ordering::Relaxed),
        page_table_frames: page_table.frames,
        leaves_4k: page_table.leaves_4k,
        leaves_2m: page_table.leaves_2m,
        leaves_1g: page_table.leaves_1g,
    }
}

pub fn activate_kernel_page_table_for_secondary() {
    let token = PUBLISHED_KERNEL_TOKEN.load(Ordering::Acquire);
    assert_ne!(
        token, 0,
        "kernel page table was not published before CPU start"
    );
    crate::arch::mm::activate_page_table(token);
}
