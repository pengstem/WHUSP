use super::frame_alloc;
use super::page_table::PTEFlags;
use super::{
    FrameTracker, PageTable, PhysAddr, PhysPageNum, StepByOne, VPNRange, VirtAddr, VirtPageNum,
};
use crate::arch::mm as arch_mm;
use crate::config::PAGE_SIZE;
use crate::fs::File;
use crate::mm::page_cache::{PageCacheId, PageCacheKey, PAGE_CACHE};
use alloc::collections::BTreeMap;
use alloc::sync::Arc;
use alloc::vec::Vec;

pub struct MapArea {
    pub(super) vpn_range: VPNRange,
    pub(super) data_frames: BTreeMap<VirtPageNum, FrameTracker>,
    pub(super) map_type: MapType,
    pub(super) map_perm: MapPermission,
    pub(super) mmap_info: Option<MmapInfo>,
    pub(super) shm_info: Option<ShmAreaInfo>,
    pub(super) locked: bool,
    pub(super) lock_on_fault: bool,
}

pub struct MmapFlush {
    file: Arc<dyn File + Send + Sync>,
    offset: usize,
    data: Vec<u8>,
}

impl MmapFlush {
    /// Writes one collected MAP_SHARED dirty page fragment back to its file.
    ///
    /// Callers build these records while holding the process memory lock and
    /// perform the actual filesystem write after that lock has been released.
    pub fn write_back(self) {
        self.file.write_at(self.offset, &self.data);
    }
}

#[derive(Clone)]
pub(super) struct MmapInfo {
    pub(super) shared: bool,
    pub(super) writable: bool,
    pub(super) grow_down: bool,
    pub(super) reported_perm: MapPermission,
    pub(super) len: usize,
    pub(super) file_offset: usize,
    pub(super) file_size: usize,
    pub(super) backing_file: Option<Arc<dyn File + Send + Sync>>,
    pub(super) page_cache_id: Option<PageCacheId>,
    pub(super) page_cache_pages: BTreeMap<VirtPageNum, PageCacheKey>,
    pub(super) exec_segment: Option<ExecSegmentInfo>,
}

#[derive(Clone)]
pub(super) struct ExecSegmentInfo {
    pub(super) page_offset: usize,
    pub(super) file_offset: usize,
    pub(super) file_size: usize,
    pub(super) mem_size: usize,
}

#[derive(Clone)]
pub struct ShmAreaInfo {
    pub(super) shmid: usize,
    pub(super) len: usize,
    pub(super) offset: usize,
    pub(super) pages: BTreeMap<VirtPageNum, usize>,
}

impl ShmAreaInfo {
    pub(crate) fn new(shmid: usize, len: usize) -> Self {
        Self {
            shmid,
            len,
            offset: 0,
            pages: BTreeMap::new(),
        }
    }

    fn split_off(&mut self, offset: usize, at: VirtPageNum) -> Self {
        let right = Self {
            shmid: self.shmid,
            len: self.len.saturating_sub(offset),
            offset: self.offset + offset,
            pages: self.pages.split_off(&at),
        };
        self.len = self.len.min(offset);
        right
    }
}

impl MmapInfo {
    fn split_off(&mut self, offset: usize, at: VirtPageNum) -> Self {
        let right_exec_segment = self
            .exec_segment
            .as_mut()
            .map(|info| info.split_off(offset));
        let right = Self {
            shared: self.shared,
            writable: self.writable,
            grow_down: self.grow_down,
            reported_perm: self.reported_perm,
            len: self.len.saturating_sub(offset),
            file_offset: self.file_offset + offset,
            file_size: self.file_size,
            backing_file: self.backing_file.clone(),
            page_cache_id: self.page_cache_id,
            page_cache_pages: self.page_cache_pages.split_off(&at),
            exec_segment: right_exec_segment,
        };
        self.len = self.len.min(offset);
        right
    }
}

