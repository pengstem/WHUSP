use super::{
    MapArea, MapPermission, MapType, MmapFlush, PageTable, PageTableEntry, VirtAddr, VirtPageNum,
    page_table::PTEFlags,
};
use crate::arch::mm as arch_mm;
use crate::perf;
use alloc::vec::Vec;
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
    /// Assume that no conflicts.
    pub fn insert_framed_area(
        &mut self,
        start_va: VirtAddr,
        end_va: VirtAddr,
        permission: MapPermission,
    ) {
        let _ = self.push(
            MapArea::new(start_va, end_va, MapType::Framed, permission),
            None,
        );
    }
    #[cfg(target_arch = "riscv64")]
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
    pub fn remove_area_with_start_vpn(&mut self, start_vpn: VirtPageNum) {
        if let Some(idx) = self.find_area_idx_by_start(start_vpn) {
            let area = &mut self.areas[idx];
            if area.is_mmap() || area.is_shm() {
                area.unmap_resident(&mut self.page_table);
            } else {
                area.unmap(&mut self.page_table);
            }
            self.areas.remove(idx);
            self.last_area_idx_containing.set(None);
        }
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
        if let Some(idx) = self.last_area_idx_containing.get() {
            if let Some(area) = self.areas.get(idx) {
                if area.vpn_range.get_start() <= vpn && vpn < area.vpn_range.get_end() {
                    perf::record_vma_lookup(1, true);
                    return Some(idx);
                }
            }
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
        self.page_table.remap_flags(vpn, flags)
    }
    pub fn recycle_data_pages(&mut self) -> Vec<MmapFlush> {
        let mut flushes = Vec::new();
        for area in &mut self.areas {
            if area.is_mmap() {
                flushes.extend(area.take_mmap_flushes(&mut self.page_table));
                area.release_mmap_refs();
            } else if area.is_shm() {
                area.unmap_resident(&mut self.page_table);
            } else {
                area.unmap(&mut self.page_table);
            }
        }
        self.areas.clear();
        self.last_area_idx_containing.set(None);
        flushes
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
        for area in &mut self.areas {
            area.release_mmap_refs();
            area.unmap_resident(&mut self.page_table);
        }
        self.areas.clear();
    }
}
