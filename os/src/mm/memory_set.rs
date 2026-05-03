use super::{
    MapArea, MapPermission, MapType, PageTable, PageTableEntry, VirtAddr, VirtPageNum,
    page_table::PTEFlags,
};
use crate::arch::mm as arch_mm;
use alloc::vec::Vec;

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
        self.push(
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
            area.unmap(&mut self.page_table);
            self.areas.remove(idx);
        }
    }
    /// Add a new MapArea into this MemorySet.
    /// Assuming that there are no conflicts in the virtual address
    /// space.
    pub fn push(&mut self, map_area: MapArea, data: Option<&[u8]>) {
        self.push_with_offset(map_area, data, 0);
    }

    pub(super) fn push_with_offset(
        &mut self,
        mut map_area: MapArea,
        data: Option<&[u8]>,
        data_offset: usize,
    ) {
        map_area.map(&mut self.page_table);
        if let Some(data) = data {
            map_area.copy_data(&self.page_table, data, data_offset);
        }
        self.areas.push(map_area);
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
    pub fn recycle_data_pages(&mut self) {
        //*self = Self::new_bare();
        self.areas.clear();
    }
}
