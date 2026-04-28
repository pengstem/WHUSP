mod address;
mod area;
mod elf_loader;
mod frame_allocator;
mod heap_allocator;
mod kernel_space;
mod memory_set;
pub mod page_table;
mod user_space;

pub use address::VPNRange;
pub use address::{PhysAddr, PhysPageNum, StepByOne, VirtAddr, VirtPageNum};
pub use area::{MapArea, MapPermission, MapType, MmapFlush};
pub use elf_loader::ElfLoadInfo;
pub use frame_allocator::{FrameTracker, frame_alloc, frame_alloc_more};
pub use kernel_space::{KERNEL_SPACE, kernel_token};
pub use memory_set::MemorySet;
pub use page_table::{
    PageTable, PageTableEntry, UserBuffer, translated_byte_buffer, translated_ref,
    translated_refmut, translated_str,
};
pub use user_space::{MemoryProtectError, MmapFaultAccess, MmapFaultResult};

pub fn init() {
    heap_allocator::init_heap();
    frame_allocator::init_frame_allocator();
    KERNEL_SPACE.exclusive_access().activate();
}