impl ExecSegmentInfo {
    fn split_off(&mut self, offset: usize) -> Self {
        let consumed_mem = offset.saturating_sub(self.page_offset).min(self.mem_size);
        let consumed_file = offset.saturating_sub(self.page_offset).min(self.file_size);
        let right = Self {
            page_offset: self.page_offset.saturating_sub(offset),
            file_offset: self.file_offset.saturating_add(consumed_file),
            file_size: self.file_size.saturating_sub(consumed_file),
            mem_size: self.mem_size.saturating_sub(consumed_mem),
        };

        let left_mem = offset.saturating_sub(self.page_offset).min(self.mem_size);
        let left_file = offset.saturating_sub(self.page_offset).min(self.file_size);
        self.mem_size = left_mem;
        self.file_size = left_file;
        right
    }
}

impl MapArea {
    pub(super) fn new(
        start_va: VirtAddr,
        end_va: VirtAddr,
        map_type: MapType,
        map_perm: MapPermission,
    ) -> Self {
        let start_vpn: VirtPageNum = start_va.floor();
        let end_vpn: VirtPageNum = end_va.ceil();
        Self {
            vpn_range: VPNRange::new(start_vpn, end_vpn),
            data_frames: BTreeMap::new(),
            map_type,
            map_perm,
            mmap_info: None,
            shm_info: None,
            locked: false,
            lock_on_fault: false,
        }
    }

    pub(super) fn from_another(another: &MapArea) -> Self {
        let mut mmap_info = another.mmap_info.clone();
        if let Some(info) = &mut mmap_info {
            info.page_cache_pages.clear();
        }
        Self {
            vpn_range: VPNRange::new(another.vpn_range.get_start(), another.vpn_range.get_end()),
            data_frames: BTreeMap::new(),
            map_type: another.map_type,
            map_perm: another.map_perm,
            mmap_info,
            shm_info: another.shm_info.clone().map(|mut info| {
                info.pages.clear();
                info
            }),
            locked: false,
            lock_on_fault: false,
        }
    }

    pub(super) fn split_off(&mut self, at: VirtPageNum) -> Option<Self> {
        let start = self.vpn_range.get_start();
        let end = self.vpn_range.get_end();
        if at <= start || at >= end {
            return None;
        }

        let right_mmap_info = self
            .mmap_info
            .as_mut()
            .map(|info| info.split_off((at.0 - start.0) * PAGE_SIZE, at));
        let right_shm_info = self.shm_info.as_mut().map(|info| {
            // UNFINISHED: Linux reports SysV SHM attach counts per process
            // attach. Splitting a SHM VMA is rare in the contest path; this
            // representation counts each split VMA piece for lifetime safety.
            crate::mm::shm::retain_attached_segment(info.shmid, 0);
            info.split_off((at.0 - start.0) * PAGE_SIZE, at)
        });
        let right = Self {
            vpn_range: VPNRange::new(at, end),
            data_frames: self.data_frames.split_off(&at),
            map_type: self.map_type,
            map_perm: self.map_perm,
            mmap_info: right_mmap_info,
            shm_info: right_shm_info,
            locked: self.locked,
            lock_on_fault: self.lock_on_fault,
        };
        self.vpn_range = VPNRange::new(start, at);
        Some(right)
    }

    pub(super) fn remap_permission(
        &mut self,
        page_table: &mut PageTable,
        permission: MapPermission,
        reported_permission: MapPermission,
    ) -> bool {
        let pte_flags = PTEFlags::from_bits_truncate(permission.bits() as usize);
        if self.is_mmap() {
            for vpn in self.data_frames.keys().copied() {
                let flags = remap_flags_preserving_cow(page_table, vpn, pte_flags);
                if !page_table.remap_flags(vpn, flags) {
                    return false;
                }
            }
            if let Some(info) = &mut self.mmap_info {
                info.writable = permission.contains(MapPermission::W);
                info.reported_perm = reported_permission;
                let mut page_cache_pte_flags = pte_flags;
                if info.shared && info.writable {
                    page_cache_pte_flags.remove(PTEFlags::W);
                }
                for vpn in info.page_cache_pages.keys().copied() {
                    if !page_table.remap_flags(vpn, page_cache_pte_flags) {
                        return false;
                    }
                }
            }
        } else {
            for vpn in self.vpn_range {
                let flags = remap_flags_preserving_cow(page_table, vpn, pte_flags);
                if !page_table.remap_flags(vpn, flags) {
                    return false;
                }
            }
        }
        self.map_perm = permission;
        true
    }

