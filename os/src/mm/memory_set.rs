use super::address::page_align_up;
use super::{
    AddressSpaceControl, MapArea, MapPermission, MapType, MmapFlush, PageTable, PageTableEntry,
    RetiredUserPages, VPNRange, VirtAddr, VirtPageNum, frame_alloc, page_table::PTEFlags,
};
use crate::arch::mm as arch_mm;
use crate::perf;
use alloc::{sync::Arc, vec::Vec};
use core::cell::Cell;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct MemoryMapEntry {
    pub(crate) start: usize,
    pub(crate) end: usize,
    pub(crate) readable: bool,
    pub(crate) writable: bool,
    pub(crate) executable: bool,
    pub(crate) shared: bool,
    pub(crate) offset: usize,
    pub(crate) resident_kb: usize,
    pub(crate) locked_kb: usize,
}

pub struct MemorySet {
    pub(super) page_table: PageTable,
    control: Arc<AddressSpaceControl>,
    // CONTEXT: contest address spaces have a small VMA count today. Keep the
    // VMA list simple until measured mmap pressure justifies an interval tree.
    pub(super) areas: Vec<MapArea>,
    // Cached hit for repeated fault/copy probes. Any area insertion, removal,
    // or full recycle must clear it because `areas` is stored as a sorted Vec.
    pub(super) last_area_idx_containing: Cell<Option<usize>>,
    pub(super) brk_base: usize,
    pub(super) brk: usize,
    pub(super) brk_limit: usize,
    pub(super) brk_mapped_end: usize,
    pub(super) mmap_next: usize,
    pub(super) mlock_future: bool,
    pub(super) mlock_future_on_fault: bool,
}

impl MemorySet {
    pub fn new_bare() -> Self {
        Self {
            page_table: PageTable::new(),
            control: AddressSpaceControl::new(),
            areas: Vec::new(),
            last_area_idx_containing: Cell::new(None),
            brk_base: 0,
            brk: 0,
            brk_limit: 0,
            brk_mapped_end: 0,
            mmap_next: crate::config::USER_MMAP_BASE,
            mlock_future: false,
            mlock_future_on_fault: false,
        }
    }
    pub fn try_new_bare() -> Option<Self> {
        Some(Self {
            page_table: PageTable::try_new()?,
            control: AddressSpaceControl::new(),
            areas: Vec::new(),
            last_area_idx_containing: Cell::new(None),
            brk_base: 0,
            brk: 0,
            brk_limit: 0,
            brk_mapped_end: 0,
            mmap_next: crate::config::USER_MMAP_BASE,
            mlock_future: false,
            mlock_future_on_fault: false,
        })
    }
    pub fn token(&self) -> usize {
        self.page_table.token()
    }

    pub(crate) fn address_space_control(&self) -> Arc<AddressSpaceControl> {
        Arc::clone(&self.control)
    }

    pub(super) fn invalidate_tlb_all(&self) {
        self.control.invalidate_tlb_all();
    }

    pub(super) fn invalidate_tlb_page(&self, virtual_address: usize) {
        self.control.invalidate_tlb_page(virtual_address);
    }

    pub(super) fn invalidate_tlb_vpn_range(&self, start_vpn: VirtPageNum, end_vpn: VirtPageNum) {
        assert!(start_vpn < end_vpn, "empty virtual-page invalidation range");
        let start = usize::from(VirtAddr::from(start_vpn));
        let pages = end_vpn
            .0
            .checked_sub(start_vpn.0)
            .expect("inverted virtual-page invalidation range");
        let size = pages
            .checked_mul(crate::config::PAGE_SIZE)
            .expect("virtual-page invalidation size overflow");
        self.control.invalidate_tlb_range(start, size);
    }
    /// Maps kernel-private framed pages without clearing the new frames.
    ///
    /// Callers must only use this for mappings that are never readable from
    /// user mode and are fully initialized before any kernel read.
    pub(crate) fn insert_kernel_private_framed_area_uninit(
        &mut self,
        start_va: VirtAddr,
        end_va: VirtAddr,
        permission: MapPermission,
    ) -> bool {
        let Some((start_vpn, end_vpn)) =
            self.insert_kernel_private_framed_area_uninit_deferred(start_va, end_va, permission)
        else {
            return false;
        };
        self.invalidate_tlb_vpn_range(start_vpn, end_vpn);
        true
    }

