use super::address::page_align_up;
use super::area::ExecSegmentInfo;
use super::{MapArea, MapPermission, MapType, MemorySet, VirtAddr};
use crate::config::{DL_INTERP_OFFSET, PAGE_SIZE, USER_HEAP_SIZE, USER_MMAP_BASE, USER_STACK_SIZE};
use crate::fs::File;
use crate::sync::UPIntrFreeCell;
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use lazy_static::*;
use xmas_elf::{header, program::Type};

pub struct ElfLoadInfo {
    pub memory_set: MemorySet,
    pub ustack_base: usize,
    pub entry_point: usize,
    pub program_entry: usize,
    pub phdr: usize,
    pub phent: usize,
    pub phnum: usize,
    pub interp_base: usize,
    pub sysinfo_ehdr: usize,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct ExecLoadStats {
    pub elf_header_bytes_read: usize,
    pub phdr_bytes_read: usize,
    pub eager_segment_bytes_read: usize,
    pub lazy_segment_faults: usize,
    pub lazy_segment_bytes_read: usize,
    pub lazy_page_cache_faults: usize,
    pub lazy_page_cache_hits: usize,
    pub lazy_page_cache_misses: usize,
    pub lazy_page_cache_bytes_read: usize,
    pub zero_fill_bytes: usize,
    pub lazy_segment_vmas: usize,
}

lazy_static! {
    static ref EXEC_LOAD_STATS: UPIntrFreeCell<ExecLoadStats> =
        unsafe { UPIntrFreeCell::new(ExecLoadStats::default()) };
}

pub(crate) fn record_exec_metadata_read(header_bytes: usize, phdr_bytes: usize) {
    let mut stats = EXEC_LOAD_STATS.exclusive_access();
    stats.elf_header_bytes_read = stats.elf_header_bytes_read.saturating_add(header_bytes);
    stats.phdr_bytes_read = stats.phdr_bytes_read.saturating_add(phdr_bytes);
}

fn record_exec_eager_segment_bytes_read(bytes: usize) {
    let mut stats = EXEC_LOAD_STATS.exclusive_access();
    stats.eager_segment_bytes_read = stats.eager_segment_bytes_read.saturating_add(bytes);
}

fn record_exec_lazy_segment_vma() {
    let mut stats = EXEC_LOAD_STATS.exclusive_access();
    stats.lazy_segment_vmas = stats.lazy_segment_vmas.saturating_add(1);
}

pub(super) fn record_exec_lazy_fault(bytes_read: usize, zero_fill_bytes: usize) {
    let mut stats = EXEC_LOAD_STATS.exclusive_access();
    stats.lazy_segment_faults = stats.lazy_segment_faults.saturating_add(1);
    stats.lazy_segment_bytes_read = stats.lazy_segment_bytes_read.saturating_add(bytes_read);
    stats.zero_fill_bytes = stats.zero_fill_bytes.saturating_add(zero_fill_bytes);
}

pub(super) fn record_exec_lazy_page_cache_fault(hit: bool, bytes_read: usize) {
    let mut stats = EXEC_LOAD_STATS.exclusive_access();
    stats.lazy_segment_faults = stats.lazy_segment_faults.saturating_add(1);
    stats.lazy_segment_bytes_read = stats.lazy_segment_bytes_read.saturating_add(bytes_read);
    stats.lazy_page_cache_faults = stats.lazy_page_cache_faults.saturating_add(1);
    if hit {
        stats.lazy_page_cache_hits = stats.lazy_page_cache_hits.saturating_add(1);
    } else {
        stats.lazy_page_cache_misses = stats.lazy_page_cache_misses.saturating_add(1);
        stats.lazy_page_cache_bytes_read =
            stats.lazy_page_cache_bytes_read.saturating_add(bytes_read);
    }
}

pub(crate) fn exec_load_stats_snapshot() -> ExecLoadStats {
    *EXEC_LOAD_STATS.exclusive_access()
}

pub(crate) fn exec_load_stats_content() -> String {
    let stats = exec_load_stats_snapshot();
    format!(
        "exec_elf_header_bytes_read {}\n\
         exec_phdr_bytes_read {}\n\
         exec_eager_segment_bytes_read {}\n\
         exec_lazy_segment_vmas {}\n\
         exec_lazy_segment_faults {}\n\
         exec_lazy_segment_bytes_read {}\n\
         exec_lazy_page_cache_faults {}\n\
         exec_lazy_page_cache_hits {}\n\
         exec_lazy_page_cache_misses {}\n\
         exec_lazy_page_cache_bytes_read {}\n\
         exec_zero_fill_bytes {}\n",
        stats.elf_header_bytes_read,
        stats.phdr_bytes_read,
        stats.eager_segment_bytes_read,
        stats.lazy_segment_vmas,
        stats.lazy_segment_faults,
        stats.lazy_segment_bytes_read,
        stats.lazy_page_cache_faults,
        stats.lazy_page_cache_hits,
        stats.lazy_page_cache_misses,
        stats.lazy_page_cache_bytes_read,
        stats.zero_fill_bytes
    )
}

fn phdr_address(elf: &xmas_elf::ElfFile<'_>) -> usize {
    let ph_offset = elf.header.pt2.ph_offset() as usize;
    let ph_size = elf.header.pt2.ph_entry_size() as usize * elf.header.pt2.ph_count() as usize;
    let mut phdr = 0usize;
    for i in 0..elf.header.pt2.ph_count() {
        let ph = elf.program_header(i).unwrap();
        let ph_type = ph.get_type().unwrap();
        if ph_type == Type::Phdr {
            return ph.virtual_addr() as usize;
        }
        if ph_type == Type::Load && phdr == 0 {
            let load_offset = ph.offset() as usize;
            let load_file_end = load_offset + ph.file_size() as usize;
            if ph_offset >= load_offset && ph_offset + ph_size <= load_file_end {
                phdr = ph.virtual_addr() as usize + (ph_offset - load_offset);
            }
        }
    }
    phdr
}

fn main_load_bias(elf: &xmas_elf::ElfFile<'_>) -> usize {
    if elf.header.pt2.type_().as_type() == header::Type::SharedObject {
        USER_MMAP_BASE
    } else {
        0
    }
}

fn bias_nonzero_addr(load_bias: usize, addr: usize) -> usize {
    if addr == 0 { 0 } else { load_bias + addr }
}

fn initial_mmap_next(user_stack_base: usize) -> usize {
    page_align_up(user_stack_base + USER_STACK_SIZE + PAGE_SIZE).max(USER_MMAP_BASE)
}

fn map_elf_load_segments(
    memory_set: &mut MemorySet,
    elf: &xmas_elf::ElfFile<'_>,
    load_bias: usize,
) -> usize {
    let mut max_end_va = 0usize;
    for i in 0..elf.header.pt2.ph_count() {
        let ph = elf.program_header(i).unwrap();
        if ph.get_type().unwrap() != Type::Load {
            continue;
        }
        let start_va: VirtAddr = (load_bias + ph.virtual_addr() as usize).into();
        let end_va: VirtAddr = (load_bias + (ph.virtual_addr() + ph.mem_size()) as usize).into();
        let segment_end = load_bias + ph.virtual_addr() as usize + ph.mem_size() as usize;
        max_end_va = max_end_va.max(segment_end);
        let mut map_perm = MapPermission::U;
        let ph_flags = ph.flags();
        if ph_flags.is_read() {
            map_perm |= MapPermission::R;
        }
        if ph_flags.is_write() {
            map_perm |= MapPermission::W;
        }
        if ph_flags.is_execute() {
            map_perm |= MapPermission::X;
        }
        let map_area = MapArea::new(start_va, end_va, MapType::Framed, map_perm);
        memory_set.push_with_offset(
            map_area,
            Some(&elf.input[ph.offset() as usize..(ph.offset() + ph.file_size()) as usize]),
            start_va.page_offset(),
        );
        record_exec_eager_segment_bytes_read(ph.file_size() as usize);
    }
    max_end_va
}

fn align_down(value: usize, align: usize) -> usize {
    value & !(align - 1)
}

fn map_permission_from_ph_flags(ph: xmas_elf::program::ProgramHeader<'_>) -> MapPermission {
    let mut map_perm = MapPermission::U;
    let ph_flags = ph.flags();
    if ph_flags.is_read() {
        map_perm |= MapPermission::R;
    }
    if ph_flags.is_write() {
        map_perm |= MapPermission::W;
    }
    if ph_flags.is_execute() {
        map_perm |= MapPermission::X;
    }
    map_perm
}

fn phdr_address_checked(elf: &xmas_elf::ElfFile<'_>) -> Option<usize> {
    let ph_offset = elf.header.pt2.ph_offset() as usize;
    let ph_size = elf.header.pt2.ph_entry_size() as usize * elf.header.pt2.ph_count() as usize;
    let mut phdr = 0usize;
    for i in 0..elf.header.pt2.ph_count() {
        let ph = elf.program_header(i).ok()?;
        let ph_type = ph.get_type().ok()?;
        if ph_type == Type::Phdr {
            return Some(ph.virtual_addr() as usize);
        }
        if ph_type == Type::Load && phdr == 0 {
            let load_offset = ph.offset() as usize;
            let load_file_end = load_offset.checked_add(ph.file_size() as usize)?;
            if ph_offset >= load_offset && ph_offset.checked_add(ph_size)? <= load_file_end {
                phdr = ph.virtual_addr() as usize + (ph_offset - load_offset);
            }
        }
    }
    Some(phdr)
}

fn validate_load_segment(
    ph: xmas_elf::program::ProgramHeader<'_>,
    backing_file_size: usize,
) -> Option<()> {
    let file_size = ph.file_size() as usize;
    let mem_size = ph.mem_size() as usize;
    if file_size > mem_size {
        return None;
    }
    let file_offset = ph.offset() as usize;
    if file_offset.checked_add(file_size)? > backing_file_size {
        return None;
    }
    let align = ph.align() as usize;
    if align > 1 && (ph.virtual_addr() as usize % align) != (file_offset % align) {
        return None;
    }
    Some(())
}

fn map_elf_load_segments_lazy(
    memory_set: &mut MemorySet,
    elf: &xmas_elf::ElfFile<'_>,
    backing_file: Arc<dyn File + Send + Sync>,
    backing_file_size: usize,
    load_bias: usize,
) -> Option<usize> {
    let mut max_end_va = 0usize;
    for i in 0..elf.header.pt2.ph_count() {
        let ph = elf.program_header(i).ok()?;
        if ph.get_type().ok()? != Type::Load {
            continue;
        }
        validate_load_segment(ph, backing_file_size)?;
        let mem_size = ph.mem_size() as usize;
        if mem_size == 0 {
            continue;
        }
        let segment_start = load_bias.checked_add(ph.virtual_addr() as usize)?;
        let segment_end = segment_start.checked_add(mem_size)?;
        max_end_va = max_end_va.max(segment_end);

        let map_start = align_down(segment_start, PAGE_SIZE);
        let page_offset = segment_start - map_start;
        let map_len = page_offset.checked_add(mem_size)?;
        let map_file_offset = ph.offset() as usize;
        let exec_segment = ExecSegmentInfo {
            page_offset,
            file_offset: map_file_offset,
            file_size: ph.file_size() as usize,
            mem_size,
        };
        let map_perm = map_permission_from_ph_flags(ph);
        // CONTEXT: LoongArch executable page-cache reuse depends on the fault
        // path syncing a cached frame before it is published as executable.
        // Writable segments stay private because they may need COW or zero-fill.
        let page_cache_id = (!map_perm.contains(MapPermission::W))
            .then(|| backing_file.page_cache_id())
            .flatten();
        memory_set.map_exec_segment_area(
            map_start,
            map_len,
            map_perm,
            backing_file.clone(),
            backing_file_size,
            map_file_offset,
            page_cache_id,
            exec_segment,
        )?;
        record_exec_lazy_segment_vma();
    }
    Some(max_end_va)
}

impl MemorySet {
    pub fn from_elf(
        elf: &xmas_elf::ElfFile<'_>,
        interpreter: Option<&xmas_elf::ElfFile<'_>>,
    ) -> ElfLoadInfo {
        let mut memory_set = Self::new_bare();
        memory_set.map_trampoline();
        let elf_header = elf.header;
        let magic = elf_header.pt1.magic;
        assert_eq!(magic, [0x7f, 0x45, 0x4c, 0x46], "invalid elf!");
        let ph_count = elf_header.pt2.ph_count();
        let ph_entry_size = elf_header.pt2.ph_entry_size();
        let load_bias = main_load_bias(elf);
        let phdr = bias_nonzero_addr(load_bias, phdr_address(elf));
        let max_end_va = map_elf_load_segments(&mut memory_set, elf, load_bias);
        let program_entry = load_bias + elf.header.pt2.entry_point() as usize;
        let mut entry_point = program_entry;
        let mut interp_base = 0usize;
        if let Some(interpreter) = interpreter {
            // CONTEXT: The loader is placed far above the normal executable
            // image. Heap placement must stay based on the main program, so the
            // interpreter's max VA is intentionally not folded into max_end_va.
            let _interp_max_end_va =
                map_elf_load_segments(&mut memory_set, interpreter, DL_INTERP_OFFSET);
            entry_point = DL_INTERP_OFFSET + interpreter.header.pt2.entry_point() as usize;
            interp_base = DL_INTERP_OFFSET;
        }
        let heap_base = page_align_up(max_end_va + PAGE_SIZE);
        let brk_limit = heap_base + USER_HEAP_SIZE;
        memory_set.brk_base = heap_base;
        memory_set.brk = heap_base;
        memory_set.brk_limit = brk_limit;
        memory_set.brk_mapped_end = heap_base;
        memory_set.push(
            MapArea::new(
                heap_base.into(),
                heap_base.into(),
                MapType::Framed,
                MapPermission::R | MapPermission::W | MapPermission::U,
            ),
            None,
        );
        let user_stack_base = brk_limit + PAGE_SIZE;
        memory_set.mmap_next = initial_mmap_next(user_stack_base);
        let sysinfo_ehdr = crate::vdso::map_into(&mut memory_set).unwrap_or(0);
        ElfLoadInfo {
            memory_set,
            ustack_base: user_stack_base,
            entry_point,
            program_entry,
            phdr,
            phent: ph_entry_size as usize,
            phnum: ph_count as usize,
            interp_base,
            sysinfo_ehdr,
        }
    }