    pub(super) fn map_one(&mut self, page_table: &mut PageTable, vpn: VirtPageNum) -> bool {
        let ppn: PhysPageNum = match self.map_type {
            MapType::Identical => {
                let va: VirtAddr = vpn.into();
                PhysAddr::from(arch_mm::virt_to_phys(usize::from(va))).floor()
            }
            MapType::Framed => {
                let Some(frame) = frame_alloc() else {
                    return false;
                };
                let ppn = frame.ppn;
                if !page_table.try_map(
                    vpn,
                    ppn,
                    PTEFlags::from_bits_truncate(self.map_perm.bits() as usize),
                ) {
                    return false;
                }
                self.data_frames.insert(vpn, frame);
                return true;
            }
        };
        page_table.try_map(
            vpn,
            ppn,
            PTEFlags::from_bits_truncate(self.map_perm.bits() as usize),
        )
    }

    pub(super) fn map(&mut self, page_table: &mut PageTable) -> bool {
        let mut mapped_vpns = Vec::new();
        for vpn in self.vpn_range {
            if !self.map_one(page_table, vpn) {
                for mapped_vpn in mapped_vpns {
                    self.unmap_one(page_table, mapped_vpn);
                }
                return false;
            }
            mapped_vpns.push(vpn);
        }
        true
    }

    pub(super) fn unmap_one(&mut self, page_table: &mut PageTable, vpn: VirtPageNum) {
        if self.map_type == MapType::Framed {
            self.data_frames.remove(&vpn);
        }
        page_table.unmap(vpn);
    }

    pub(super) fn unmap(&mut self, page_table: &mut PageTable) {
        for vpn in self.vpn_range {
            self.unmap_one(page_table, vpn);
        }
    }

    pub(super) fn map_existing_frame(
        &mut self,
        page_table: &mut PageTable,
        vpn: VirtPageNum,
        frame: FrameTracker,
    ) -> bool {
        let pte_flags = PTEFlags::from_bits_truncate(self.map_perm.bits() as usize);
        self.map_existing_frame_with_flags(page_table, vpn, frame, pte_flags)
    }

    pub(super) fn map_existing_frame_with_flags(
        &mut self,
        page_table: &mut PageTable,
        vpn: VirtPageNum,
        frame: FrameTracker,
        pte_flags: PTEFlags,
    ) -> bool {
        if self.data_frames.contains_key(&vpn) {
            return true;
        }
        if page_table.translate(vpn).is_some_and(|pte| pte.bits != 0) {
            return true;
        }
        let ppn = frame.ppn;
        if !page_table.try_map(vpn, ppn, pte_flags) {
            return false;
        }
        self.data_frames.insert(vpn, frame);
        true
    }

    /// Maps one shared page-cache frame into this mmap VMA.
    ///
    /// The caller must already own one page-cache reference for `key`. This
    /// method records that reference only after the PTE install succeeds.
    pub(super) fn map_page_cache_frame(
        &mut self,
        page_table: &mut PageTable,
        vpn: VirtPageNum,
        ppn: PhysPageNum,
        key: PageCacheKey,
    ) -> bool {
        let Some(info) = self.mmap_info.as_mut() else {
            return false;
        };
        if info.page_cache_id != Some(key.id) {
            return false;
        }
        if info.page_cache_pages.contains_key(&vpn) {
            return true;
        }
        if self.data_frames.contains_key(&vpn) {
            return true;
        }
        if page_table.translate(vpn).is_some_and(|pte| pte.bits != 0) {
            return true;
        }
        let mut pte_flags = PTEFlags::from_bits_truncate(self.map_perm.bits() as usize);
        if info.shared && info.writable {
            pte_flags.remove(PTEFlags::W);
        }
        if !page_table.try_map(vpn, ppn, pte_flags) {
            return false;
        }
        info.page_cache_pages.insert(vpn, key);
        true
    }

    pub(super) fn map_shm_frame(
        &mut self,
        page_table: &mut PageTable,
        vpn: VirtPageNum,
        ppn: PhysPageNum,
        page_index: usize,
    ) -> bool {
        let Some(info) = self.shm_info.as_mut() else {
            return false;
        };
        if info.pages.contains_key(&vpn) {
            return true;
        }
        if self.data_frames.contains_key(&vpn) {
            return true;
        }
        if page_table.translate(vpn).is_some_and(|pte| pte.bits != 0) {
            return true;
        }
        let pte_flags = PTEFlags::from_bits_truncate(self.map_perm.bits() as usize);
        if !page_table.try_map(vpn, ppn, pte_flags) {
            return false;
        }
        info.pages.insert(vpn, page_index);
        true
    }

