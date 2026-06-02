use super::address::page_align_up;
use super::area::{ExecSegmentInfo, MmapInfo, ShmAreaInfo};
use super::page_table::PTEFlags;
use super::{
    FrameTracker, MapArea, MapPermission, MapType, MemorySet, MmapFlush, PageTableEntry,
    PhysPageNum, VPNRange, VirtAddr,
};
use super::{VirtPageNum, frame_alloc, frame_ref_count};
use crate::arch::mm as arch_mm;
use crate::config::{PAGE_SIZE, USER_MMAP_BASE, USER_MMAP_LIMIT};
use crate::fs::{File, FsError};
use crate::mm::page_cache::{PAGE_CACHE, PAGE_CACHE_SOFT_MAX_PAGES, PageCacheId, PageCacheKey};
use crate::perf;
use alloc::collections::BTreeMap;
use alloc::sync::Arc;
use alloc::vec::Vec;

// Leave unmapped space below MAP_GROWSDOWN expansion so a stack-like VMA does
// not grow into an adjacent mapping when handling one-page-at-a-time faults.
const STACK_GUARD_GAP_PAGES: usize = 256;
const TLB_RANGE_FULL_FLUSH_THRESHOLD: usize = 64;

fn flush_tlb_vpn_range(start_vpn: VirtPageNum, end_vpn: VirtPageNum) {
    let pages = end_vpn.0.saturating_sub(start_vpn.0);
    if pages == 0 {
        return;
    }
    if pages > TLB_RANGE_FULL_FLUSH_THRESHOLD {
        perf::record_tlb_flush_all();
        arch_mm::flush_tlb_all();
        return;
    }
    perf::record_tlb_flush_range(pages);
    for vpn in VPNRange::new(start_vpn, end_vpn) {
        arch_mm::flush_tlb_page(usize::from(VirtAddr::from(vpn)));
    }
}

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
    FatalSigsegv,
    FatalSigbus,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) enum FutexSharedKey {
    File {
        id: PageCacheId,
        offset: usize,
    },
    VfsNode {
        node: crate::fs::VfsNodeId,
        offset: usize,
    },
    FileObject {
        object: usize,
        offset: usize,
    },
    Shm {
        shmid: usize,
        offset: usize,
    },
    AnonymousPage {
        ppn: usize,
        offset: usize,
    },
}

enum GrowDownMmapFault {
    Grown(usize),
    GuardBlocked,
}

pub struct MmapFaultPage {
    vpn: VirtPageNum,
    file_offset: usize,
    dst_offset: usize,
    read_len: usize,
    backing_file: Option<Arc<dyn File + Send + Sync>>,
    exec_fault: bool,
    zero_fill_len: usize,
}

