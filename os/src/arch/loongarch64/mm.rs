use core::arch::asm;
use loongArch64::register::{pgdh, pgdl};

pub const VIRT_ADDR_START: usize = 0x9000_0000_0000_0000;
const DIRECT_MAP_MASK: usize = 0xf000_0000_0000_0000;
const PHYS_ADDR_MASK: usize = 0x0000_ffff_ffff_ffff;
const PTE_ADDR_MASK: usize = 0x0000_ffff_ffff_f000;
const PA_WIDTH: usize = 48;
const VA_WIDTH: usize = 48;
const PPN_WIDTH: usize = PA_WIDTH - crate::config::PAGE_SIZE_BITS;
const VPN_WIDTH: usize = VA_WIDTH - crate::config::PAGE_SIZE_BITS;

const LA_PTE_V: usize = 1 << 0;
const LA_PTE_D: usize = 1 << 1;
const LA_PTE_PLV_USER: usize = 0b11 << 2;
const LA_PTE_MAT_CC: usize = 0b01 << 4;
const LA_PTE_P: usize = 1 << 7;
const LA_PTE_W: usize = 1 << 8;
const LA_PTE_NR: usize = 1 << 61;
const LA_PTE_NX: usize = 1 << 62;

pub fn page_table_token(root_ppn: usize) -> usize {
    root_ppn << crate::config::PAGE_SIZE_BITS
}

pub fn page_table_root_ppn(token: usize) -> usize {
    token >> crate::config::PAGE_SIZE_BITS
}

pub fn activate_page_table(token: usize) {
    pgdl::set_base(token);
    pgdh::set_base(token);
    flush_tlb_all();
}

pub fn flush_tlb_all() {
    unsafe {
        asm!("dbar 0", "invtlb 0x00, $r0, $r0");
    }
}

pub fn canonicalize_phys_addr(addr: usize) -> usize {
    virt_to_phys(addr) & ((1usize << PA_WIDTH) - 1)
}

pub fn canonicalize_phys_page_num(ppn: usize) -> usize {
    ppn & ((1usize << PPN_WIDTH) - 1)
}

pub fn canonicalize_virt_addr(addr: usize) -> usize {
    addr
}

pub fn canonicalize_virt_page_num(vpn: usize) -> usize {
    vpn & ((1usize << VPN_WIDTH) - 1)
}

pub fn sign_extend_virt_addr(addr: usize) -> usize {
    addr
}

pub fn phys_to_virt(addr: usize) -> usize {
    if addr & DIRECT_MAP_MASK == 0 {
        addr | VIRT_ADDR_START
    } else {
        addr
    }
}

pub fn virt_to_phys(addr: usize) -> usize {
    if addr & DIRECT_MAP_MASK == VIRT_ADDR_START {
        addr & PHYS_ADDR_MASK
    } else {
        addr
    }
}

pub fn pte_new_bits(ppn: usize, flags: crate::mm::page_table::PTEFlags) -> usize {
    let pa = ppn << crate::config::PAGE_SIZE_BITS;
    let leaf_flags = crate::mm::page_table::PTEFlags::R
        | crate::mm::page_table::PTEFlags::W
        | crate::mm::page_table::PTEFlags::X;
    if !flags.intersects(leaf_flags) {
        return pa;
    }

    let mut bits = pa | LA_PTE_V | LA_PTE_P | LA_PTE_MAT_CC;
    if flags.contains(crate::mm::page_table::PTEFlags::W) {
        bits |= LA_PTE_W | LA_PTE_D;
    }
    if !flags.contains(crate::mm::page_table::PTEFlags::R) {
        bits |= LA_PTE_NR;
    }
    if !flags.contains(crate::mm::page_table::PTEFlags::X) {
        bits |= LA_PTE_NX;
    }
    if flags.contains(crate::mm::page_table::PTEFlags::U) {
        bits |= LA_PTE_PLV_USER;
    }
    bits
}

pub fn pte_ppn(bits: usize) -> usize {
    (bits & PTE_ADDR_MASK) >> crate::config::PAGE_SIZE_BITS
}

pub fn pte_flags(bits: usize) -> crate::mm::page_table::PTEFlags {
    let mut flags = crate::mm::page_table::PTEFlags::empty();
    if bits != 0 {
        flags |= crate::mm::page_table::PTEFlags::V;
    }
    if bits & LA_PTE_NR == 0 {
        flags |= crate::mm::page_table::PTEFlags::R;
    }
    if bits & LA_PTE_W != 0 {
        flags |= crate::mm::page_table::PTEFlags::W;
    }
    if bits & LA_PTE_NX == 0 {
        flags |= crate::mm::page_table::PTEFlags::X;
    }
    if bits & LA_PTE_PLV_USER == LA_PTE_PLV_USER {
        flags |= crate::mm::page_table::PTEFlags::U;
    }
    flags
}

pub fn pte_is_valid(bits: usize) -> bool {
    bits != 0
}
