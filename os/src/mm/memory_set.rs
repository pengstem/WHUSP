use super::{
    MapArea, MapPermission, MapType, MmapFlush, PageTable, PageTableEntry, VirtAddr, VirtPageNum,
    page_table::PTEFlags,
};
use crate::arch::mm as arch_mm;
use alloc::vec::Vec;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct MemoryMapEntry {
    pub(crate) start: usize,
    pub(crate) end: usize,
    pub(crate) readable: bool,
    pub(crate) writable: bool,
    pub(crate) executable: bool,
    pub(crate) shared: bool,
    pub(crate) offset: usize,
}

// TODO: replace vec to a high perfermonce data structure
pub struct MemorySet {
    pub(super) page_table: PageTable,
    pub(super) areas: Vec<MapArea>,
    pub(super) brk_base: usize,
    pub(super) brk: usize,
    pub(super) brk_limit: usize,
    pub(super) brk_mapped_end: usize,
    pub(super) mmap_next: usize,
}

impl MemorySet {
    pub fn new_bare() -> Self {
        Self {
            page_table: PageTable::new(),
            areas: Vec::new(),
            brk_base: 0,
            brk: 0,
            brk_limit: 0,
            brk_mapped_end: 0,
            mmap_next: crate::config::USER_MMAP_BASE,
        }
    }
    pub fn try_new_bare() -> Option<Self> {
        Some(Self {
            page_table: PageTable::try_new()?,
            areas: Vec::new(),
            brk_base: 0,
            brk: 0,
            brk_limit: 0,
            brk_mapped_end: 0,
            mmap_next: crate::config::USER_MMAP_BASE,
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
    pub fn remove_area_with_start_vpn(&mut self, start_vpn: VirtPageNum) {
        if let Some((idx, area)) = self
            .areas
            .iter_mut()
            .enumerate()
            .find(|(_, area)| area.vpn_range.get_start() == start_vpn)
        {
            if area.is_mmap() || area.is_shm() {
                area.unmap_resident(&mut self.page_table);
            } else {
                area.unmap(&mut self.page_table);
            }
            self.areas.remove(idx);
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
        }
        self.areas.push(map_area);
        true
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
            flushes.extend(area.collect_mmap_flushes(&self.page_table));
            area.release_mmap_refs();
            if area.is_mmap() || area.is_shm() {
                area.unmap_resident(&mut self.page_table);
            } else {
                area.unmap(&mut self.page_table);
            }
        }
        self.areas.clear();
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
                }
            })
            .collect();
        entries.sort_by_key(|entry| entry.start);
        entries
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
