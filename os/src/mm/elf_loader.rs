use super::address::page_align_up;
use super::{MapArea, MapPermission, MapType, MemorySet, VirtAddr};
use crate::config::{DL_INTERP_OFFSET, PAGE_SIZE, USER_HEAP_SIZE, USER_MMAP_BASE};
use alloc::string::{String, ToString};
use core::str;
use xmas_elf::dynamic::Tag;
use xmas_elf::program::{SegmentData, Type};

pub struct ElfLoadInfo {
    pub memory_set: MemorySet,
    pub ustack_base: usize,
    pub entry_point: usize,
    pub aux_entry: usize,
    pub phdr: usize,
    pub phent: usize,
    pub phnum: usize,
    pub interp_base: usize,
}

pub fn elf_interpreter_path(elf_data: &[u8]) -> Option<String> {
    let elf = xmas_elf::ElfFile::new(elf_data).ok()?;
    for i in 0..elf.header.pt2.ph_count() {
        let ph = elf.program_header(i).ok()?;
        if ph.get_type().ok()? != Type::Interp {
            continue;
        }
        let start = ph.offset() as usize;
        let len = ph.file_size() as usize;
        let bytes = elf_data.get(start..start.checked_add(len)?)?;
        let end = bytes
            .iter()
            .position(|byte| *byte == 0)
            .unwrap_or(bytes.len());
        return str::from_utf8(&bytes[..end]).ok().map(ToString::to_string);
    }
    None
}

pub fn elf_needs_interpreter(elf_data: &[u8]) -> bool {
    let Ok(elf) = xmas_elf::ElfFile::new(elf_data) else {
        return false;
    };
    for i in 0..elf.header.pt2.ph_count() {
        let Ok(ph) = elf.program_header(i) else {
            continue;
        };
        if ph.get_type().ok() != Some(Type::Dynamic) {
            continue;
        }
        let Ok(data) = ph.get_data(&elf) else {
            continue;
        };
        match data {
            SegmentData::Dynamic32(entries) => {
                if entries
                    .iter()
                    .any(|entry| entry.get_tag().ok() == Some(Tag::Needed))
                {
                    return true;
                }
            }
            SegmentData::Dynamic64(entries) => {
                if entries
                    .iter()
                    .any(|entry| entry.get_tag().ok() == Some(Tag::Needed))
                {
                    return true;
                }
            }
            _ => {}
        }
    }
    false
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

fn map_elf_load_segments(memory_set: &mut MemorySet, elf_data: &[u8], load_bias: usize) -> usize {
    let elf = xmas_elf::ElfFile::new(elf_data).unwrap();
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
            Some(&elf_data[ph.offset() as usize..(ph.offset() + ph.file_size()) as usize]),
            start_va.page_offset(),
        );
    }
    max_end_va
}

impl MemorySet {
    pub fn from_elf(elf_data: &[u8], interpreter_data: Option<&[u8]>) -> ElfLoadInfo {
        let mut memory_set = Self::new_bare();
        memory_set.map_trampoline();
        let elf = xmas_elf::ElfFile::new(elf_data).unwrap();
        let elf_header = elf.header;
        let magic = elf_header.pt1.magic;
        assert_eq!(magic, [0x7f, 0x45, 0x4c, 0x46], "invalid elf!");
        let ph_count = elf_header.pt2.ph_count();
        let ph_entry_size = elf_header.pt2.ph_entry_size();
        let phdr = phdr_address(&elf);
        let max_end_va = map_elf_load_segments(&mut memory_set, elf_data, 0);
        let aux_entry = elf.header.pt2.entry_point() as usize;
        let mut entry_point = aux_entry;
        let mut interp_base = 0usize;
        if let Some(interpreter_data) = interpreter_data {
            let interp_elf = xmas_elf::ElfFile::new(interpreter_data).unwrap();
            map_elf_load_segments(&mut memory_set, interpreter_data, DL_INTERP_OFFSET);
            entry_point = DL_INTERP_OFFSET + interp_elf.header.pt2.entry_point() as usize;
            interp_base = DL_INTERP_OFFSET;
        }
        let heap_base = page_align_up(max_end_va + PAGE_SIZE);
        let brk_limit = heap_base + USER_HEAP_SIZE;
        memory_set.brk_base = heap_base;
        memory_set.brk = heap_base;
        memory_set.brk_limit = brk_limit;
        memory_set.brk_mapped_end = heap_base;
        memory_set.mmap_next = USER_MMAP_BASE;
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
        ElfLoadInfo {
            memory_set,
            ustack_base: user_stack_base,
            entry_point,
            aux_entry,
            phdr,
            phent: ph_entry_size as usize,
            phnum: ph_count as usize,
            interp_base,
        }
    }
}