    /// Installs a kernel-only framed range without publishing the PTE edit.
    ///
    /// The caller must invalidate the returned virtual range after releasing
    /// any lock that a remote CPU could be waiting for with interrupts masked.
    pub(crate) fn insert_kernel_private_framed_area_uninit_deferred(
        &mut self,
        start_va: VirtAddr,
        end_va: VirtAddr,
        permission: MapPermission,
    ) -> Option<(VirtPageNum, VirtPageNum)> {
        assert!(
            !permission.contains(MapPermission::U),
            "uninitialized framed pages must stay kernel-private"
        );
        let mut map_area = MapArea::new(start_va, end_va, MapType::Framed, permission);
        let start_vpn = map_area.vpn_range.get_start();
        let end_vpn = map_area.vpn_range.get_end();
        if !map_area.map_uninit(&mut self.page_table) {
            return None;
        }
        self.insert_area_sorted(map_area);
        Some((start_vpn, end_vpn))
    }

    pub(crate) fn insert_lazy_framed_area(
        &mut self,
        start_va: VirtAddr,
        end_va: VirtAddr,
        permission: MapPermission,
    ) {
        self.insert_area_sorted(MapArea::new(start_va, end_va, MapType::Framed, permission));
    }

    /// Materializes already-declared lazy framed user pages.
    ///
    /// This is for exec/user-stack style regions after the VMA exists but
    /// before user pointers are live; mmap and SysV SHM faults must stay on
    /// their dedicated lazy fault paths.
    pub(crate) fn materialize_framed_range(&mut self, start: usize, end: usize) -> bool {
        if start >= end {
            return true;
        }
        let aligned_end = page_align_up(end);
        let start_vpn = VirtAddr::from(start).floor();
        let end_vpn = VirtAddr::from(aligned_end).floor();
        for vpn in VPNRange::new(start_vpn, end_vpn) {
            if self
                .page_table
                .translate(vpn)
                .is_some_and(|pte| pte.bits != 0)
            {
                continue;
            }
            let Some(area_idx) = self.find_area_idx_containing(vpn) else {
                return false;
            };
            let area = &self.areas[area_idx];
            if area.map_type != MapType::Framed
                || area.is_mmap()
                || area.is_shm()
                || !area.map_perm.contains(MapPermission::U)
                || area.data_frames.contains_key(&vpn)
            {
                return false;
            }
            let frame = {
                let _profile_scope =
                    perf::time_scope(perf::ProfilePoint::FrameAllocMaterializeFramed);
                frame_alloc()
            };
            let Some(frame) = frame else {
                return false;
            };
            let page_table = &mut self.page_table;
            let area = &mut self.areas[area_idx];
            if !area.map_existing_frame(page_table, vpn, frame) {
                return false;
            }
        }
        true
    }
    #[cfg(any(target_arch = "riscv64", target_arch = "loongarch64"))]
    pub(crate) fn map_vdso_image(&mut self, start_va: usize, image: &[u8]) -> bool {
        let Some(end_va) = start_va.checked_add(image.len()) else {
            return false;
        };
        if image.is_empty()
            || image.len() % crate::config::PAGE_SIZE != 0
            || self.range_overlaps(start_va, end_va)
        {
            return false;
        }
        self.push(
            MapArea::new(
                start_va.into(),
                end_va.into(),
                MapType::Framed,
                MapPermission::R | MapPermission::X | MapPermission::U,
            ),
            Some(image),
        )
    }