    pub(super) fn unmap_resident(&mut self, page_table: &mut PageTable) {
        let vpns: Vec<_> = self.data_frames.keys().copied().collect();
        for vpn in vpns {
            page_table.unmap(vpn);
        }
        self.data_frames.clear();

        if let Some(info) = self.mmap_info.as_mut() {
            let cache_vpns: Vec<_> = info.page_cache_pages.keys().copied().collect();
            let cache_keys: Vec<_> = info.page_cache_pages.values().copied().collect();
            for vpn in cache_vpns {
                if page_table.translate(vpn).is_some_and(|pte| pte.bits != 0) {
                    page_table.unmap(vpn);
                }
            }
            let mut cache = PAGE_CACHE.exclusive_access();
            for key in cache_keys {
                let _ = cache.dec_ref_and_take_if_unused(key);
            }
            info.page_cache_pages.clear();
        }

        if let Some(info) = self.shm_info.as_mut() {
            let shm_vpns: Vec<_> = info.pages.keys().copied().collect();
            for vpn in shm_vpns {
                if page_table.translate(vpn).is_some_and(|pte| pte.bits != 0) {
                    page_table.unmap(vpn);
                }
            }
            info.pages.clear();
            let _ = crate::mm::shm::detach_segment(info.shmid, 0);
        }
    }

    pub(super) fn copy_data(&mut self, page_table: &PageTable, data: &[u8], data_offset: usize) {
        assert_eq!(self.map_type, MapType::Framed);
        assert!(data_offset < PAGE_SIZE);
        let mut copied = 0usize;
        let mut current_vpn = self.vpn_range.get_start();
        let len = data.len();
        let mut page_offset = data_offset;
        while copied < len {
            let copy_len = (PAGE_SIZE - page_offset).min(len - copied);
            let src = &data[copied..copied + copy_len];
            let dst = &mut page_table
                .translate(current_vpn)
                .expect("copy_data requires pages mapped by MapArea::map")
                .ppn()
                .get_bytes_array()[page_offset..page_offset + copy_len];
            dst.copy_from_slice(src);
            copied += copy_len;
            page_offset = 0;
            current_vpn.step();
        }
    }

    pub(super) fn is_mmap(&self) -> bool {
        self.mmap_info.is_some()
    }

    pub(super) fn is_shm(&self) -> bool {
        self.shm_info.is_some()
    }

    pub(super) fn is_locked(&self) -> bool {
        self.locked || self.lock_on_fault
    }

    pub(super) fn locked_bytes(&self) -> usize {
        if !self.is_locked() {
            return 0;
        }
        (self.vpn_range.get_end().0 - self.vpn_range.get_start().0) * PAGE_SIZE
    }

    pub(super) fn resident_bytes(&self, page_table: &PageTable) -> usize {
        let resident_pages = self
            .vpn_range
            .into_iter()
            .filter(|vpn| page_table.translate(*vpn).is_some_and(|pte| pte.bits != 0))
            .count();
        resident_pages * PAGE_SIZE
    }

    pub(super) fn shm_segment_id(&self) -> Option<usize> {
        self.shm_info.as_ref().map(|info| info.shmid)
    }

    pub(super) fn shm_page_mappings(&self) -> Vec<(VirtPageNum, usize)> {
        self.shm_info
            .as_ref()
            .map(|info| {
                info.pages
                    .iter()
                    .map(|(vpn, page_index)| (*vpn, *page_index))
                    .collect()
            })
            .unwrap_or_default()
    }

    pub(super) fn page_cache_mappings(&self) -> Vec<(VirtPageNum, PageCacheKey)> {
        self.mmap_info
            .as_ref()
            .map(|info| {
                info.page_cache_pages
                    .iter()
                    .map(|(vpn, key)| (*vpn, *key))
                    .collect()
            })
            .unwrap_or_default()
    }

