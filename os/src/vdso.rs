use crate::mm::MemorySet;

#[cfg(target_arch = "riscv64")]
use crate::config::{PAGE_SIZE, USER_MMAP_LIMIT};

#[cfg(target_arch = "riscv64")]
const VDSO_BASE: usize = USER_MMAP_LIMIT - PAGE_SIZE;

#[cfg(target_arch = "riscv64")]
mod arch {
    use super::*;
    use alloc::vec::Vec;
    use core::arch::global_asm;

    const PT_LOAD: u32 = 1;
    const PT_DYNAMIC: u32 = 2;
    const PF_X: u32 = 1;
    const PF_R: u32 = 4;
    const ET_DYN: u16 = 3;
    const EM_RISCV: u16 = 243;
    const EV_CURRENT: u32 = 1;
    const DT_NULL: u64 = 0;
    const DT_HASH: u64 = 4;
    const DT_STRTAB: u64 = 5;
    const DT_SYMTAB: u64 = 6;
    const DT_STRSZ: u64 = 10;
    const DT_SYMENT: u64 = 11;
    const DT_VERSYM: u64 = 0x6fff_fff0;
    const DT_VERDEF: u64 = 0x6fff_fffc;
    const DT_VERDEFNUM: u64 = 0x6fff_fffd;
    const STB_GLOBAL: u8 = 1;
    const STT_FUNC: u8 = 2;
    const CLOCK_SYMBOL: &[u8] = b"__vdso_clock_gettime";
    const LINUX_4_15: &[u8] = b"LINUX_4.15";

    global_asm!(
        r#"
        .section .rodata.vdso_clock,"a"
        .globl __whusp_vdso_clock_gettime_start
        .globl __whusp_vdso_clock_freq
        .globl __whusp_vdso_clock_gettime_end
        .balign 4
__whusp_vdso_clock_gettime_start:
        .option push
        .option arch, +m
        li t0, 1
        beq a0, t0, 1f
        li t0, 4
        beq a0, t0, 1f
        li t0, 6
        beq a0, t0, 1f
        li t0, 7
        beq a0, t0, 1f
        li a0, -38
        ret
1:
        rdtime t0
.Lwhusp_vdso_freq_pcrel:
        auipc t1, %pcrel_hi(__whusp_vdso_clock_freq)
        addi t1, t1, %pcrel_lo(.Lwhusp_vdso_freq_pcrel)
        ld t1, 0(t1)
        beqz t1, 2f
        divu t2, t0, t1
        remu t3, t0, t1
        li t4, 1000000000
        mul t3, t3, t4
        divu t3, t3, t1
        sd t2, 0(a1)
        sd t3, 8(a1)
        li a0, 0
        ret
2:
        li a0, -38
        ret
        .balign 8
__whusp_vdso_clock_freq:
        .quad 0
        .option pop
__whusp_vdso_clock_gettime_end:
        "#
    );

    unsafe extern "C" {
        static __whusp_vdso_clock_gettime_start: u8;
        static __whusp_vdso_clock_freq: u8;
        static __whusp_vdso_clock_gettime_end: u8;
    }

    pub(super) fn map_into(memory_set: &mut MemorySet) -> Option<usize> {
        let image = build_image()?;
        memory_set
            .map_vdso_image(VDSO_BASE, image.as_slice())
            .then_some(VDSO_BASE)
    }

    fn build_image() -> Option<Vec<u8>> {
        let code = clock_code();
        let freq_offset_in_code = clock_freq_offset()?;
        let phoff = 64usize;
        let phentsize = 56usize;
        let phnum = 2usize;
        let dynamic_off = align_up(phoff + phentsize * phnum, 8);
        let dynamic_count = 9usize;
        let dynamic_size = dynamic_count * 16;
        let hash_off = align_up(dynamic_off + dynamic_size, 8);
        let hash_size = 5 * 4;
        let symtab_off = align_up(hash_off + hash_size, 8);
        let symtab_size = 2 * 24;
        let versym_off = align_up(symtab_off + symtab_size, 2);
        let verdef_off = align_up(versym_off + 2 * 2, 4);
        let verdef_size = 20 + 8;
        let strtab_off = verdef_off + verdef_size;
        let clock_name_off = 1usize;
        let version_name_off = clock_name_off + CLOCK_SYMBOL.len() + 1;
        let strtab_size = version_name_off + LINUX_4_15.len() + 1;
        let code_off = align_up(strtab_off + strtab_size, 16);
        if code_off.checked_add(code.len())? > PAGE_SIZE {
            return None;
        }

        let mut image = Vec::new();
        image.resize(PAGE_SIZE, 0);
        write_elf_header(&mut image, phoff, phentsize, phnum);
        write_program_header(
            &mut image,
            phoff,
            PT_LOAD,
            PF_R | PF_X,
            0,
            0,
            PAGE_SIZE,
            PAGE_SIZE,
            PAGE_SIZE,
        );
        write_program_header(
            &mut image,
            phoff + phentsize,
            PT_DYNAMIC,
            PF_R,
            dynamic_off,
            dynamic_off,
            dynamic_size,
            dynamic_size,
            8,
        );
        write_dynamic_entries(
            &mut image,
            dynamic_off,
            &[
                (DT_HASH, hash_off),
                (DT_STRTAB, strtab_off),
                (DT_SYMTAB, symtab_off),
                (DT_STRSZ, strtab_size),
                (DT_SYMENT, 24),
                (DT_VERSYM, versym_off),
                (DT_VERDEF, verdef_off),
                (DT_VERDEFNUM, 1),
                (DT_NULL, 0),
            ],
        );
        write_sysv_hash(&mut image, hash_off);
        write_symbol(
            &mut image,
            symtab_off + 24,
            clock_name_off,
            code_off,
            code.len(),
        );
        write_u16(&mut image, versym_off, 0);
        write_u16(&mut image, versym_off + 2, 2);
        write_version_def(&mut image, verdef_off, version_name_off);
        image[strtab_off] = 0;
        copy_cstr(&mut image, strtab_off + clock_name_off, CLOCK_SYMBOL);
        copy_cstr(&mut image, strtab_off + version_name_off, LINUX_4_15);
        image[code_off..code_off + code.len()].copy_from_slice(code);
        let freq_offset = code_off.checked_add(freq_offset_in_code)?;
        write_u64(&mut image, freq_offset, crate::config::clock_freq() as u64);
        Some(image)
    }