    pub fn from_elf_lazy(
        elf: &xmas_elf::ElfFile<'_>,
        backing_file: Arc<dyn File + Send + Sync>,
        backing_file_size: usize,
        interpreter: Option<(&xmas_elf::ElfFile<'_>, Arc<dyn File + Send + Sync>, usize)>,
    ) -> Option<ElfLoadInfo> {
        let mut memory_set = Self::new_bare();
        memory_set.map_trampoline();
        let elf_header = elf.header;
        if elf_header.pt1.magic != [0x7f, 0x45, 0x4c, 0x46] {
            return None;
        }
        let ph_count = elf_header.pt2.ph_count();
        let ph_entry_size = elf_header.pt2.ph_entry_size();
        let load_bias = main_load_bias(elf);
        let phdr = bias_nonzero_addr(load_bias, phdr_address_checked(elf)?);
        let max_end_va = map_elf_load_segments_lazy(
            &mut memory_set,
            elf,
            backing_file,
            backing_file_size,
            load_bias,
        )?;
        let program_entry = load_bias + elf.header.pt2.entry_point() as usize;
        let mut entry_point = program_entry;
        let mut interp_base = 0usize;
        if let Some((interpreter, interpreter_file, interpreter_file_size)) = interpreter {
            let _interp_max_end_va = map_elf_load_segments_lazy(
                &mut memory_set,
                interpreter,
                interpreter_file,
                interpreter_file_size,
                DL_INTERP_OFFSET,
            )?;
            entry_point = DL_INTERP_OFFSET + interpreter.header.pt2.entry_point() as usize;
            interp_base = DL_INTERP_OFFSET;
        }
        let heap_base = page_align_up(max_end_va + PAGE_SIZE);
        let brk_limit = heap_base + USER_HEAP_SIZE;
        memory_set.brk_base = heap_base;
        memory_set.brk = heap_base;
        memory_set.brk_limit = brk_limit;
        memory_set.brk_mapped_end = heap_base;
        memory_set.push(
            MapArea::new(
                heap_base.into(),
                heap_base.into(),
                MapType::Framed,
                MapPermission::R | MapPermission::W | MapPermission::U,
            ),
            None,
        );
        let user_stack_base = brk_limit + PAGE_SIZE;
        memory_set.mmap_next = initial_mmap_next(user_stack_base);
        let sysinfo_ehdr = crate::vdso::map_into(&mut memory_set).unwrap_or(0);
        Some(ElfLoadInfo {
            memory_set,
            ustack_base: user_stack_base,
            entry_point,
            program_entry,
            phdr,
            phent: ph_entry_size as usize,
            phnum: ph_count as usize,
            interp_base,
            sysinfo_ehdr,
        })
    }
}
