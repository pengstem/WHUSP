use super::super::address::page_align_up;
use super::super::area::ExecSegmentInfo;
use super::super::{MapPermission, VirtAddr, VirtPageNum};
use super::MmapFaultAccess;
use crate::config::{PAGE_SIZE, USER_MMAP_BASE, USER_MMAP_LIMIT};

pub(super) fn checked_page_align_up(addr: usize) -> Option<usize> {
    addr.checked_add(PAGE_SIZE - 1)
        .map(|addr| addr & !(PAGE_SIZE - 1))
}

pub(super) struct ExecSegmentFault {
    pub(super) file_offset: usize,
    pub(super) dst_offset: usize,
    pub(super) read_len: usize,
    pub(super) zero_fill_len: usize,
}

pub(super) fn exec_segment_fault(
    info: &ExecSegmentInfo,
    area_offset: usize,
) -> Option<ExecSegmentFault> {
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

pub(super) fn checked_page_range(start: usize, len: usize) -> Option<(VirtPageNum, VirtPageNum)> {
    let start_vpn = VirtAddr::from(start).floor();
    if len == 0 {
        return Some((start_vpn, start_vpn));
    }
    let end = start.checked_add(len)?;
    Some((start_vpn, VirtAddr::from(end).ceil()))
}

pub(super) fn prefault_access(permission: MapPermission) -> MmapFaultAccess {
    if permission.contains(MapPermission::R) {
        MmapFaultAccess::Read
    } else if permission.contains(MapPermission::W) {
        MmapFaultAccess::Write
    } else {
        MmapFaultAccess::Execute
    }
}

pub(super) fn normalized_mmap_hint(hint: usize) -> usize {
    if !(USER_MMAP_BASE..USER_MMAP_LIMIT).contains(&hint) {
        USER_MMAP_BASE
    } else {
        page_align_up(hint)
    }
}

pub(super) fn next_mmap_hint(end: usize) -> usize {
    if end >= USER_MMAP_LIMIT {
        USER_MMAP_BASE
    } else {
        end
    }
}