    fn clock_code() -> &'static [u8] {
        unsafe {
            let start = &__whusp_vdso_clock_gettime_start as *const u8 as usize;
            let end = &__whusp_vdso_clock_gettime_end as *const u8 as usize;
            core::slice::from_raw_parts(start as *const u8, end - start)
        }
    }

    fn clock_freq_offset() -> Option<usize> {
        unsafe {
            let start = &__whusp_vdso_clock_gettime_start as *const u8 as usize;
            let freq = &__whusp_vdso_clock_freq as *const u8 as usize;
            freq.checked_sub(start)
        }
    }

    fn align_up(value: usize, align: usize) -> usize {
        (value + align - 1) & !(align - 1)
    }

    fn write_elf_header(image: &mut [u8], phoff: usize, phentsize: usize, phnum: usize) {
        image[0..4].copy_from_slice(b"\x7fELF");
        image[4] = 2;
        image[5] = 1;
        image[6] = 1;
        write_u16(image, 16, ET_DYN);
        write_u16(image, 18, EM_RISCV);
        write_u32(image, 20, EV_CURRENT);
        write_u64(image, 32, phoff as u64);
        write_u16(image, 52, 64);
        write_u16(image, 54, phentsize as u16);
        write_u16(image, 56, phnum as u16);
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "ELF program headers are positional records"
    )]
    fn write_program_header(
        image: &mut [u8],
        off: usize,
        p_type: u32,
        flags: u32,
        offset: usize,
        vaddr: usize,
        filesz: usize,
        memsz: usize,
        align: usize,
    ) {
        write_u32(image, off, p_type);
        write_u32(image, off + 4, flags);
        write_u64(image, off + 8, offset as u64);
        write_u64(image, off + 16, vaddr as u64);
        write_u64(image, off + 32, filesz as u64);
        write_u64(image, off + 40, memsz as u64);
        write_u64(image, off + 48, align as u64);
    }

    fn write_dynamic_entries(image: &mut [u8], mut off: usize, entries: &[(u64, usize)]) {
        for &(tag, value) in entries {
            write_u64(image, off, tag);
            write_u64(image, off + 8, value as u64);
            off += 16;
        }
    }

    fn write_sysv_hash(image: &mut [u8], off: usize) {
        write_u32(image, off, 1);
        write_u32(image, off + 4, 2);
        write_u32(image, off + 8, 1);
        write_u32(image, off + 12, 0);
        write_u32(image, off + 16, 0);
    }

    fn write_symbol(image: &mut [u8], off: usize, name: usize, value: usize, size: usize) {
        write_u32(image, off, name as u32);
        image[off + 4] = (STB_GLOBAL << 4) | STT_FUNC;
        image[off + 5] = 0;
        write_u16(image, off + 6, 1);
        write_u64(image, off + 8, value as u64);
        write_u64(image, off + 16, size as u64);
    }

    fn write_version_def(image: &mut [u8], off: usize, name: usize) {
        write_u16(image, off, 1);
        write_u16(image, off + 2, 0);
        write_u16(image, off + 4, 2);
        write_u16(image, off + 6, 1);
        write_u32(image, off + 8, elf_hash(LINUX_4_15));
        write_u32(image, off + 12, 20);
        write_u32(image, off + 16, 0);
        write_u32(image, off + 20, name as u32);
        write_u32(image, off + 24, 0);
    }

    fn copy_cstr(image: &mut [u8], off: usize, value: &[u8]) {
        image[off..off + value.len()].copy_from_slice(value);
        image[off + value.len()] = 0;
    }

    fn elf_hash(name: &[u8]) -> u32 {
        let mut h = 0u32;
        for &byte in name {
            h = (h << 4).wrapping_add(byte as u32);
            let g = h & 0xf000_0000;
            if g != 0 {
                h ^= g >> 24;
            }
            h &= !g;
        }
        h
    }

    fn write_u16(image: &mut [u8], off: usize, value: u16) {
        image[off..off + 2].copy_from_slice(&value.to_le_bytes());
    }

    fn write_u32(image: &mut [u8], off: usize, value: u32) {
        image[off..off + 4].copy_from_slice(&value.to_le_bytes());
    }

    fn write_u64(image: &mut [u8], off: usize, value: u64) {
        image[off..off + 8].copy_from_slice(&value.to_le_bytes());
    }
}

#[cfg(not(target_arch = "riscv64"))]
mod arch {
    use super::*;

    pub(super) fn map_into(_memory_set: &mut MemorySet) -> Option<usize> {
        None
    }
}

pub(crate) fn map_into(memory_set: &mut MemorySet) -> Option<usize> {
    arch::map_into(memory_set)
}