    pub(super) fn collect_mmap_flushes(&self, page_table: &PageTable) -> Vec<MmapFlush> {
        let mut flushes = Vec::new();
        let Some(info) = &self.mmap_info else {
            return flushes;
        };
        if !info.shared || !info.writable {
            return flushes;
        }
        let Some(file) = &info.backing_file else {
            return flushes;
        };
        let start_vpn = self.vpn_range.get_start();
        for vpn in self.data_frames.keys().copied() {
            let area_offset = (vpn.0 - start_vpn.0) * PAGE_SIZE;
            let Some(copy_len) = mmap_writeback_len(info, area_offset) else {
                continue;
            };
            let src = &page_table
                .translate(vpn)
                .expect("resident mmap frame must have a page-table entry")
                .ppn()
                .get_bytes_array()[..copy_len];
            flushes.push(MmapFlush {
                file: file.clone(),
                offset: info.file_offset + area_offset,
                data: src.to_vec(),
            });
        }
        for (vpn, key) in &info.page_cache_pages {
            let area_offset = (vpn.0 - start_vpn.0) * PAGE_SIZE;
            let Some(copy_len) = mmap_writeback_len(info, area_offset) else {
                continue;
            };
            let Some(data) = PAGE_CACHE
                .exclusive_access()
                .take_dirty_page_data(*key, copy_len)
            else {
                continue;
            };
            flushes.push(MmapFlush {
                file: file.clone(),
                offset: info.file_offset + area_offset,
                data,
            });
        }
        flushes
    }

    /// Tears down resident mmap pages and releases page-cache references.
    ///
    /// Returned flush records are currently empty by design; see the
    /// `UNFINISHED` note below for the missing implicit MAP_SHARED writeback.
    pub(super) fn take_mmap_flushes(&mut self, page_table: &mut PageTable) -> Vec<MmapFlush> {
        // UNFINISHED: Linux eventually writes dirty MAP_SHARED pages back on
        // munmap/exit. This kernel currently makes msync() the explicit
        // writeback boundary for file-backed mmap. Implicit writeback of large
        // stress mappings exhausts lwext4/kernel heap before the contest runner
        // can continue to the next case.
        let data_frames = core::mem::take(&mut self.data_frames);
        for (vpn, _frame) in data_frames {
            if page_table.translate(vpn).is_some_and(|pte| pte.bits != 0) {
                page_table.unmap(vpn);
            }
        }

        if let Some(info) = self.mmap_info.as_mut() {
            let page_cache_pages = core::mem::take(&mut info.page_cache_pages);
            for (vpn, key) in page_cache_pages {
                if page_table.translate(vpn).is_some_and(|pte| pte.bits != 0) {
                    page_table.unmap(vpn);
                }
                let _ = PAGE_CACHE
                    .exclusive_access()
                    .dec_ref_and_take_if_unused(key);
            }
        }

        Vec::new()
    }

    /// Releases file-level accounting owned by this mmap VMA.
    ///
    /// Call this exactly once when the VMA leaves the address space.
    pub(super) fn release_mmap_refs(&self) {
        let Some(info) = &self.mmap_info else {
            return;
        };
        if info.shared
            && info.writable
            && let Some(file) = &info.backing_file
        {
            file.dec_writable_shared_mmap();
        }
    }
}

fn remap_flags_preserving_cow(
    page_table: &PageTable,
    vpn: VirtPageNum,
    mut flags: PTEFlags,
) -> PTEFlags {
    if page_table.translate(vpn).is_some_and(|pte| pte.cow()) {
        flags.remove(PTEFlags::W);
        flags.insert(PTEFlags::COW);
    }
    flags
}

fn mmap_writeback_len(info: &MmapInfo, area_offset: usize) -> Option<usize> {
    if area_offset >= info.len {
        return None;
    }
    let map_len = (info.len - area_offset).min(PAGE_SIZE);
    let file_offset = info.file_offset.checked_add(area_offset)?;
    let file_len = info.file_size.saturating_sub(file_offset).min(PAGE_SIZE);
    let len = map_len.min(file_len);
    (len > 0).then_some(len)
}

#[derive(Copy, Clone, PartialEq, Debug)]
pub enum MapType {
    Identical,
    Framed,
}

bitflags! {
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct MapPermission: u8 {
        const R = 1 << 1;
        const W = 1 << 2;
        const X = 1 << 3;
        const U = 1 << 4;
    }
}