    #[cfg(any(target_arch = "riscv64", target_arch = "loongarch64"))]
    /// Patches fixed data inside the mapped vDSO image without making it writable.
    ///
    /// The write goes through the backing physical page, so callers must keep
    /// the user mapping R/X/U-only and restrict this to kernel-owned vDSO data.
    pub(crate) fn patch_vdso_u64(&mut self, start_va: usize, offset: usize, value: u64) -> bool {
        let Some(va) = start_va.checked_add(offset) else {
            return false;
        };
        let Some(end) = va.checked_add(core::mem::size_of::<u64>()) else {
            return false;
        };
        if end > start_va.saturating_add(crate::config::PAGE_SIZE) {
            return false;
        }
        let va = VirtAddr::from(va);
        let Some(pte) = self.page_table.translate(va.floor()) else {
            return false;
        };
        let flags = pte.flags();
        if !flags.contains(PTEFlags::R | PTEFlags::X | PTEFlags::U) || flags.contains(PTEFlags::W) {
            return false;
        }
        let page_offset = va.page_offset();
        let bytes = pte.ppn().get_bytes_array();
        bytes[page_offset..page_offset + core::mem::size_of::<u64>()]
            .copy_from_slice(&value.to_le_bytes());
        true
    }
    pub fn remove_area_with_start_vpn(&mut self, start_vpn: VirtPageNum) {
        let Some((range_start, range_end, retired)) =
            self.remove_area_with_start_vpn_deferred(start_vpn)
        else {
            return;
        };
        if retired.pte_cleared() {
            self.invalidate_tlb_vpn_range(range_start, range_end);
        }
        retired.release();
    }

    /// Clears an area's PTEs while retaining all backing resources.
    ///
    /// The caller must invalidate the returned range before releasing the
    /// retired resources. This split lets global kernel mappings wait for
    /// remote CPUs without holding `KERNEL_SPACE`'s interrupt-masking lock.
    pub(crate) fn remove_area_with_start_vpn_deferred(
        &mut self,
        start_vpn: VirtPageNum,
    ) -> Option<(VirtPageNum, VirtPageNum, RetiredUserPages)> {
        let idx = self.find_area_idx_by_start(start_vpn)?;
        let mut area = self.areas.remove(idx);
        let range_start = area.vpn_range.get_start();
        let range_end = area.vpn_range.get_end();
        let mut retired = RetiredUserPages::new();
        area.unmap_resident_deferred(&mut self.page_table, &mut retired);
        self.last_area_idx_containing.set(None);
        Some((range_start, range_end, retired))
    }
    /// Add a new MapArea into this MemorySet.
    /// Assuming that there are no conflicts in the virtual address
    /// space.
    pub fn push(&mut self, map_area: MapArea, data: Option<&[u8]>) -> bool {
        self.push_with_offset(map_area, data, 0)
    }

    pub(super) fn push_with_offset(
        &mut self,
        mut map_area: MapArea,
        data: Option<&[u8]>,
        data_offset: usize,
    ) -> bool {
        if !map_area.map(&mut self.page_table) {
            return false;
        }
        if let Some(data) = data {
            map_area.copy_data(&self.page_table, data, data_offset);
            if map_area.is_executable() {
                arch_mm::publish_pte_barrier();
                arch_mm::instruction_barrier();
            }
        }
        self.insert_area_sorted(map_area);
        true
    }

    pub(super) fn insert_area_sorted(&mut self, map_area: MapArea) -> usize {
        // The binary-search lookup below depends on this sorted-by-start
        // invariant. Use this helper instead of pushing directly into areas.
        let idx = self.area_insert_index(map_area.vpn_range.get_start());
        self.areas.insert(idx, map_area);
        self.last_area_idx_containing.set(None);
        idx
    }

    pub(super) fn area_insert_index(&self, start_vpn: VirtPageNum) -> usize {
        let mut low = 0usize;
        let mut high = self.areas.len();
        while low < high {
            let mid = (low + high) / 2;
            if self.areas[mid].vpn_range.get_start() < start_vpn {
                low = mid + 1;
            } else {
                high = mid;
            }
        }
        low
    }

    pub(super) fn find_area_idx_by_start(&self, start_vpn: VirtPageNum) -> Option<usize> {
        let idx = self.area_insert_index(start_vpn);
        (idx < self.areas.len() && self.areas[idx].vpn_range.get_start() == start_vpn)
            .then_some(idx)
    }