impl MmapFaultPage {
    /// Allocates and optionally fills the private frame for a mmap fault.
    ///
    /// The returned frame is not installed into any page table yet; callers
    /// must revalidate the VMA and install it through `MemorySet`.
    pub fn build_frame(&self) -> Option<FrameTracker> {
        let frame = match frame_alloc() {
            Some(frame) => frame,
            None => {
                crate::fs::reclaim_memcg_pressure_pages();
                frame_alloc()?
            }
        };
        let mut read_len = 0usize;
        if let Some(file) = &self.backing_file
            && self.read_len > 0
        {
            let end = self.dst_offset.checked_add(self.read_len)?;
            let dst = frame.ppn.get_bytes_array().get_mut(self.dst_offset..end)?;
            read_len = file.read_at(self.file_offset, dst);
        }
        if self.exec_fault {
            super::elf_loader::record_exec_lazy_fault(
                read_len,
                self.zero_fill_len
                    .saturating_add(self.read_len.saturating_sub(read_len)),
            );
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
    exec_fault: bool,
}

impl MmapPageCacheFault {
    pub fn key(&self) -> PageCacheKey {
        self.key
    }

    pub fn is_exec_fault(&self) -> bool {
        self.exec_fault
    }

    /// Resolves the shared page-cache frame for a file-backed MAP_SHARED fault.
    ///
    /// This may allocate a new frame and read the backing file when the page is
    /// not already cached. A successful return owns one page-cache reference.
    pub fn resolve_ppn(&self) -> Option<PhysPageNum> {
        let cached_ppn = {
            let mut cache = PAGE_CACHE.exclusive_access();
            if let Some(ppn) = cache.get_and_inc_ref(self.key) {
                if self.exec_fault && !cache.ensure_exec_icache_synced(self.key) {
                    cache.dec_ref(self.key);
                    None
                } else {
                    Some(ppn)
                }
            } else {
                None
            }
        };
        if let Some(ppn) = cached_ppn {
            if self.exec_fault {
                super::elf_loader::record_exec_lazy_page_cache_fault(true, 0);
            }
            return Some(ppn);
        }

        let frame = frame_alloc()?;
        let mut read_len = 0usize;
        if self.read_len > 0 {
            let dst = &mut frame.ppn.get_bytes_array()[..self.read_len];
            read_len = self.backing_file.read_at(self.file_offset, dst);
        }

        let mut cache = PAGE_CACHE.exclusive_access();
        let ppn = cache.insert_loaded_page_and_inc_ref(self.key, frame, self.file_size_at_load);
        if self.exec_fault && !cache.ensure_exec_icache_synced(self.key) {
            cache.dec_ref(self.key);
            return None;
        }
        if self.exec_fault {
            super::elf_loader::record_exec_lazy_page_cache_fault(false, read_len);
        }
        Some(ppn)
    }
}

fn mmap_fault_hits_file_hole(area: &MapArea, info: &MmapInfo, addr: usize) -> bool {
    if info.backing_file.is_none() || info.exec_segment.is_some() {
        return false;
    }
    let area_start = usize::from(VirtAddr::from(area.vpn_range.get_start()));
    let Some(area_offset) = addr.checked_sub(area_start) else {
        return true;
    };
    let page_area_offset = area_offset / PAGE_SIZE * PAGE_SIZE;
    info.file_offset
        .checked_add(page_area_offset)
        .is_none_or(|file_offset| file_offset >= info.file_size)
}

fn mmap_shared_write_hits_enospc(area: &MapArea, info: &MmapInfo, addr: usize) -> bool {
    // CONTEXT: A MAP_SHARED write fault must fail as SIGBUS when the backing
    // file cannot accept a byte at the faulting offset. Check before granting
    // PTE write permission so a later store cannot dirty an unflushable page.
    if !info.shared || !info.writable {
        return false;
    }
    let Some(file) = &info.backing_file else {
        return false;
    };
    let area_start = usize::from(VirtAddr::from(area.vpn_range.get_start()));
    let Some(area_offset) = addr.checked_sub(area_start) else {
        return true;
    };
    let Some(file_offset) = info.file_offset.checked_add(area_offset) else {
        return true;
    };
    matches!(file.check_write_at(file_offset, 1), Err(FsError::NoSpace))
}

fn exec_fault_can_use_page_cache(info: &MmapInfo, fault: &ExecSegmentFault) -> bool {
    !info.writable
        && fault.dst_offset == 0
        && fault.read_len == PAGE_SIZE
        && fault.zero_fill_len == 0
}

fn page_cache_has_page_or_capacity(key: PageCacheKey) -> bool {
    let cache = PAGE_CACHE.exclusive_access();
    cache.contains(key) || cache.len() < PAGE_CACHE_SOFT_MAX_PAGES
}

fn area_is_private_user_writable(area: &MapArea) -> bool {
    area.map_perm.contains(MapPermission::W | MapPermission::U)
        && !area.is_shm()
        && area.mmap_info.as_ref().is_none_or(|info| !info.shared)
}

fn cow_flags_from_pte(pte: PageTableEntry) -> PTEFlags {
    let mut flags = pte.flags();
    flags.remove(PTEFlags::W);
    flags.insert(PTEFlags::COW);
    flags
}

impl MemorySet {
    /// Builds a child address space for fork/clone.
    ///
    /// Writable private user mappings are shared as COW pages. File-backed
    /// MAP_SHARED and SHM mappings keep their shared backing references.
    pub fn from_existed_user(user_space: &mut MemorySet) -> Option<MemorySet> {
        let mut memory_set = Self::try_new_bare()?;
        let mut parent_needs_tlb_flush = false;
        memory_set.brk_base = user_space.brk_base;
        memory_set.brk = user_space.brk;
        memory_set.brk_limit = user_space.brk_limit;
        memory_set.brk_mapped_end = user_space.brk_mapped_end;
        memory_set.mmap_next = user_space.mmap_next;
        memory_set.mlock_future = false;
        memory_set.mlock_future_on_fault = false;
        if !memory_set.map_trampoline() {
            return None;
        }
        for area_idx in 0..user_space.areas.len() {
            if !user_space.ensure_shared_anonymous_mmap_resident(area_idx) {
                return None;
            }
            let area = &user_space.areas[area_idx];
            let new_area = MapArea::from_another(area);
            if area.is_shm() {
                let Some(shmid) = area.shm_segment_id() else {
                    continue;
                };
                if !crate::mm::shm::retain_attached_segment(shmid, 0) {
                    continue;
                }
                let area_idx = memory_set.insert_area_sorted(new_area);
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
                    if !dst_area.map_shm_frame(page_table, vpn, mapping.ppn, page_index) {
                        return None;
                    }
                }
            } else if area.is_mmap() {
                let area_idx = memory_set.insert_area_sorted(new_area);
                if area.is_wipe_on_fork() {
                    continue;
                }
                let cow_resident = area_is_private_user_writable(area);
                let can_share_resident = area.mmap_info.as_ref().is_some_and(|info| info.shared)
                    || !area.map_perm.contains(MapPermission::W)
                    || cow_resident;
                let resident_vpns: Vec<_> = area.data_frames.keys().copied().collect();
                for vpn in resident_vpns {
                    let Some(src_frame) = area.data_frames.get(&vpn) else {
                        continue;
                    };
                    let src_ppn = src_frame.ppn;
                    let frame = if cow_resident || can_share_resident {
                        FrameTracker::from_retained(src_ppn)
                    } else {
                        let frame = frame_alloc()?;
                        frame
                            .ppn
                            .get_bytes_array()
                            .copy_from_slice(src_ppn.get_bytes_array());
                        Some(frame)
                    };
                    let frame = frame?;
                    let pte_flags = if cow_resident {
                        let src_pte = user_space.page_table.translate(vpn)?;
                        cow_flags_from_pte(src_pte)
                    } else {
                        PTEFlags::from_bits_truncate(area.map_perm.bits() as usize)
                    };
                    let page_table = &mut memory_set.page_table;
                    let dst_area = &mut memory_set.areas[area_idx];
                    if !dst_area.map_existing_frame_with_flags(page_table, vpn, frame, pte_flags) {
                        return None;
                    }
                    if cow_resident {
                        if !user_space.page_table.mark_cow_readonly(vpn) {
                            return None;
                        }
                        parent_needs_tlb_flush = true;
                    }
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
                        return None;
                    }
                }
            } else if area_is_private_user_writable(area) {
                let area_idx = memory_set.insert_area_sorted(new_area);
                let resident_vpns: Vec<_> = area.data_frames.keys().copied().collect();
                for vpn in resident_vpns {
                    let src_pte = user_space.page_table.translate(vpn)?;
                    let frame = FrameTracker::from_retained(src_pte.ppn())?;
                    let pte_flags = cow_flags_from_pte(src_pte);
                    let page_table = &mut memory_set.page_table;
                    let dst_area = &mut memory_set.areas[area_idx];
                    if !dst_area.map_existing_frame_with_flags(page_table, vpn, frame, pte_flags) {
                        return None;
                    }
                    if !user_space.page_table.mark_cow_readonly(vpn) {
                        return None;
                    }
                    parent_needs_tlb_flush = true;
                }
            } else if area.map_perm.contains(MapPermission::W) {
                if !memory_set.push(new_area, None) {
                    return None;
                }
                for vpn in area.vpn_range {
                    let src_ppn = user_space.translate(vpn).map(|pte| pte.ppn())?;
                    let dst_ppn = memory_set.translate(vpn).map(|pte| pte.ppn())?;
                    dst_ppn
                        .get_bytes_array()
                        .copy_from_slice(src_ppn.get_bytes_array());
                }
            } else {
                let area_idx = memory_set.insert_area_sorted(new_area);
                let resident_vpns: Vec<_> = area.data_frames.keys().copied().collect();
                for vpn in resident_vpns {
                    let Some(src_pte) = user_space.translate(vpn) else {
                        continue;
                    };
                    let frame = FrameTracker::from_retained(src_pte.ppn())?;
                    let page_table = &mut memory_set.page_table;
                    let dst_area = &mut memory_set.areas[area_idx];
                    if !dst_area.map_existing_frame(page_table, vpn, frame) {
                        return None;
                    }
                }
            }
        }
        if parent_needs_tlb_flush {
            arch_mm::flush_tlb_all();
        }
        Some(memory_set)
    }

