use core::arch::asm;
use riscv::register::satp;

const SV39_MODE: usize = 8;
const SATP_PPN_MASK: usize = (1usize << 44) - 1;
const PA_WIDTH: usize = 56;
const VA_WIDTH: usize = 39;
const PPN_WIDTH: usize = PA_WIDTH - crate::config::PAGE_SIZE_BITS;
const VPN_WIDTH: usize = VA_WIDTH - crate::config::PAGE_SIZE_BITS;

pub fn page_table_token(root_ppn: usize) -> usize {
    SV39_MODE << 60 | root_ppn
}

pub fn page_table_root_ppn(token: usize) -> usize {
    token & SATP_PPN_MASK
}

pub fn activate_page_table(token: usize) {
    satp::write(token);
    flush_tlb_all();
}

pub fn flush_tlb_all() {
    unsafe {
        asm!("sfence.vma");
    }
}

pub fn canonicalize_phys_addr(addr: usize) -> usize {
    addr & ((1usize << PA_WIDTH) - 1)
}

pub fn canonicalize_phys_page_num(ppn: usize) -> usize {
    ppn & ((1usize << PPN_WIDTH) - 1)
}

pub fn canonicalize_virt_addr(addr: usize) -> usize {
    addr & ((1usize << VA_WIDTH) - 1)
}

pub fn canonicalize_virt_page_num(vpn: usize) -> usize {
    vpn & ((1usize << VPN_WIDTH) - 1)
}

pub fn sign_extend_virt_addr(addr: usize) -> usize {
    if addr >= (1usize << (VA_WIDTH - 1)) {
        addr | (!((1usize << VA_WIDTH) - 1))
    } else {
        addr
    }
}

pub fn phys_to_virt(addr: usize) -> usize {
    addr
}

pub fn virt_to_phys(addr: usize) -> usize {
    addr
}

pub fn pte_new_bits(ppn: usize, flags: crate::mm::page_table::PTEFlags) -> usize {
    ppn << 10 | flags.bits() as usize
}

pub fn pte_ppn(bits: usize) -> usize {
    bits >> 10 & ((1usize << PPN_WIDTH) - 1)
}

pub fn pte_flags(bits: usize) -> crate::mm::page_table::PTEFlags {
    crate::mm::page_table::PTEFlags::from_bits_truncate(bits as u8)
}

pub fn pte_is_valid(bits: usize) -> bool {
    pte_flags(bits).contains(crate::mm::page_table::PTEFlags::V)
}
