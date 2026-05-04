use super::frame_alloc;
use super::page_table::PTEFlags;
use super::{
    FrameTracker, PageTable, PhysAddr, PhysPageNum, StepByOne, VPNRange, VirtAddr, VirtPageNum,
};
use crate::arch::mm as arch_mm;
use crate::config::PAGE_SIZE;
use crate::fs::File;
use alloc::collections::BTreeMap;
use alloc::sync::Arc;
use alloc::vec::Vec;

pub struct MapArea {
    pub(super) vpn_range: VPNRange,
    pub(super) data_frames: BTreeMap<VirtPageNum, FrameTracker>,
    pub(super) map_type: MapType,
    pub(super) map_perm: MapPermission,
    pub(super) mmap_info: Option<MmapInfo>,
}

pub struct MmapFlush {
    file: Arc<dyn File + Send + Sync>,
    offset: usize,
    data: Vec<u8>,
}

impl MmapFlush {
    pub fn write_back(self) {
        self.file.write_at(self.offset, &self.data);
    }
}

#[derive(Clone)]
pub(super) struct MmapInfo {
    pub(super) shared: bool,
    pub(super) writable: bool,
    pub(super) len: usize,
    pub(super) file_offset: usize,
    pub(super) file_size: usize,
    pub(super) backing_file: Option<Arc<dyn File + Send + Sync>>,
}

impl MmapInfo {
    fn split_off(&mut self, offset: usize) -> Self {
        let right = Self {
            shared: self.shared,
            writable: self.writable,
            len: self.len.saturating_sub(offset),
            file_offset: self.file_offset + offset,
            file_size: self.file_size,
            backing_file: self.backing_file.clone(),
        };
        self.len = self.len.min(offset);
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
        }
    }

    pub(super) fn from_another(another: &MapArea) -> Self {
        Self {
            vpn_range: VPNRange::new(another.vpn_range.get_start(), another.vpn_range.get_end()),
            data_frames: BTreeMap::new(),
            map_type: another.map_type,
            map_perm: another.map_perm,
            mmap_info: another.mmap_info.clone(),
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
            .map(|info| info.split_off((at.0 - start.0) * PAGE_SIZE));
        let right = Self {
            vpn_range: VPNRange::new(at, end),
            data_frames: self.data_frames.split_off(&at),
            map_type: self.map_type,
            map_perm: self.map_perm,
            mmap_info: right_mmap_info,
        };
        self.vpn_range = VPNRange::new(start, at);
        Some(right)
    }

    pub(super) fn remap_permission(
        &mut self,
        page_table: &mut PageTable,
        permission: MapPermission,
    ) -> bool {
        let pte_flags = PTEFlags::from_bits_truncate(permission.bits());
        if self.is_mmap() {
            for vpn in self.data_frames.keys().copied() {
                if !page_table.remap_flags(vpn, pte_flags) {
                    return false;
                }
            }
        } else {
            for vpn in self.vpn_range {
                if !page_table.remap_flags(vpn, pte_flags) {
                    return false;
                }
            }
        }
        self.map_perm = permission;
        true
    }

    pub(super) fn map_one(&mut self, page_table: &mut PageTable, vpn: VirtPageNum) {
        let ppn: PhysPageNum = match self.map_type {
            MapType::Identical => {
                let va: VirtAddr = vpn.into();
                PhysAddr::from(arch_mm::virt_to_phys(usize::from(va))).floor()
            }
            MapType::Framed => {
                let frame = frame_alloc().unwrap();
                let ppn = frame.ppn;
                self.data_frames.insert(vpn, frame);
                ppn
            }
        };
        let pte_flags = PTEFlags::from_bits_truncate(self.map_perm.bits());
        page_table.map(vpn, ppn, pte_flags);
    }

    pub(super) fn unmap_one(&mut self, page_table: &mut PageTable, vpn: VirtPageNum) {
        if self.map_type == MapType::Framed {
            self.data_frames.remove(&vpn);
        }
        page_table.unmap(vpn);
    }

    pub(super) fn map(&mut self, page_table: &mut PageTable) {
        for vpn in self.vpn_range {
            self.map_one(page_table, vpn);
        }
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
        if self.data_frames.contains_key(&vpn) {
            return true;
        }
        if page_table.translate(vpn).is_some_and(|pte| pte.bits != 0) {
            return true;
        }
        let ppn = frame.ppn;
        self.data_frames.insert(vpn, frame);
        let pte_flags = PTEFlags::from_bits_truncate(self.map_perm.bits());
        page_table.map(vpn, ppn, pte_flags);
        true
    }

    pub(super) fn unmap_resident(&mut self, page_table: &mut PageTable) {
        let vpns: Vec<_> = self.data_frames.keys().copied().collect();
        for vpn in vpns {
            page_table.unmap(vpn);
        }
        self.data_frames.clear();
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
                .unwrap()
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
            if area_offset >= info.len {
                continue;
            }
            let copy_len = (info.len - area_offset).min(PAGE_SIZE);
            let src = &page_table.translate(vpn).unwrap().ppn().get_bytes_array()[..copy_len];
            flushes.push(MmapFlush {
                file: file.clone(),
                offset: info.file_offset + area_offset,
                data: src.to_vec(),
            });
        }
        flushes
    }
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