    /// Materializes lazy MAP_SHARED anonymous pages before fork.
    ///
    /// Parent and child must keep the same physical frames after fork; leaving
    /// this VMA lazy would let each side fault in a different frame later.
    fn ensure_shared_anonymous_mmap_resident(&mut self, area_idx: usize) -> bool {
        let area = &self.areas[area_idx];
        let shared_anonymous = area.mmap_info.as_ref().is_some_and(|info| {
            info.shared && info.backing_file.is_none() && info.page_cache_id.is_none()
        });
        if !shared_anonymous {
            return true;
        }

        let vpn_range = area.vpn_range;
        for vpn in vpn_range {
            if self.translate(vpn).is_some_and(|pte| pte.bits != 0) {
                continue;
            }
            let Some(frame) = frame_alloc() else {
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

    pub fn resolve_cow_page_fault(&mut self, addr: usize) -> bool {
        let vpn = VirtAddr::from(addr).floor();
        let Some(pte) = self.page_table.translate(vpn) else {
            return false;
        };
        if !pte.is_valid() || pte.writable() || !pte.cow() {
            return false;
        }
        let Some(area_idx) = self.find_area_idx_containing(vpn) else {
            return false;
        };
        if !self.areas[area_idx].map_perm.contains(MapPermission::W)
            || !self.areas[area_idx].data_frames.contains_key(&vpn)
        {
            return false;
        }

        let Some(ref_count) = frame_ref_count(pte.ppn()) else {
            return false;
        };
        if ref_count == 1 {
            if !self.page_table.restore_write_clear_cow(vpn) {
                return false;
            }
            arch_mm::flush_tlb_page(usize::from(VirtAddr::from(vpn)));
            return true;
        }

        let Some(frame) = frame_alloc() else {
            return false;
        };
        frame
            .ppn
            .get_bytes_array()
            .copy_from_slice(pte.ppn().get_bytes_array());
        let mut flags = pte.flags();
        flags.remove(PTEFlags::COW);
        flags.insert(PTEFlags::W);
        let ppn = frame.ppn;
        if !self.page_table.replace_leaf(vpn, ppn, flags) {
            return false;
        }
        self.areas[area_idx].data_frames.insert(vpn, frame);
        arch_mm::flush_tlb_page(usize::from(VirtAddr::from(vpn)));
        true
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
            if self.mlock_future {
                let mut heap_area = MapArea::new(
                    old_mapped_end.into(),
                    new_mapped_end.into(),
                    MapType::Framed,
                    MapPermission::R | MapPermission::W | MapPermission::U,
                );
                apply_mlock_flags(
                    &mut heap_area,
                    self.mlock_future,
                    self.mlock_future_on_fault,
                );
                if !heap_area.map(&mut self.page_table) {
                    return self.brk;
                }
                self.insert_area_sorted(heap_area);
                self.brk = addr;
                self.brk_mapped_end = new_mapped_end;
                return self.brk;
            }
            let Some(area_idx) = self.find_brk_extension_area(heap_start_vpn, old_end_vpn) else {
                let mut heap_area = MapArea::new(
                    old_mapped_end.into(),
                    new_mapped_end.into(),
                    MapType::Framed,
                    MapPermission::R | MapPermission::W | MapPermission::U,
                );
                apply_mlock_flags(
                    &mut heap_area,
                    self.mlock_future,
                    self.mlock_future_on_fault,
                );
                if !heap_area.map(&mut self.page_table) {
                    return self.brk;
                }
                self.insert_area_sorted(heap_area);
                self.brk = addr;
                self.brk_mapped_end = new_mapped_end;
                return self.brk;
            };
            let heap_area = &mut self.areas[area_idx];
            for vpn in VPNRange::new(old_end_vpn, new_end_vpn) {
                if !heap_area.map_one(&mut self.page_table, vpn) {
                    return self.brk;
                }
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

    /// Creates a non-fixed mmap VMA and returns its chosen start address.
    ///
    /// No user pages are allocated here unless mlock-future state requests
    /// later fault accounting; regular mmap contents are populated lazily by
    /// the page-fault path.
    #[expect(
        clippy::too_many_arguments,
        reason = "mmap metadata mirrors syscall arguments and VMA attributes at the mapping boundary"
    )]
    pub fn mmap_area(
        &mut self,
        len: usize,
        permission: MapPermission,
        reported_permission: MapPermission,
        backing_file: Option<Arc<dyn File + Send + Sync>>,
        file_size: usize,
        file_offset: usize,
        shared: bool,
        writable: bool,
        grow_down: bool,
        page_cache_id: Option<PageCacheId>,
    ) -> Option<usize> {
        let map_len = checked_page_align_up(len)?;
        let start = self.alloc_mmap_range(map_len)?;
        let end = start.checked_add(map_len)?;
        let mut area = MapArea::new(start.into(), end.into(), MapType::Framed, permission);
        area.mmap_info = Some(MmapInfo {
            shared,
            writable,
            grow_down,
            reported_perm: reported_permission,
            len,
            file_offset,
            file_size,
            backing_file,
            page_cache_id,
            page_cache_pages: BTreeMap::new(),
            exec_segment: None,
        });
        apply_mlock_flags(&mut area, self.mlock_future, self.mlock_future_on_fault);
        self.insert_area_sorted(area);
        self.mmap_next = next_mmap_hint(end);
        Some(start)
    }

    /// Replaces an existing virtual range with a fixed mmap area.
    ///
    /// Any removed MAP_SHARED pages are returned as deferred flush records so
    /// the caller can write them back after releasing the memory-set lock.
    #[expect(
        clippy::too_many_arguments,
        reason = "fixed mmap needs the same explicit VMA metadata plus replacement range"
    )]
    pub fn mmap_fixed_area(
        &mut self,
        start: usize,
        len: usize,
        permission: MapPermission,
        reported_permission: MapPermission,
        backing_file: Option<Arc<dyn File + Send + Sync>>,
        file_size: usize,
        file_offset: usize,
        shared: bool,
        writable: bool,
        grow_down: bool,
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
        let mut unmapped = false;
        let mut idx = self.first_area_idx_ending_after(start_vpn);
        let index_skips = idx;
        let mut area_visits = 0usize;
        while idx < self.areas.len() {
            let area_start = self.areas[idx].vpn_range.get_start();
            if area_start >= end_vpn {
                break;
            }
            area_visits += 1;
            let area_end = self.areas[idx].vpn_range.get_end();
            if area_start < end_vpn && area_end > start_vpn {
                let mut area = self.areas.remove(idx);
                unmapped = true;
                if area.is_mmap() {
                    flushes.extend(area.take_mmap_flushes(&mut self.page_table));
                    area.release_mmap_refs();
                } else if area.is_shm() {
                    area.unmap_resident(&mut self.page_table);
                } else {
                    area.unmap(&mut self.page_table);
                }
            } else {
                idx += 1;
            }
        }
        perf::record_vma_range_scan(area_visits, index_skips);
        if unmapped {
            arch_mm::flush_tlb_all();
        }

        let mut area = MapArea::new(start.into(), end.into(), MapType::Framed, permission);
        area.mmap_info = Some(MmapInfo {
            shared,
            writable,
            grow_down,
            reported_perm: reported_permission,
            len,
            file_offset,
            file_size,
            backing_file,
            page_cache_id,
            page_cache_pages: BTreeMap::new(),
            exec_segment: None,
        });
        apply_mlock_flags(&mut area, self.mlock_future, self.mlock_future_on_fault);
        self.insert_area_sorted(area);
        Some((start, flushes))
    }

    pub fn mmap_shared_frames_area(
        &mut self,
        len: usize,
        permission: MapPermission,
        reported_permission: MapPermission,
        backing_file: Arc<dyn File + Send + Sync>,
        pages: &[crate::mm::shm::ShmPageMapping],
    ) -> Option<usize> {
        let map_len = checked_page_align_up(len)?;
        let start = self.alloc_mmap_range(map_len)?;
        let end = start.checked_add(map_len)?;
        let start_vpn = VirtAddr::from(start).floor();
        let mut area = MapArea::new(start.into(), end.into(), MapType::Framed, permission);
        area.mmap_info = Some(MmapInfo {
            shared: true,
            writable: permission.contains(MapPermission::W),
            grow_down: false,
            reported_perm: reported_permission,
            len,
            file_offset: 0,
            file_size: len,
            backing_file: Some(backing_file),
            page_cache_id: None,
            page_cache_pages: BTreeMap::new(),
            exec_segment: None,
        });
        apply_mlock_flags(&mut area, self.mlock_future, self.mlock_future_on_fault);
        for mapping in pages {
            if mapping.page_index >= map_len / PAGE_SIZE {
                continue;
            }
            let vpn = VirtPageNum(start_vpn.0 + mapping.page_index);
            let frame = FrameTracker::from_retained(mapping.ppn)?;
            if !area.map_existing_frame(&mut self.page_table, vpn, frame) {
                area.unmap_resident(&mut self.page_table);
                return None;
            }
        }
        self.insert_area_sorted(area);
        self.mmap_next = next_mmap_hint(end);
        Some(start)
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "ELF segment mapping keeps loader-provided segment metadata explicit"
    )]
    pub(super) fn map_exec_segment_area(
        &mut self,
        start: usize,
        len: usize,
        permission: MapPermission,
        backing_file: Arc<dyn File + Send + Sync>,
        backing_file_size: usize,
        map_file_offset: usize,
        page_cache_id: Option<PageCacheId>,
        exec_segment: ExecSegmentInfo,
    ) -> Option<usize> {
        if start % PAGE_SIZE != 0 || len == 0 {
            return None;
        }
        let map_len = checked_page_align_up(len)?;
        let end = start.checked_add(map_len)?;
        if self.range_overlaps(start, end) {
            return None;
        }

        let mut area = MapArea::new(start.into(), end.into(), MapType::Framed, permission);
        area.mmap_info = Some(MmapInfo {
            shared: false,
            writable: permission.contains(MapPermission::W),
            grow_down: false,
            reported_perm: permission,
            len,
            file_offset: map_file_offset,
            file_size: backing_file_size,
            backing_file: Some(backing_file),
            page_cache_id,
            page_cache_pages: BTreeMap::new(),
            exec_segment: Some(exec_segment),
        });
        self.insert_area_sorted(area);
        Some(start)
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
        apply_mlock_flags(&mut area, self.mlock_future, self.mlock_future_on_fault);
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
        self.insert_area_sorted(area);
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

    pub fn shm_segment_id_for_range(&self, start: usize, len: usize) -> Option<usize> {
        if len == 0 {
            return None;
        }
        let end = start.checked_add(len - 1)?;
        let start_vpn = VirtAddr::from(start).floor();
        let end_vpn = VirtAddr::from(end).floor();
        self.find_area_idx_containing(start_vpn)
            .and_then(|idx| {
                let area = &self.areas[idx];
                (area.is_shm() && end_vpn < area.vpn_range.get_end()).then_some(area)
            })
            .and_then(MapArea::shm_segment_id)
    }

    pub(crate) fn futex_shared_key(&self, addr: usize) -> Option<FutexSharedKey> {
        let vpn = VirtAddr::from(addr).floor();
        let idx = self.find_area_idx_containing(vpn)?;
        let area = &self.areas[idx];
        let area_start = usize::from(VirtAddr::from(area.vpn_range.get_start()));
        let area_offset = addr.checked_sub(area_start)?;
        if let Some(info) = &area.mmap_info
            && info.shared
        {
            if let Some(id) = info.page_cache_id {
                return Some(FutexSharedKey::File {
                    id,
                    offset: info.file_offset.checked_add(area_offset)?,
                });
            }
            if let Some(file) = &info.backing_file {
                let offset = info.file_offset.checked_add(area_offset)?;
                if let Some(node) = file.vfs_node_id() {
                    return Some(FutexSharedKey::VfsNode { node, offset });
                }
                return Some(FutexSharedKey::FileObject {
                    object: Arc::as_ptr(file) as *const () as usize,
                    offset,
                });
            }
            if info.backing_file.is_none()
                && let Some(pte) = self.page_table.translate(vpn).filter(|pte| pte.bits != 0)
            {
                return Some(FutexSharedKey::AnonymousPage {
                    ppn: pte.ppn().0,
                    offset: addr & (PAGE_SIZE - 1),
                });
            }
        }
        if let Some(info) = &area.shm_info {
            return Some(FutexSharedKey::Shm {
                shmid: info.shmid,
                offset: info.offset.checked_add(area_offset)?,
            });
        }
        None
    }

    /// Resolves a user mmap fault into either an already-handled fault or work
    /// that must be completed without holding `MemorySet` mutably.
    ///
    /// The returned page work may allocate frames or read files later, so the
    /// caller must revalidate the VMA through the install helpers.
    pub fn prepare_mmap_page_fault(
        &mut self,
        addr: usize,
        access: MmapFaultAccess,
    ) -> Option<MmapFaultResult> {
        let vpn = VirtAddr::from(addr).floor();
        let area_idx = match self.find_area_idx_containing(vpn) {
            Some(idx) if self.areas[idx].is_mmap() => idx,
            Some(_) => return None,
            None => match self.grow_down_mmap_area_for_fault(vpn, access) {
                Some(GrowDownMmapFault::Grown(idx)) => idx,
                Some(GrowDownMmapFault::GuardBlocked) => {
                    return Some(MmapFaultResult::FatalSigsegv);
                }
                None => return None,
            },
        };
        let area = &self.areas[area_idx];
        if area.is_poisoned(vpn) {
            return Some(MmapFaultResult::FatalSigbus);
        }
        if !access.is_allowed_by(area.map_perm) {
            return None;
        }
        if let Some(pte) = self.translate(vpn).filter(|pte| pte.bits != 0) {
            if access == MmapFaultAccess::Write && !pte.writable() {
                let Some(info) = area.mmap_info.as_ref() else {
                    return None;
                };
                if mmap_fault_hits_file_hole(area, info, addr) {
                    return Some(MmapFaultResult::FatalSigbus);
                }
                if mmap_shared_write_hits_enospc(area, info, addr) {
                    return Some(MmapFaultResult::FatalSigbus);
                }
                let key = {
                    if info.shared && info.writable {
                        info.page_cache_pages.get(&vpn).copied()
                    } else {
                        None
                    }
                }?;
                if !PAGE_CACHE.exclusive_access().mark_dirty(key) {
                    return None;
                }
                let pte_flags = crate::mm::page_table::PTEFlags::from_bits_truncate(
                    self.areas[area_idx].map_perm.bits() as usize,
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

        let info = area
            .mmap_info
            .as_ref()
            .expect("mmap fault area must carry mmap metadata");
        if mmap_fault_hits_file_hole(area, info, addr) {
            return Some(MmapFaultResult::FatalSigbus);
        }
        if access == MmapFaultAccess::Write && mmap_shared_write_hits_enospc(area, info, addr) {
            return Some(MmapFaultResult::FatalSigbus);
        }
        let area_offset = (vpn.0 - area.vpn_range.get_start().0) * PAGE_SIZE;
        if let Some(exec_segment) = &info.exec_segment {
            let fault = exec_segment_fault(exec_segment, area_offset)?;
            if let (Some(page_cache_id), Some(backing_file)) =
                (info.page_cache_id, &info.backing_file)
                && exec_fault_can_use_page_cache(info, &fault)
                && let Some(key) = PageCacheKey::from_file_offset(page_cache_id, fault.file_offset)
                && page_cache_has_page_or_capacity(key)
            {
                return Some(MmapFaultResult::PageCache(MmapPageCacheFault {
                    vpn,
                    key,
                    file_offset: fault.file_offset,
                    read_len: fault.read_len,
                    file_size_at_load: info.file_size,
                    backing_file: backing_file.clone(),
                    exec_fault: true,
                }));
            }
            return Some(MmapFaultResult::Page(MmapFaultPage {
                vpn,
                file_offset: fault.file_offset,
                dst_offset: fault.dst_offset,
                read_len: fault.read_len,
                backing_file: info.backing_file.clone(),
                exec_fault: true,
                zero_fill_len: fault.zero_fill_len,
            }));
        }
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
            && let Some(key) = PageCacheKey::from_file_offset(page_cache_id, file_offset)
        {
            return Some(MmapFaultResult::PageCache(MmapPageCacheFault {
                vpn,
                key,
                file_offset,
                read_len,
                file_size_at_load: info.file_size,
                backing_file: backing_file.clone(),
                exec_fault: false,
            }));
        }
        Some(MmapFaultResult::Page(MmapFaultPage {
            vpn,
            file_offset,
            dst_offset: 0,
            read_len,
            backing_file: info.backing_file.clone(),
            exec_fault: false,
            zero_fill_len: 0,
        }))
    }

    /// Installs a frame produced by `MmapFaultPage::build_frame`.
    ///
    /// The VMA is looked up again because the caller may have dropped process
    /// memory state while allocating or reading the backing file.
    pub fn install_mmap_fault_page(&mut self, page: MmapFaultPage, frame: FrameTracker) -> bool {
        let Some(idx) = self.find_area_idx_containing(page.vpn) else {
            return false;
        };
        if !self.areas[idx].is_mmap() {
            return false;
        }
        let page_table = &mut self.page_table;
        let area = &mut self.areas[idx];
        area.map_existing_frame(page_table, page.vpn, frame)
    }

    /// Installs a page-cache frame resolved for a MAP_SHARED mmap fault.
    ///
    /// The page-cache reference belongs to this mapping only if installation
    /// succeeds; callers must drop that reference on failure.
    pub fn install_mmap_page_cache_fault_page(
        &mut self,
        page: MmapPageCacheFault,
        ppn: PhysPageNum,
    ) -> bool {
        let Some(idx) = self.find_area_idx_containing(page.vpn) else {
            return false;
        };
        if !self.areas[idx].is_mmap() {
            return false;
        }
        let page_table = &mut self.page_table;
        let area = &mut self.areas[idx];
        let exec_fault = page.is_exec_fault();
        let installed = area.map_page_cache_frame(page_table, page.vpn, ppn, page.key);
        if installed && exec_fault {
            arch_mm::publish_pte_barrier();
        }
        installed
    }

    /// Unmaps complete mmap VMAs covered by the page-aligned range.
    ///
    /// Returned flush records are deferred filesystem writes and should be
    /// consumed without holding the process memory lock.
    pub fn munmap_area(&mut self, start: usize, len: usize) -> Option<Vec<MmapFlush>> {
        if len == 0 || start % PAGE_SIZE != 0 {
            return None;
        }
        let map_len = checked_page_align_up(len)?;
        let end = start.checked_add(map_len)?;
        let start_vpn = VirtAddr::from(start).floor();
        let end_vpn = VirtAddr::from(end).floor();

        self.split_area_at(start_vpn);
        self.split_area_at(end_vpn);

        let mut flushes = Vec::new();
        let mut unmapped = false;
        let mut idx = self.first_area_idx_ending_after(start_vpn);
        let index_skips = idx;
        let mut area_visits = 0usize;
        while idx < self.areas.len() {
            let area_start = self.areas[idx].vpn_range.get_start();
            if area_start >= end_vpn {
                break;
            }
            area_visits += 1;
            let area_end = self.areas[idx].vpn_range.get_end();
            if self.areas[idx].is_mmap() && area_start >= start_vpn && area_end <= end_vpn {
                let mut area = self.areas.remove(idx);
                unmapped = true;
                flushes.extend(area.take_mmap_flushes(&mut self.page_table));
                area.release_mmap_refs();
            } else {
                idx += 1;
            }
        }
        perf::record_vma_range_scan(area_visits, index_skips);
        if unmapped {
            flush_tlb_vpn_range(start_vpn, end_vpn);
        }
        Some(flushes)
    }

    /// Collects dirty MAP_SHARED writeback records for an `msync` range.
    ///
    /// This does not unmap pages. It snapshots data that must be written after
    /// the caller releases memory-set state.
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
        let (start_idx, end_idx) = self.overlap_area_idx_bounds(start_vpn, end_vpn);
        let mut area_visits = 0usize;
        for area in &self.areas[start_idx..end_idx] {
            area_visits += 1;
            let area_start = area.vpn_range.get_start();
            let area_end = area.vpn_range.get_end();
            if area.is_mmap() && area_start < end_vpn && area_end > start_vpn {
                flushes.extend(area.collect_mmap_flushes(&self.page_table));
            }
        }
        perf::record_vma_range_scan(area_visits, start_idx);
        Some(flushes)
    }

    pub fn mprotect_area(
        &mut self,
        start: usize,
        len: usize,
        permission: MapPermission,
        reported_permission: MapPermission,
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
        let (start_idx, end_idx) = self.overlap_area_idx_bounds(start_vpn, end_vpn);
        let mut area_visits = 0usize;
        for area in &mut self.areas[start_idx..end_idx] {
            area_visits += 1;
            let area_start = area.vpn_range.get_start();
            let area_end = area.vpn_range.get_end();
            if area_start >= start_vpn && area_end <= end_vpn {
                if !area.remap_permission(&mut self.page_table, permission, reported_permission) {
                    return Err(MemoryProtectError::Unmapped);
                }
                touched = true;
            }
        }
        perf::record_vma_range_scan(area_visits, start_idx);
        if !touched {
            return Err(MemoryProtectError::Unmapped);
        }
        flush_tlb_vpn_range(start_vpn, end_vpn);
        Ok(())
    }

    pub fn additional_locked_bytes_for_range(&self, start: usize, len: usize) -> Option<usize> {
        let (start_vpn, end_vpn) = checked_page_range(start, len)?;
        if !self.range_is_mapped_vpn(start_vpn, end_vpn) {
            return None;
        }
        Some(self.unlocked_pages_in_range(start_vpn, end_vpn) * PAGE_SIZE)
    }

    pub fn additional_locked_bytes_for_current(&self) -> usize {
        self.areas
            .iter()
            .filter(|area| !area.is_locked())
            .map(|area| area.vpn_range.get_end().0 - area.vpn_range.get_start().0)
            .sum::<usize>()
            * PAGE_SIZE
    }

    pub fn locked_bytes(&self) -> usize {
        self.areas.iter().map(MapArea::locked_bytes).sum()
    }

    /// Marks a mapped range as locked for mlock/mlock2 accounting.
    ///
    /// When `on_fault` is false, mmap pages are faulted in before the lock mark
    /// is applied so Linux-visible ENOMEM behavior stays deterministic.
    pub fn mlock_range(&mut self, start: usize, len: usize, on_fault: bool) -> bool {
        let Some((start_vpn, end_vpn)) = checked_page_range(start, len) else {
            return false;
        };
        if !self.range_is_mapped_vpn(start_vpn, end_vpn) {
            return false;
        }
        if !on_fault && !self.prefault_range_for_mlock(start_vpn, end_vpn) {
            return false;
        }
        self.mark_lock_range(start_vpn, end_vpn, on_fault);
        true
    }

    pub fn munlock_range(&mut self, start: usize, len: usize) -> bool {
        let Some((start_vpn, end_vpn)) = checked_page_range(start, len) else {
            return false;
        };
        if !self.range_is_mapped_vpn(start_vpn, end_vpn) {
            return false;
        }
        self.split_area_at(start_vpn);
        self.split_area_at(end_vpn);
        for area in &mut self.areas {
            let area_start = area.vpn_range.get_start();
            let area_end = area.vpn_range.get_end();
            if area_start >= start_vpn && area_end <= end_vpn {
                area.locked = false;
                area.lock_on_fault = false;
            }
        }
        true
    }

    /// Applies mlockall(MCL_CURRENT) to every current VMA.
    ///
    /// Non-ONFAULT mode prefaults mmap pages first; later mappings are governed
    /// separately by `set_mlock_future`.
    pub fn mlock_current(&mut self, on_fault: bool) -> bool {
        if !on_fault {
            let ranges: Vec<_> = self
                .areas
                .iter()
                .map(|area| (area.vpn_range.get_start(), area.vpn_range.get_end()))
                .collect();
            for (start_vpn, end_vpn) in ranges {
                if !self.prefault_range_for_mlock(start_vpn, end_vpn) {
                    return false;
                }
            }
        }
        for area in &mut self.areas {
            apply_mlock_flags(area, true, on_fault);
        }
        true
    }

    pub fn set_mlock_future(&mut self, on_fault: bool) {
        self.mlock_future = true;
        self.mlock_future_on_fault = on_fault;
    }

    pub fn future_mlock_prefaults(&self) -> bool {
        self.mlock_future && !self.mlock_future_on_fault
    }

    pub fn munlock_all(&mut self) {
        for area in &mut self.areas {
            area.locked = false;
            area.lock_on_fault = false;
        }
        self.mlock_future = false;
        self.mlock_future_on_fault = false;
    }

    pub fn mincore_vec(&self, start: usize, len: usize) -> Option<Vec<u8>> {
        let map_len = checked_page_align_up(len)?;
        let end = start.checked_add(map_len)?;
        let start_vpn = VirtAddr::from(start).floor();
        let end_vpn = VirtAddr::from(end).floor();
        if !self.range_is_mapped_vpn(start_vpn, end_vpn) {
            return None;
        }
        let mut vec = Vec::new();
        for vpn in VPNRange::new(start_vpn, end_vpn) {
            let resident = self
                .page_table
                .translate(vpn)
                .is_some_and(|pte| pte.bits != 0 && pte.ppn().0 != 0);
            vec.push(if resident { 1 } else { 0 });
        }
        Some(vec)
    }

    pub fn madvise_range_is_mapped(&self, start: usize, len: usize) -> Option<bool> {
        let (start_vpn, end_vpn) = checked_page_range(start, len)?;
        Some(self.range_is_mapped_vpn(start_vpn, end_vpn))
    }

    pub fn madvise_range_has_locked(&self, start: usize, len: usize) -> Option<bool> {
        let (start_vpn, end_vpn) = checked_page_range(start, len)?;
        if !self.range_is_mapped_vpn(start_vpn, end_vpn) {
            return None;
        }
        Some(self.areas.iter().any(|area| {
            area.vpn_range.get_start() < end_vpn
                && area.vpn_range.get_end() > start_vpn
                && area.is_locked()
        }))
    }

    pub fn madvise_range_is_private_anonymous(&self, start: usize, len: usize) -> Option<bool> {
        let (start_vpn, end_vpn) = checked_page_range(start, len)?;
        if !self.range_is_mapped_vpn(start_vpn, end_vpn) {
            return None;
        }
        Some(
            self.areas
                .iter()
                .filter(|area| {
                    area.vpn_range.get_start() < end_vpn && area.vpn_range.get_end() > start_vpn
                })
                .all(MapArea::is_private_anonymous_mmap),
        )
    }

    pub fn madvise_range_is_shared_writable(&self, start: usize, len: usize) -> Option<bool> {
        let (start_vpn, end_vpn) = checked_page_range(start, len)?;
        if !self.range_is_mapped_vpn(start_vpn, end_vpn) {
            return None;
        }
        Some(
            self.areas
                .iter()
                .filter(|area| {
                    area.vpn_range.get_start() < end_vpn && area.vpn_range.get_end() > start_vpn
                })
                .all(MapArea::is_shared_writable_mmap),
        )
    }

    pub fn madvise_set_wipe_on_fork(&mut self, start: usize, len: usize, enabled: bool) -> bool {
        let Some((start_vpn, end_vpn)) = checked_page_range(start, len) else {
            return false;
        };
        if !self.range_is_mapped_vpn(start_vpn, end_vpn) {
            return false;
        }
        self.split_area_at(start_vpn);
        self.split_area_at(end_vpn);
        for area in &mut self.areas {
            let area_start = area.vpn_range.get_start();
            let area_end = area.vpn_range.get_end();
            if area_start >= start_vpn && area_end <= end_vpn {
                area.set_wipe_on_fork(enabled);
            }
        }
        true
    }

    pub fn madvise_set_dumpable(&mut self, start: usize, len: usize, enabled: bool) -> bool {
        let Some((start_vpn, end_vpn)) = checked_page_range(start, len) else {
            return false;
        };
        if !self.range_is_mapped_vpn(start_vpn, end_vpn) {
            return false;
        }
        self.split_area_at(start_vpn);
        self.split_area_at(end_vpn);
        for area in &mut self.areas {
            let area_start = area.vpn_range.get_start();
            let area_end = area.vpn_range.get_end();
            if area_start >= start_vpn && area_end <= end_vpn && area.is_mmap() {
                area.set_dumpable(enabled);
            }
        }
        true
    }

    pub fn madvise_poison_range(&mut self, start: usize, len: usize) -> bool {
        let Some((start_vpn, end_vpn)) = checked_page_range(start, len) else {
            return false;
        };
        if !self.range_is_mapped_vpn(start_vpn, end_vpn) {
            return false;
        }
        self.split_area_at(start_vpn);
        self.split_area_at(end_vpn);
        let mut poisoned = false;
        for area in &mut self.areas {
            let area_start = area.vpn_range.get_start();
            let area_end = area.vpn_range.get_end();
            if area_start >= start_vpn && area_end <= end_vpn && area.is_private_anonymous_mmap() {
                area.poison_pages(&mut self.page_table, start_vpn, end_vpn);
                poisoned = true;
            }
        }
        if poisoned {
            arch_mm::flush_tlb_all();
        }
        poisoned
    }

    pub fn madvise_mark_lazy_free(&mut self, start: usize, len: usize) -> bool {
        let Some((start_vpn, end_vpn)) = checked_page_range(start, len) else {
            return false;
        };
        if !self.range_is_mapped_vpn(start_vpn, end_vpn) {
            return false;
        }
        self.split_area_at(start_vpn);
        self.split_area_at(end_vpn);
        let mut marked = false;
        for area in &mut self.areas {
            let area_start = area.vpn_range.get_start();
            let area_end = area.vpn_range.get_end();
            if area_start >= start_vpn && area_end <= end_vpn && area.is_private_anonymous_mmap() {
                area.mark_lazy_free_pages(start_vpn, end_vpn);
                marked = true;
            }
        }
        marked
    }

    pub fn madvise_dontneed_range(&mut self, start: usize, len: usize) -> bool {
        let Some((start_vpn, end_vpn)) = checked_page_range(start, len) else {
            return false;
        };
        if !self.range_is_mapped_vpn(start_vpn, end_vpn) {
            return false;
        }
        self.split_area_at(start_vpn);
        self.split_area_at(end_vpn);
        let mut touched = false;
        for area in &mut self.areas {
            let area_start = area.vpn_range.get_start();
            let area_end = area.vpn_range.get_end();
            if area_start >= start_vpn && area_end <= end_vpn && area.is_mmap() {
                area.unmap_resident(&mut self.page_table);
                touched = true;
            }
        }
        if touched {
            arch_mm::flush_tlb_all();
        }
        true
    }

    pub fn discard_lazy_free_pages(&mut self) -> bool {
        let mut discarded = false;
        for area in &mut self.areas {
            if area.discard_lazy_free_pages(&mut self.page_table) {
                discarded = true;
            }
        }
        if discarded {
            arch_mm::flush_tlb_all();
        }
        discarded
    }

    pub fn discard_memcg_pressure_pages(&mut self) -> bool {
        let mut discarded = false;
        for area in &mut self.areas {
            if area.discard_memcg_pressure_pages(&mut self.page_table) {
                discarded = true;
            }
        }
        if discarded {
            arch_mm::flush_tlb_all();
        }
        discarded
    }

    pub fn core_dump_bytes(&self, max_len: usize) -> Vec<u8> {
        let mut output = Vec::new();
        for area in &self.areas {
            if !area.is_dumpable() {
                continue;
            }
            for vpn in area.vpn_range {
                if output.len() >= max_len {
                    return output;
                }
                let Some(pte) = self.page_table.translate(vpn).filter(|pte| pte.bits != 0) else {
                    continue;
                };
                let page = pte.ppn().get_bytes_array();
                let remaining = max_len - output.len();
                output.extend_from_slice(&page[..PAGE_SIZE.min(remaining)]);
            }
        }
        output
    }

    pub fn prefault_mmap_range(&mut self, start: usize, len: usize) -> bool {
        let Some((start_vpn, end_vpn)) = checked_page_range(start, len) else {
            return false;
        };
        for vpn in VPNRange::new(start_vpn, end_vpn) {
            if !self.ensure_vpn_resident_for_mlock(vpn) {
                return false;
            }
        }
        true
    }

    pub fn mremap_area(
        &mut self,
        old_addr: usize,
        old_len: usize,
        new_len: usize,
        may_move: bool,
    ) -> Option<(usize, Vec<MmapFlush>)> {
        if old_addr % PAGE_SIZE != 0 || old_len == 0 || new_len == 0 {
            return None;
        }
        let old_map_len = checked_page_align_up(old_len)?;
        let new_map_len = checked_page_align_up(new_len)?;
        let old_end = old_addr.checked_add(old_map_len)?;
        let new_end = old_addr.checked_add(new_map_len)?;
        let old_start_vpn = VirtAddr::from(old_addr).floor();
        let old_end_vpn = VirtAddr::from(old_end).floor();

        if !self.range_is_mapped_vpn(old_start_vpn, old_end_vpn) {
            return None;
        }
        if new_map_len == old_map_len {
            return Some((old_addr, Vec::new()));
        }
        if new_map_len < old_map_len {
            let tail = old_addr.checked_add(new_map_len)?;
            let flushes = self.munmap_area(tail, old_map_len - new_map_len)?;
            return Some((old_addr, flushes));
        }
        if self.range_overlaps(old_end, new_end) {
            if may_move {
                // UNFINISHED: MREMAP_MAYMOVE relocation is not implemented yet;
                // current LTP mmap16 only needs in-place growth into a free gap.
            }
            return None;
        }

        self.split_area_at(old_start_vpn);
        self.split_area_at(old_end_vpn);
        let idx = self.find_area_idx_by_start(old_start_vpn)?;
        if self.areas[idx].vpn_range.get_end() != old_end_vpn {
            return None;
        }
        let new_end_vpn = VirtAddr::from(new_end).floor();
        self.areas[idx].vpn_range = VPNRange::new(old_start_vpn, new_end_vpn);
        if let Some(info) = self.areas[idx].mmap_info.as_mut() {
            info.len = new_len;
        }
        self.areas[idx].write_protect_shared_mmap_pages(&mut self.page_table);
        self.last_area_idx_containing.set(None);
        arch_mm::flush_tlb_all();
        Some((old_addr, Vec::new()))
    }

    fn unlocked_pages_in_range(&self, start: super::VirtPageNum, end: super::VirtPageNum) -> usize {
        let mut pages = 0;
        for vpn in VPNRange::new(start, end) {
            let locked = self.areas.iter().any(|area| {
                area.vpn_range.get_start() <= vpn
                    && vpn < area.vpn_range.get_end()
                    && area.is_locked()
            });
            if !locked {
                pages += 1;
            }
        }
        pages
    }

    fn prefault_range_for_mlock(
        &mut self,
        start: super::VirtPageNum,
        end: super::VirtPageNum,
    ) -> bool {
        for vpn in VPNRange::new(start, end) {
            if !self.ensure_vpn_resident_for_mlock(vpn) {
                return false;
            }
        }
        true
    }

    fn ensure_vpn_resident_for_mlock(&mut self, vpn: super::VirtPageNum) -> bool {
        if self
            .page_table
            .translate(vpn)
            .is_some_and(|pte| pte.bits != 0 && pte.ppn().0 != 0)
        {
            return true;
        }
        let Some(area) = self
            .areas
            .iter()
            .find(|area| area.vpn_range.get_start() <= vpn && vpn < area.vpn_range.get_end())
        else {
            return false;
        };
        if !area.is_mmap() {
            return false;
        }
        let access = mlock_fault_access(area.map_perm);
        let addr = usize::from(VirtAddr::from(vpn));
        let Some(fault) = self.prepare_mmap_page_fault(addr, access) else {
            return false;
        };
        match fault {
            MmapFaultResult::Handled => true,
            MmapFaultResult::FatalSigsegv | MmapFaultResult::FatalSigbus => false,
            MmapFaultResult::Page(page) => {
                let Some(frame) = page.build_frame() else {
                    return false;
                };
                self.install_mmap_fault_page(page, frame)
            }
            MmapFaultResult::PageCache(page) => {
                let Some(ppn) = page.resolve_ppn() else {
                    return false;
                };
                let key = page.key();
                let installed = self.install_mmap_page_cache_fault_page(page, ppn);
                if !installed {
                    PAGE_CACHE.exclusive_access().dec_ref(key);
                }
                installed
            }
        }
    }

    fn mark_lock_range(
        &mut self,
        start_vpn: super::VirtPageNum,
        end_vpn: super::VirtPageNum,
        on_fault: bool,
    ) {
        self.split_area_at(start_vpn);
        self.split_area_at(end_vpn);
        for area in &mut self.areas {
            let area_start = area.vpn_range.get_start();
            let area_end = area.vpn_range.get_end();
            if area_start >= start_vpn && area_end <= end_vpn {
                apply_mlock_flags(area, true, on_fault);
            }
        }
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
        let mut gap_checks = 0;
        let mut area_visits = 0;
        let vma_count = self.areas.len();

        if start >= limit || len == 0 {
            perf::record_mmap_hole_search(0, gap_checks, area_visits, vma_count);
            return None;
        }
        let mut cursor = page_align_up(start);
        loop {
            let Some(end) = cursor.checked_add(len) else {
                perf::record_mmap_hole_search(0, gap_checks, area_visits, vma_count);
                return None;
            };
            if end > limit {
                perf::record_mmap_hole_search(0, gap_checks, area_visits, vma_count);
                return None;
            }
            let cursor_vpn = VirtAddr::from(cursor).floor();
            let mut idx = self.area_insert_index(cursor_vpn);
            if idx > 0 {
                area_visits += 1;
                let prev_end = usize::from(VirtAddr::from(self.areas[idx - 1].vpn_range.get_end()));
                if prev_end > cursor {
                    cursor = page_align_up(prev_end);
                    continue;
                }
            }
            while idx < self.areas.len() {
                area_visits += 1;
                let area_end = usize::from(VirtAddr::from(self.areas[idx].vpn_range.get_end()));
                if area_end > cursor {
                    break;
                }
                idx += 1;
            }
            gap_checks += 1;
            if idx >= self.areas.len() {
                perf::record_mmap_hole_search(0, gap_checks, area_visits, vma_count);
                return Some(cursor);
            }
            let area = &self.areas[idx];
            let area_start = usize::from(VirtAddr::from(area.vpn_range.get_start()));
            let area_end = usize::from(VirtAddr::from(area.vpn_range.get_end()));
            if area_start >= limit {
                perf::record_mmap_hole_search(0, gap_checks, area_visits, vma_count);
                return Some(cursor);
            }
            if end <= area_start {
                perf::record_mmap_hole_search(0, gap_checks, area_visits, vma_count);
                return Some(cursor);
            }
            cursor = page_align_up(area_end);
        }
    }

    pub(crate) fn range_overlaps(&self, start: usize, end: usize) -> bool {
        if start >= end {
            return false;
        }
        let start_vpn = VirtAddr::from(start).floor();
        let end_vpn = VirtAddr::from(end).floor();
        let idx = self.area_insert_index(start_vpn);
        if idx > 0 && self.areas[idx - 1].vpn_range.get_end() > start_vpn {
            return true;
        }
        idx < self.areas.len()
            && self.areas[idx].vpn_range.get_start() < end_vpn
            && self.areas[idx].vpn_range.get_end() > start_vpn
    }

    fn range_is_mapped_vpn(&self, start: super::VirtPageNum, end: super::VirtPageNum) -> bool {
        let mut cursor = start;
        let mut idx = self.first_area_idx_ending_after(start);
        while cursor < end {
            let Some(area) = self.areas.get(idx) else {
                return false;
            };
            let area_start = area.vpn_range.get_start();
            let area_end = area.vpn_range.get_end();
            if area_start > cursor || area_end <= cursor {
                return false;
            }
            cursor = area_end.min(end);
            idx += 1;
        }
        true
    }

    fn first_area_idx_ending_after(&self, start: super::VirtPageNum) -> usize {
        let idx = self.area_insert_index(start);
        if idx > 0 && self.areas[idx - 1].vpn_range.get_end() > start {
            idx - 1
        } else {
            idx
        }
    }

    fn overlap_area_idx_bounds(
        &self,
        start: super::VirtPageNum,
        end: super::VirtPageNum,
    ) -> (usize, usize) {
        if start >= end {
            return (0, 0);
        }
        let start_idx = self.first_area_idx_ending_after(start);
        let end_idx = self.area_insert_index(end);
        (start_idx.min(end_idx), end_idx)
    }

    fn split_area_at(&mut self, at: super::VirtPageNum) {
        let Some(idx) = self.find_area_idx_containing(at) else {
            return;
        };
        let area_start = self.areas[idx].vpn_range.get_start();
        let area_end = self.areas[idx].vpn_range.get_end();
        if !(area_start < at && at < area_end) {
            return;
        }
        if let Some(right) = self.areas[idx].split_off(at) {
            // Insert the right half immediately after the left half to preserve
            // the sorted VMA invariant used by find_area_idx_containing().
            // The cached index is range-checked before reuse, so a stale hit
            // after splitting can only degrade to a normal binary search.
            self.areas.insert(idx + 1, right);
        }
    }

    fn can_mprotect_write(&self, start: super::VirtPageNum, end: super::VirtPageNum) -> bool {
        let (start_idx, end_idx) = self.overlap_area_idx_bounds(start, end);
        let mut area_visits = 0usize;
        for area in &self.areas[start_idx..end_idx] {
            area_visits += 1;
            let area_start = area.vpn_range.get_start();
            let area_end = area.vpn_range.get_end();
            if !(area_start < end && area_end > start) {
                continue;
            }
            let Some(info) = &area.mmap_info else {
                continue;
            };
            if !info.shared {
                continue;
            }
            if info
                .backing_file
                .as_ref()
                .is_some_and(|file| !file.writable() || file.blocks_shared_writable_mmap())
            {
                perf::record_vma_range_scan(area_visits, start_idx);
                return false;
            }
        }
        perf::record_vma_range_scan(area_visits, start_idx);
        true
    }

    fn grow_down_mmap_area_for_fault(
        &mut self,
        vpn: super::VirtPageNum,
        access: MmapFaultAccess,
    ) -> Option<GrowDownMmapFault> {
        let next_vpn = VirtPageNum(vpn.0.checked_add(1)?);
        let area_idx = self.find_area_idx_by_start(next_vpn)?;
        let area = &self.areas[area_idx];
        let info = area.mmap_info.as_ref()?;
        // UNFINISHED: Linux also checks the faulting stack pointer,
        // RLIMIT_STACK, and more VMA flags. This handles the contest
        // pthread/LTP path by growing anonymous MAP_GROWSDOWN VMAs one
        // page at a time.
        if !info.grow_down || info.backing_file.is_some() || !access.is_allowed_by(area.map_perm) {
            return None;
        }

        if !self.grow_down_guard_gap_is_clear(vpn, area_idx) {
            return Some(GrowDownMmapFault::GuardBlocked);
        }

        let end = self.areas[area_idx].vpn_range.get_end();
        self.areas[area_idx].vpn_range = VPNRange::new(vpn, end);
        Some(GrowDownMmapFault::Grown(area_idx))
    }

    fn grow_down_guard_gap_is_clear(&self, new_start: super::VirtPageNum, grow_idx: usize) -> bool {
        let guard_start = new_start.0.saturating_sub(STACK_GUARD_GAP_PAGES);
        self.areas.iter().enumerate().all(|(idx, area)| {
            if idx == grow_idx {
                return true;
            }
            area.vpn_range.get_start().0 >= new_start.0 || area.vpn_range.get_end().0 <= guard_start
        })
    }
}

fn checked_page_align_up(addr: usize) -> Option<usize> {
    addr.checked_add(PAGE_SIZE - 1)
        .map(|addr| addr & !(PAGE_SIZE - 1))
}

struct ExecSegmentFault {
    file_offset: usize,
    dst_offset: usize,
    read_len: usize,
    zero_fill_len: usize,
}

fn exec_segment_fault(info: &ExecSegmentInfo, area_offset: usize) -> Option<ExecSegmentFault> {
    let page_end = area_offset.checked_add(PAGE_SIZE)?;
    let segment_start = info.page_offset;
    let segment_mem_end = segment_start.checked_add(info.mem_size)?;
    let segment_file_end = segment_start.checked_add(info.file_size)?;
    let mem_start = area_offset.max(segment_start);
    let mem_end = page_end.min(segment_mem_end);
    let mem_len = mem_end.saturating_sub(mem_start);
    let file_start = mem_start.min(segment_file_end);
    let file_end = mem_end.min(segment_file_end);

    let read_len = file_end.saturating_sub(file_start);
    let dst_offset = file_start.saturating_sub(area_offset);
    let file_offset = info
        .file_offset
        .checked_add(file_start.saturating_sub(segment_start))?;
    Some(ExecSegmentFault {
        file_offset,
        dst_offset,
        read_len,
        zero_fill_len: mem_len.saturating_sub(read_len),
    })
}

fn checked_page_range(start: usize, len: usize) -> Option<(VirtPageNum, VirtPageNum)> {
    let start_vpn = VirtAddr::from(start).floor();
    if len == 0 {
        return Some((start_vpn, start_vpn));
    }
    let end = start.checked_add(len)?;
    Some((start_vpn, VirtAddr::from(end).ceil()))
}

fn mlock_fault_access(permission: MapPermission) -> MmapFaultAccess {
    if permission.contains(MapPermission::R) {
        MmapFaultAccess::Read
    } else if permission.contains(MapPermission::W) {
        MmapFaultAccess::Write
    } else {
        MmapFaultAccess::Execute
    }
}

fn apply_mlock_flags(area: &mut MapArea, locked: bool, on_fault: bool) {
    if !locked {
        return;
    }
    if on_fault {
        if !area.locked {
            area.lock_on_fault = true;
        }
    } else {
        area.locked = true;
        area.lock_on_fault = false;
    }
}

fn normalized_mmap_hint(hint: usize) -> usize {
    if !(USER_MMAP_BASE..USER_MMAP_LIMIT).contains(&hint) {
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
