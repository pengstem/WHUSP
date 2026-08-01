use super::page_table::PTEFlags;
use super::{
    MapArea, MapPermission, MapType, MemorySet, PhysAddr, VirtAddr, VirtPageNum,
    invalidate_global_tlb_range,
};
#[cfg(target_arch = "riscv64")]
use crate::config::{KERNEL_STACK_TOP, USER_MMAP_BASE};
use crate::config::{PAGE_SIZE, TRAMPOLINE, memory_end, mmio_regions};
use crate::sync::SpinNoIrqLock;
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
    pub static ref KERNEL_SPACE: Arc<SpinNoIrqLock<MemorySet>> =
        Arc::new(SpinNoIrqLock::new(MemorySet::new_kernel()));
}

pub fn kernel_token() -> usize {
    KERNEL_SPACE.lock().token()
}

#[cfg(not(target_arch = "riscv64"))]
pub(super) fn install_kernel_mappings_into_user(_memory_set: &mut MemorySet) -> bool {
    true
}

/// Makes the current process root usable in S-mode without a trap-time SATP
/// switch. Kernel RAM and the kernel-stack subtree are referenced through
/// kernel-owned root entries. MMIO is exposed through one shared high-address
/// alias so device leaves cannot occupy the low root entries used by ELF LOAD
/// segments.
#[cfg(target_arch = "riscv64")]
pub(super) fn install_kernel_mappings_into_user(memory_set: &mut MemorySet) -> bool {
    let first_ram_root = VirtAddr::from(stext as usize).floor().indexes()[0];
    let last_ram_root = VirtAddr::from(memory_end() - 1).floor().indexes()[0];
    let kernel_stack_root = VirtAddr::from(KERNEL_STACK_TOP - PAGE_SIZE)
        .floor()
        .indexes()[0];
    let first_mmio = mmio_regions()
        .first()
        .expect("single-SATP mode requires at least one MMIO region");
    let mmio_root = VirtAddr::from(crate::arch::mm::mmio_phys_to_virt(first_mmio.base))
        .floor()
        .indexes()[0];
    assert!(
        mmio_regions().iter().all(|range| {
            let start = crate::arch::mm::mmio_phys_to_virt(range.base);
            let end = start
                .checked_add(range.size)
                .expect("RISC-V MMIO virtual range overflow");
            VirtAddr::from(start).floor().indexes()[0] == mmio_root
                && VirtAddr::from(end - 1).floor().indexes()[0] == mmio_root
        }),
        "RISC-V MMIO aliases must share one Sv39 root entry"
    );
    {
        let kernel_space = KERNEL_SPACE.lock();
        for index in first_ram_root..=last_ram_root {
            let entry = kernel_space.page_table.root_entry(index);
            if !memory_set
                .page_table
                .install_shared_root_entry(index, entry)
            {
                return false;
            }
        }
        let stack_entry = kernel_space.page_table.root_entry(kernel_stack_root);
        if !memory_set
            .page_table
            .install_shared_root_entry(kernel_stack_root, stack_entry)
        {
            return false;
        }
        let mmio_entry = kernel_space.page_table.root_entry(mmio_root);
        if !memory_set
            .page_table
            .install_shared_root_entry(mmio_root, mmio_entry)
        {
            return false;
        }
    }
    true
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
        .lock()
        .insert_kernel_private_framed_area_uninit_deferred(start_va, end_va, permission);
    let Some((start_vpn, end_vpn)) = inserted else {
        return false;
    };
    invalidate_global_vpn_range(start_vpn, end_vpn);
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
        #[cfg(not(target_arch = "riscv64"))]
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
        #[cfg(target_arch = "riscv64")]
        for pair in mmio_regions() {
            let virtual_start = crate::arch::mm::mmio_phys_to_virt(pair.base);
            let virtual_end = virtual_start
                .checked_add(pair.size)
                .expect("RISC-V MMIO virtual range overflow");
            let start_vpn = VirtAddr::from(virtual_start).floor();
            let end_vpn = VirtAddr::from(virtual_end).ceil();
            let start_ppn = PhysAddr::from(pair.base).floor();
            assert_eq!(
                start_vpn.indexes()[0],
                VirtAddr::from(virtual_end - 1).floor().indexes()[0],
                "RISC-V MMIO alias crossed an Sv39 root boundary"
            );
            assert!(
                memory_set.page_table.try_map_kernel_linear_range(
                    start_vpn,
                    start_ppn,
                    end_vpn,
                    PTEFlags::R | PTEFlags::W | PTEFlags::G,
                ),
                "failed to map RISC-V MMIO high alias"
            );
        }
        #[cfg(target_arch = "riscv64")]
        {
            assert!(
                memory_end() <= USER_MMAP_BASE,
                "single-SATP kernel direct map overlaps automatic user mmap range"
            );
            assert!(
                memory_set
                    .page_table
                    .prepare_empty_leaf_path(VirtAddr::from(KERNEL_STACK_TOP - PAGE_SIZE).floor()),
                "failed to prepare shared kernel-stack page-table subtree"
            );
        }
        memory_set
    }
}
