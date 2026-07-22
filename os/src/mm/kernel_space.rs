use super::page_table::PTEFlags;
use super::{
    MapArea, MapPermission, MapType, MemorySet, PhysAddr, VirtAddr, VirtPageNum,
    invalidate_global_tlb_range,
};
use crate::config::{PAGE_SIZE, TRAMPOLINE, memory_end, mmio_regions};
use crate::sync::UPIntrFreeCell;
use alloc::sync::Arc;
use lazy_static::*;

unsafe extern "C" {
    safe fn stext();
    safe fn etext();
    safe fn srodata();
    safe fn erodata();
    safe fn sdata();
    safe fn edata();
    safe fn sbss_with_stack();
    safe fn ebss();
    safe fn ekernel();
    safe fn strampoline();
}

lazy_static! {
    pub static ref KERNEL_SPACE: Arc<UPIntrFreeCell<MemorySet>> =
        Arc::new(unsafe { UPIntrFreeCell::new(MemorySet::new_kernel()) });
}

pub fn kernel_token() -> usize {
    KERNEL_SPACE.exclusive_access().token()
}

fn invalidate_global_vpn_range(start_vpn: VirtPageNum, end_vpn: VirtPageNum) {
    let start = usize::from(VirtAddr::from(start_vpn));
    let pages = end_vpn
        .0
        .checked_sub(start_vpn.0)
        .expect("inverted global virtual-page invalidation range");
    let size = pages
        .checked_mul(PAGE_SIZE)
        .expect("global virtual-page invalidation size overflow");
    invalidate_global_tlb_range(start, size);
}

/// Installs a dynamically allocated mapping shared by every kernel page table.
///
/// Remote invalidation deliberately happens after dropping `KERNEL_SPACE`'s
/// interrupt-masking lock. A target CPU can otherwise spin on that same lock
/// with interrupts disabled and be unable to acknowledge the shootdown.
pub(crate) fn insert_global_kernel_framed_area_uninit(
    start_va: VirtAddr,
    end_va: VirtAddr,
    permission: MapPermission,
) -> bool {
    let inserted = KERNEL_SPACE
        .exclusive_access()
        .insert_kernel_private_framed_area_uninit_deferred(start_va, end_va, permission);
    let Some((start_vpn, end_vpn)) = inserted else {
        return false;
    };
    invalidate_global_vpn_range(start_vpn, end_vpn);
    true
}

/// Removes a dynamically allocated mapping shared by every kernel page table.
pub(crate) fn remove_global_kernel_area(start_vpn: VirtPageNum) -> bool {
    let removed = KERNEL_SPACE
        .exclusive_access()
        .remove_area_with_start_vpn_deferred(start_vpn);
    let Some((range_start, range_end, retired)) = removed else {
        return false;
    };
    if retired.pte_cleared() {
        invalidate_global_vpn_range(range_start, range_end);
    }
    retired.release();
    true
}

impl MemorySet {
    pub(super) fn map_trampoline(&mut self) -> bool {
        self.page_table.try_map(
            VirtAddr::from(TRAMPOLINE).into(),
            PhysAddr::from(strampoline as usize).into(),
            PTEFlags::R | PTEFlags::X,
        )
    }

    pub fn new_kernel() -> Self {
        let mut memory_set = Self::new_bare();
        memory_set.map_trampoline();
        memory_set.push(
            MapArea::new(
                (stext as usize).into(),
                (etext as usize).into(),
                MapType::Identical,
                MapPermission::R | MapPermission::X,
            ),
            None,
        );
        memory_set.push(
            MapArea::new(
                (srodata as usize).into(),
                (erodata as usize).into(),
                MapType::Identical,
                MapPermission::R,
            ),
            None,
        );
        memory_set.push(
            MapArea::new(
                (sdata as usize).into(),
                (edata as usize).into(),
                MapType::Identical,
                MapPermission::R | MapPermission::W,
            ),
            None,
        );
        memory_set.push(
            MapArea::new(
                (sbss_with_stack as usize).into(),
                (ebss as usize).into(),
                MapType::Identical,
                MapPermission::R | MapPermission::W,
            ),
            None,
        );
        memory_set.push(
            MapArea::new(
                (ekernel as usize).into(),
                memory_end().into(),
                MapType::Identical,
                MapPermission::R | MapPermission::W,
            ),
            None,
        );
        for pair in mmio_regions() {
            memory_set.push(
                MapArea::new(
                    pair.base.into(),
                    (pair.base + pair.size).into(),
                    MapType::Identical,
                    MapPermission::R | MapPermission::W,
                ),
                None,
            );
        }
        memory_set
    }
}
