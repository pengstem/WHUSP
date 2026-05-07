use super::address::page_align_up;
use super::area::{MmapInfo, ShmAreaInfo};
use super::{frame_alloc, VirtPageNum};
use super::{
    FrameTracker, MapArea, MapPermission, MapType, MemorySet, MmapFlush, PhysPageNum, VPNRange,
    VirtAddr,
};
use crate::arch::mm as arch_mm;
use crate::config::{PAGE_SIZE, USER_MMAP_BASE, USER_MMAP_LIMIT};
use crate::fs::File;
use crate::mm::page_cache::{PageCacheId, PageCacheKey, PAGE_CACHE};
use alloc::collections::BTreeMap;
use alloc::sync::Arc;
use alloc::vec::Vec;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MemoryProtectError {
    Unmapped,
    AccessDenied,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MmapFaultAccess {
    Read,
    Write,
    Execute,
}

impl MmapFaultAccess {
    fn is_allowed_by(self, permission: MapPermission) -> bool {
        match self {
            Self::Read => permission.contains(MapPermission::R),
            Self::Write => permission.contains(MapPermission::W),
            Self::Execute => permission.contains(MapPermission::X),
        }
    }
}

pub enum MmapFaultResult {
    Handled,
    Page(MmapFaultPage),
    PageCache(MmapPageCacheFault),
}

pub struct MmapFaultPage {
    vpn: VirtPageNum,
    file_offset: usize,
    read_len: usize,
    backing_file: Option<Arc<dyn File + Send + Sync>>,
}

impl MmapFaultPage {
    pub fn build_frame(&self) -> Option<FrameTracker> {
        let frame = frame_alloc()?;
        if let Some(file) = &self.backing_file {
            if self.read_len > 0 {
                let dst = &mut frame.ppn.get_bytes_array()[..self.read_len];
                file.read_at(self.file_offset, dst);
            }
        }
        Some(frame)
    }
}

pub struct MmapPageCacheFault {
    vpn: VirtPageNum,
    key: PageCacheKey,
    file_offset: usize,
    read_len: usize,
    file_size_at_load: usize,
    backing_file: Arc<dyn File + Send + Sync>,
}

impl MmapPageCacheFault {
    pub fn key(&self) -> PageCacheKey {
        self.key
    }

    pub fn resolve_ppn(&self) -> Option<PhysPageNum> {
        if let Some(ppn) = {
            let mut cache = PAGE_CACHE.exclusive_access();
            cache.get_and_inc_ref(self.key)
        } {
            return Some(ppn);
        }

        let frame = frame_alloc()?;
        if self.read_len > 0 {
            let dst = &mut frame.ppn.get_bytes_array()[..self.read_len];
            self.backing_file.read_at(self.file_offset, dst);
        }

        let mut cache = PAGE_CACHE.exclusive_access();
        Some(cache.insert_loaded_page_and_inc_ref(self.key, frame, self.file_size_at_load))
    }
}

impl MemorySet {
    pub fn from_existed_user(user_space: &MemorySet) -> MemorySet {
        let mut memory_set = Self::new_bare();
        memory_set.brk_base = user_space.brk_base;
        memory_set.brk = user_space.brk;
        memory_set.brk_limit = user_space.brk_limit;
        memory_set.brk_mapped_end = user_space.brk_mapped_end;
        memory_set.mmap_next = user_space.mmap_next;
        memory_set.map_trampoline();
        for area in &user_space.areas {
            let new_area = MapArea::from_another(area);
            if area.is_shm() {
                let Some(shmid) = area.shm_segment_id() else {
                    continue;
                };
                if !crate::mm::shm::retain_attached_segment(shmid, 0) {
                    continue;
                }
                memory_set.areas.push(new_area);
                let area_idx = memory_set.areas.len() - 1;
                let shm_pages = crate::mm::shm::attached_segment_pages(shmid).unwrap_or_default();
                for (vpn, page_index) in area.shm_page_mappings() {
                    let Some(mapping) = shm_pages
                        .iter()
                        .find(|mapping| mapping.page_index == page_index)
                    else {
                        continue;
                    };
                    let page_table = &mut memory_set.page_table;
                    let dst_area = &mut memory_set.areas[area_idx];
                    dst_area.map_shm_frame(page_table, vpn, mapping.ppn, page_index);
                }
            } else if area.is_mmap() {
                // UNFINISHED: MAP_SHARED mappings that cannot enter PAGE_CACHE
                // still copy resident frames on fork; only page-cache-backed
                // mappings share PPNs with refcounting.
                memory_set.areas.push(new_area);
                let area_idx = memory_set.areas.len() - 1;
                let resident_vpns: Vec<_> = area.data_frames.keys().copied().collect();
                for vpn in resident_vpns {
                    let src_ppn = user_space.translate(vpn).unwrap().ppn();
                    let frame = frame_alloc().unwrap();
                    frame
                        .ppn
                        .get_bytes_array()
                        .copy_from_slice(src_ppn.get_bytes_array());
                    let page_table = &mut memory_set.page_table;
                    let dst_area = &mut memory_set.areas[area_idx];
                    dst_area.map_existing_frame(page_table, vpn, frame);
                }
                for (vpn, key) in area.page_cache_mappings() {
                    let Some(ppn) = ({
                        let mut cache = PAGE_CACHE.exclusive_access();
                        cache.get_and_inc_ref(key)
                    }) else {
                        continue;
                    };
                    let page_table = &mut memory_set.page_table;
                    let dst_area = &mut memory_set.areas[area_idx];
                    if !dst_area.map_page_cache_frame(page_table, vpn, ppn, key) {
                        PAGE_CACHE.exclusive_access().dec_ref(key);
                    }
                }
            } else {
                memory_set.push(new_area, None);
                for vpn in area.vpn_range {
                    let src_ppn = user_space.translate(vpn).unwrap().ppn();
                    let dst_ppn = memory_set.translate(vpn).unwrap().ppn();
                    dst_ppn
                        .get_bytes_array()
                        .copy_from_slice(src_ppn.get_bytes_array());
                }
            }
        }
        memory_set
    }

    pub fn set_program_break(&mut self, addr: usize) -> usize {
        if addr == 0 {
            return self.brk;
        }
        if addr < self.brk_base || addr > self.brk_limit {
            return self.brk;
        }

        let old_mapped_end = self.brk_mapped_end;
        let new_mapped_end = page_align_up(addr);
        let heap_start_vpn = VirtAddr::from(self.brk_base).floor();
        let old_end_vpn = VirtAddr::from(old_mapped_end).floor();
        let new_end_vpn = VirtAddr::from(new_mapped_end).floor();

        if new_mapped_end > old_mapped_end {
            if self.range_overlaps(old_mapped_end, new_mapped_end) {
                return self.brk;
            }
            let Some(area_idx) = self.find_brk_extension_area(heap_start_vpn, old_end_vpn) else {
                let mut heap_area = MapArea::new(
                    old_mapped_end.into(),
                    new_mapped_end.into(),
                    MapType::Framed,
                    MapPermission::R | MapPermission::W | MapPermission::U,
                );
                heap_area.map(&mut self.page_table);
                self.areas.push(heap_area);
                self.brk = addr;
                self.brk_mapped_end = new_mapped_end;
                return self.brk;
            };
            let heap_area = &mut self.areas[area_idx];
            for vpn in VPNRange::new(old_end_vpn, new_end_vpn) {
                heap_area.map_one(&mut self.page_table, vpn);
            }
            let area_start = heap_area.vpn_range.get_start();
            heap_area.vpn_range = VPNRange::new(area_start, new_end_vpn);
        } else if new_mapped_end < old_mapped_end {
            self.shrink_brk_areas(heap_start_vpn, new_end_vpn, old_end_vpn);
        }

        self.brk = addr;
        self.brk_mapped_end = new_mapped_end;
        self.brk
    }

    fn find_brk_extension_area(
        &self,
        heap_start_vpn: super::VirtPageNum,
        old_end_vpn: super::VirtPageNum,
    ) -> Option<usize> {
        self.areas.iter().position(|area| {
            !area.is_mmap()
                && !area.is_shm()
                && area.vpn_range.get_start() >= heap_start_vpn
                && area.vpn_range.get_end() == old_end_vpn
        })
    }

    fn shrink_brk_areas(
        &mut self,
        heap_start_vpn: super::VirtPageNum,
        new_end_vpn: super::VirtPageNum,
        old_end_vpn: super::VirtPageNum,
    ) {
        self.split_area_at(new_end_vpn);
        self.split_area_at(old_end_vpn);

        let mut idx = 0;
        while idx < self.areas.len() {
            let area_start = self.areas[idx].vpn_range.get_start();
            let area_end = self.areas[idx].vpn_range.get_end();
            if !self.areas[idx].is_mmap()
                && !self.areas[idx].is_shm()
                && area_start >= heap_start_vpn
                && area_start >= new_end_vpn
                && area_end <= old_end_vpn
            {
                let mut area = self.areas.remove(idx);
                area.unmap(&mut self.page_table);
            } else {
                idx += 1;
            }
        }
    }

    pub fn mmap_area(
        &mut self,
        len: usize,
        permission: MapPermission,
        backing_file: Option<Arc<dyn File + Send + Sync>>,
        file_size: usize,
        file_offset: usize,
        shared: bool,
        writable: bool,
        page_cache_id: Option<PageCacheId>,
    ) -> Option<usize> {
        let map_len = checked_page_align_up(len)?;
        let start = self.alloc_mmap_range(map_len)?;
        let end = start.checked_add(map_len)?;
        let mut area = MapArea::new(start.into(), end.into(), MapType::Framed, permission);
        area.mmap_info = Some(MmapInfo {
            shared,
            writable,
            len,
            file_offset,
            file_size,
            backing_file,
            page_cache_id,
            page_cache_pages: BTreeMap::new(),
        });
        self.areas.push(area);
        self.mmap_next = next_mmap_hint(end);
        Some(start)
    }

    pub fn mmap_fixed_area(
        &mut self,
        start: usize,
        len: usize,
        permission: MapPermission,
        backing_file: Option<Arc<dyn File + Send + Sync>>,
        file_size: usize,
        file_offset: usize,
        shared: bool,
        writable: bool,
        page_cache_id: Option<PageCacheId>,
    ) -> Option<(usize, Vec<MmapFlush>)> {
        if start % PAGE_SIZE != 0 {
            return None;
        }
        let map_len = checked_page_align_up(len)?;
        let end = start.checked_add(map_len)?;
        let start_vpn = VirtAddr::from(start).floor();
        let end_vpn = VirtAddr::from(end).floor();

        self.split_area_at(start_vpn);
        self.split_area_at(end_vpn);

        let mut flushes = Vec::new();
        let mut idx = 0;
        while idx < self.areas.len() {
            let area_start = self.areas[idx].vpn_range.get_start();
            let area_end = self.areas[idx].vpn_range.get_end();
            if area_start < end_vpn && area_end > start_vpn {
                let mut area = self.areas.remove(idx);
                flushes.extend(area.collect_mmap_flushes(&self.page_table));
                area.unmap_resident(&mut self.page_table);
            } else {
                idx += 1;
            }
        }

        let mut area = MapArea::new(start.into(), end.into(), MapType::Framed, permission);
        area.mmap_info = Some(MmapInfo {
            shared,
            writable,
            len,
            file_offset,
            file_size,
            backing_file,
            page_cache_id,
            page_cache_pages: BTreeMap::new(),
        });
        self.areas.push(area);
        Some((start, flushes))
    }

    pub fn attach_shm_area(
        &mut self,
        requested_addr: usize,
        len: usize,
        permission: MapPermission,
        shmid: usize,
        pages: &[crate::mm::shm::ShmPageMapping],
    ) -> Option<usize> {
        let map_len = checked_page_align_up(len)?;
        let start = if requested_addr == 0 {
            self.alloc_mmap_range(map_len)?
        } else {
            if requested_addr % PAGE_SIZE != 0 {
                return None;
            }
            let end = requested_addr.checked_add(map_len)?;
            if end > USER_MMAP_LIMIT || self.range_overlaps(requested_addr, end) {
                return None;
            }
            requested_addr
        };
        let end = start.checked_add(map_len)?;
        let start_vpn = VirtAddr::from(start).floor();
        let mut area = MapArea::new(start.into(), end.into(), MapType::Framed, permission);
        area.shm_info = Some(ShmAreaInfo::new(shmid, len));
        for mapping in pages {
            if mapping.page_index >= map_len / PAGE_SIZE {
                continue;
            }
            let vpn = VirtPageNum(start_vpn.0 + mapping.page_index);
            if !area.map_shm_frame(&mut self.page_table, vpn, mapping.ppn, mapping.page_index) {
                area.unmap_resident(&mut self.page_table);
                return None;
            }
        }
        self.areas.push(area);
        self.mmap_next = next_mmap_hint(end);
        Some(start)
    }

    pub fn detach_shm_area(&mut self, start: usize) -> Option<()> {
        if start % PAGE_SIZE != 0 {
            return None;
        }
        let start_vpn = VirtAddr::from(start).floor();
        let idx = self
            .areas
            .iter()
            .position(|area| area.is_shm() && area.vpn_range.get_start() == start_vpn)?;
        let mut area = self.areas.remove(idx);
        area.unmap_resident(&mut self.page_table);
        Some(())
    }

    pub fn prepare_mmap_page_fault(
        &mut self,
        addr: usize,
        access: MmapFaultAccess,
    ) -> Option<MmapFaultResult> {
        let vpn = VirtAddr::from(addr).floor();
        let area_idx = self.areas.iter().position(|area| {
            area.is_mmap() && area.vpn_range.get_start() <= vpn && vpn < area.vpn_range.get_end()
        })?;
        let area = &self.areas[area_idx];
        if !access.is_allowed_by(area.map_perm) {
            return None;
        }
        if let Some(pte) = self.translate(vpn).filter(|pte| pte.bits != 0) {
            if access == MmapFaultAccess::Write && !pte.writable() {
                let key = area.mmap_info.as_ref().and_then(|info| {
                    if info.shared && info.writable {
                        info.page_cache_pages.get(&vpn).copied()
                    } else {
                        None
                    }
                })?;
                if !PAGE_CACHE.exclusive_access().mark_dirty(key) {
                    return None;
                }
                let pte_flags = crate::mm::page_table::PTEFlags::from_bits_truncate(
                    self.areas[area_idx].map_perm.bits(),
                );
                if !self.page_table.remap_flags(vpn, pte_flags) {
                    return None;
                }
                arch_mm::flush_tlb_all();
            }
            return Some(MmapFaultResult::Handled);
        }
        let area = &self.areas[area_idx];
        if area.data_frames.contains_key(&vpn) {
            return Some(MmapFaultResult::Handled);
        }

        let info = area.mmap_info.as_ref().unwrap();
        let area_offset = (vpn.0 - area.vpn_range.get_start().0) * PAGE_SIZE;
        let file_offset = info.file_offset.checked_add(area_offset)?;
        // UNFINISHED: Linux raises SIGBUS for accesses to file-backed mmap
        // pages wholly beyond the backing object's end. The current contest
        // path zero-fills those bytes, but it must at least avoid asking EXT4
        // to read past EOF for the partial tail page used by dynamic DSOs.
        let map_read_len = info.len.saturating_sub(area_offset).min(PAGE_SIZE);
        let file_read_len = info.file_size.saturating_sub(file_offset).min(PAGE_SIZE);
        let read_len = if info.backing_file.is_some() {
            map_read_len.min(file_read_len)
        } else {
            0
        };
        if let (Some(page_cache_id), Some(backing_file)) = (info.page_cache_id, &info.backing_file)
        {
            if let Some(key) = PageCacheKey::from_file_offset(page_cache_id, file_offset) {
                return Some(MmapFaultResult::PageCache(MmapPageCacheFault {
                    vpn,
                    key,
                    file_offset,
                    read_len,
                    file_size_at_load: info.file_size,
                    backing_file: backing_file.clone(),
                }));
            }
        }
        Some(MmapFaultResult::Page(MmapFaultPage {
            vpn,
            file_offset,
            read_len,
            backing_file: info.backing_file.clone(),
        }))
    }

    pub fn install_mmap_fault_page(&mut self, page: MmapFaultPage, frame: FrameTracker) -> bool {
        let Some(idx) = self.areas.iter().position(|area| {
            area.is_mmap()
                && area.vpn_range.get_start() <= page.vpn
                && page.vpn < area.vpn_range.get_end()
        }) else {
            return false;
        };
        let page_table = &mut self.page_table;
        let area = &mut self.areas[idx];
        area.map_existing_frame(page_table, page.vpn, frame)
    }

    pub fn install_mmap_page_cache_fault_page(
        &mut self,
        page: MmapPageCacheFault,
        ppn: PhysPageNum,
    ) -> bool {
        let Some(idx) = self.areas.iter().position(|area| {
            area.is_mmap()
                && area.vpn_range.get_start() <= page.vpn
                && page.vpn < area.vpn_range.get_end()
        }) else {
            return false;
        };
        let page_table = &mut self.page_table;
        let area = &mut self.areas[idx];
        area.map_page_cache_frame(page_table, page.vpn, ppn, page.key)
    }

    pub fn munmap_area(&mut self, start: usize, len: usize) -> Option<Vec<MmapFlush>> {
        if len == 0 || start % PAGE_SIZE != 0 {
            return None;
        }
        let Some(map_len) = checked_page_align_up(len) else {
            return None;
        };
        let Some(end) = start.checked_add(map_len) else {
            return None;
        };
        let start_vpn = VirtAddr::from(start).floor();
        let end_vpn = VirtAddr::from(end).floor();

        self.split_area_at(start_vpn);
        self.split_area_at(end_vpn);

        let mut flushes = Vec::new();
        let mut idx = 0;
        while idx < self.areas.len() {
            let area_start = self.areas[idx].vpn_range.get_start();
            let area_end = self.areas[idx].vpn_range.get_end();
            if self.areas[idx].is_mmap() && area_start >= start_vpn && area_end <= end_vpn {
                let mut area = self.areas.remove(idx);
                flushes.extend(area.collect_mmap_flushes(&self.page_table));
                area.unmap_resident(&mut self.page_table);
            } else {
                idx += 1;
            }
        }
        Some(flushes)
    }

    pub fn msync_area(&self, start: usize, len: usize) -> Option<Vec<MmapFlush>> {
        if len == 0 {
            return Some(Vec::new());
        }
        let map_len = checked_page_align_up(len)?;
        let end = start.checked_add(map_len)?;
        let start_vpn = VirtAddr::from(start).floor();
        let end_vpn = VirtAddr::from(end).floor();
        if !self.range_is_mapped_vpn(start_vpn, end_vpn) {
            return None;
        }

        let mut flushes = Vec::new();
        for area in &self.areas {
            let area_start = area.vpn_range.get_start();
            let area_end = area.vpn_range.get_end();
            if area.is_mmap() && area_start < end_vpn && area_end > start_vpn {
                flushes.extend(area.collect_mmap_flushes(&self.page_table));
            }
        }
        Some(flushes)
    }

    pub fn mprotect_area(
        &mut self,
        start: usize,
        len: usize,
        permission: MapPermission,
    ) -> Result<(), MemoryProtectError> {
        if len == 0 {
            return Ok(());
        }
        if start % PAGE_SIZE != 0 {
            return Err(MemoryProtectError::Unmapped);
        }
        let Some(end) = start.checked_add(len) else {
            return Err(MemoryProtectError::Unmapped);
        };
        let start_vpn = VirtAddr::from(start).floor();
        let end_vpn = VirtAddr::from(end).floor();
        if !self.range_is_mapped_vpn(start_vpn, end_vpn) {
            return Err(MemoryProtectError::Unmapped);
        }

        if permission.contains(MapPermission::W) && !self.can_mprotect_write(start_vpn, end_vpn) {
            return Err(MemoryProtectError::AccessDenied);
        }

        self.split_area_at(start_vpn);
        self.split_area_at(end_vpn);

        let mut touched = false;
        for area in &mut self.areas {
            let area_start = area.vpn_range.get_start();
            let area_end = area.vpn_range.get_end();
            if area_start >= start_vpn && area_end <= end_vpn {
                if !area.remap_permission(&mut self.page_table, permission) {
                    return Err(MemoryProtectError::Unmapped);
                }
                touched = true;
            }
        }
        if !touched {
            return Err(MemoryProtectError::Unmapped);
        }
        arch_mm::flush_tlb_all();
        Ok(())
    }

    fn alloc_mmap_range(&self, len: usize) -> Option<usize> {
        if len == 0 || len > USER_MMAP_LIMIT - USER_MMAP_BASE {
            return None;
        }
        let hint = normalized_mmap_hint(self.mmap_next);
        self.find_mmap_hole(hint, USER_MMAP_LIMIT, len)
            .or_else(|| self.find_mmap_hole(USER_MMAP_BASE, hint, len))
    }

    fn find_mmap_hole(&self, start: usize, limit: usize, len: usize) -> Option<usize> {
        if start >= limit {
            return None;
        }
        let mut cursor = page_align_up(start);
        while let Some(end) = cursor.checked_add(len) {
            if end > limit {
                break;
            }
            if !self.range_overlaps(cursor, end) {
                return Some(cursor);
            }
            cursor = cursor.checked_add(PAGE_SIZE)?;
        }
        None
    }

    fn range_overlaps(&self, start: usize, end: usize) -> bool {
        let start_vpn = VirtAddr::from(start).floor();
        let end_vpn = VirtAddr::from(end).floor();
        self.areas.iter().any(|area| {
            let area_start = area.vpn_range.get_start();
            let area_end = area.vpn_range.get_end();
            start_vpn < area_end && end_vpn > area_start
        })
    }

    fn range_is_mapped_vpn(&self, start: super::VirtPageNum, end: super::VirtPageNum) -> bool {
        let mut cursor = start;
        while cursor < end {
            let Some(area_end) = self
                .areas
                .iter()
                .filter_map(|area| {
                    let area_start = area.vpn_range.get_start();
                    let area_end = area.vpn_range.get_end();
                    if area_start <= cursor && cursor < area_end {
                        Some(area_end)
                    } else {
                        None
                    }
                })
                .max()
            else {
                return false;
            };
            if area_end <= cursor {
                return false;
            }
            cursor = area_end.min(end);
        }
        true
    }

    fn split_area_at(&mut self, at: super::VirtPageNum) {
        let Some(idx) = self.areas.iter().position(|area| {
            let area_start = area.vpn_range.get_start();
            let area_end = area.vpn_range.get_end();
            area_start < at && at < area_end
        }) else {
            return;
        };
        if let Some(right) = self.areas[idx].split_off(at) {
            self.areas.insert(idx + 1, right);
        }
    }

    fn can_mprotect_write(&self, start: super::VirtPageNum, end: super::VirtPageNum) -> bool {
        self.areas
            .iter()
            .filter(|area| {
                let area_start = area.vpn_range.get_start();
                let area_end = area.vpn_range.get_end();
                area_start < end && area_end > start
            })
            .all(|area| {
                let Some(info) = &area.mmap_info else {
                    return true;
                };
                if !info.shared {
                    return true;
                }
                info.backing_file
                    .as_ref()
                    .is_none_or(|file| file.writable())
            })
    }
}

fn checked_page_align_up(addr: usize) -> Option<usize> {
    addr.checked_add(PAGE_SIZE - 1)
        .map(|addr| addr & !(PAGE_SIZE - 1))
}

fn normalized_mmap_hint(hint: usize) -> usize {
    if hint < USER_MMAP_BASE || hint >= USER_MMAP_LIMIT {
        USER_MMAP_BASE
    } else {
        page_align_up(hint)
    }
}

fn next_mmap_hint(end: usize) -> usize {
    if end >= USER_MMAP_LIMIT {
        USER_MMAP_BASE
    } else {
        end
    }
}