    pub(super) fn find_area_idx_containing(&self, vpn: VirtPageNum) -> Option<usize> {
        if let Some(idx) = self.last_area_idx_containing.get()
            && let Some(area) = self.areas.get(idx)
            && area.vpn_range.get_start() <= vpn
            && vpn < area.vpn_range.get_end()
        {
            perf::record_vma_lookup(1, true);
            return Some(idx);
        }

        let mut low = 0usize;
        let mut high = self.areas.len();
        let mut probes = 0usize;
        while low < high {
            let mid = (low + high) / 2;
            probes += 1;
            if self.areas[mid].vpn_range.get_start() <= vpn {
                low = mid + 1;
            } else {
                high = mid;
            }
        }
        let idx = low.checked_sub(1);
        let hit = idx.is_some_and(|idx| {
            probes += 1;
            vpn < self.areas[idx].vpn_range.get_end()
        });
        let result = hit.then(|| idx.expect("hit requires predecessor area"));
        self.last_area_idx_containing.set(result);
        perf::record_vma_lookup(probes, hit);
        result
    }
    pub fn activate(&self) {
        arch_mm::activate_page_table(self.page_table.token());
    }
    pub fn translate(&self, vpn: VirtPageNum) -> Option<PageTableEntry> {
        self.page_table.translate(vpn)
    }
    pub fn remap_existing_page_flags(&mut self, vpn: VirtPageNum, flags: PTEFlags) -> bool {
        if !self.page_table.remap_flags(vpn, flags) {
            return false;
        }
        self.invalidate_tlb_page(usize::from(VirtAddr::from(vpn)));
        true
    }
    /// Tears down user mappings and returns deferred file-backed writebacks.
    ///
    /// Callers must write the returned flushes and drop the returned areas
    /// after releasing the process memory-set lock. Both operations can enter
    /// VFS file and mount cleanup paths that may sleep.
    pub fn recycle_data_pages(&mut self) -> (Vec<MmapFlush>, Vec<MapArea>) {
        let mut flushes = Vec::new();
        let mut retired = RetiredUserPages::new();
        for area in &mut self.areas {
            if area.is_mmap() || area.is_shm() || area.map_type == MapType::Framed {
                flushes.extend(area.take_mmap_flushes(&mut self.page_table, &mut retired));
                area.release_mmap_refs();
            } else {
                area.unmap_resident_deferred(&mut self.page_table, &mut retired);
            }
        }
        if retired.pte_cleared() {
            self.invalidate_tlb_all();
        }
        retired.release();
        let retired_areas = core::mem::take(&mut self.areas);
        self.last_area_idx_containing.set(None);
        (flushes, retired_areas)
    }

    pub(crate) fn proc_maps_entries(&self) -> Vec<MemoryMapEntry> {
        let mut entries: Vec<_> = self
            .areas
            .iter()
            .map(|area| {
                let start_va: VirtAddr = area.vpn_range.get_start().into();
                let end_va: VirtAddr = area.vpn_range.get_end().into();
                let reported_perm = area
                    .mmap_info
                    .as_ref()
                    .map_or(area.map_perm, |info| info.reported_perm);
                MemoryMapEntry {
                    start: usize::from(start_va),
                    end: usize::from(end_va),
                    readable: reported_perm.contains(MapPermission::R),
                    writable: reported_perm.contains(MapPermission::W),
                    executable: reported_perm.contains(MapPermission::X),
                    shared: area.mmap_info.as_ref().is_some_and(|info| info.shared)
                        || area.is_shm(),
                    offset: area.mmap_info.as_ref().map_or(0, |info| info.file_offset),
                    resident_kb: area.resident_bytes(&self.page_table) / 1024,
                    locked_kb: area.locked_bytes() / 1024,
                }
            })
            .collect();
        entries.sort_by_key(|entry| entry.start);
        entries
    }

    pub(crate) fn resident_bytes(&self) -> usize {
        self.areas
            .iter()
            .map(|area| area.resident_bytes(&self.page_table))
            .sum()
    }
}

impl Drop for MemorySet {
    fn drop(&mut self) {
        let mut retired = RetiredUserPages::new();
        for area in &mut self.areas {
            area.release_mmap_refs();
            area.unmap_resident_deferred(&mut self.page_table, &mut retired);
        }
        if retired.pte_cleared() {
            self.invalidate_tlb_all();
        }
        retired.release();
        self.areas.clear();
    }
}
