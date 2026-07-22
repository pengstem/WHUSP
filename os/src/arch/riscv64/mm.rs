use core::arch::asm;
use core::sync::atomic::{AtomicUsize, Ordering};
use riscv::register::satp;

const LOCAL_TLB_RANGE_PAGE_LIMIT: usize = 64;

const SV39_MODE: usize = 8;
const SATP_ASID_SHIFT: usize = 44;
const SATP_ASID_BITS: usize = 16;
const SATP_ASID_MAX: usize = (1usize << SATP_ASID_BITS) - 1;
const SATP_ASID_MASK: usize = SATP_ASID_MAX << SATP_ASID_SHIFT;
const SATP_PPN_MASK: usize = (1usize << 44) - 1;
const PA_WIDTH: usize = 56;
const VA_WIDTH: usize = 39;
const PPN_WIDTH: usize = PA_WIDTH - crate::config::PAGE_SIZE_BITS;
const VPN_WIDTH: usize = VA_WIDTH - crate::config::PAGE_SIZE_BITS;

const ASID_SUPPORT_NO: usize = 0;
const ASID_SUPPORT_YES: usize = 1;
const ASID_SUPPORT_UNKNOWN: usize = 2;

static ASID_SUPPORT: AtomicUsize = AtomicUsize::new(ASID_SUPPORT_UNKNOWN);

pub fn page_table_token_with_asid(root_ppn: usize, asid: usize) -> usize {
    SV39_MODE << 60 | ((asid & SATP_ASID_MAX) << SATP_ASID_SHIFT) | (root_ppn & SATP_PPN_MASK)
}

pub fn page_table_root_ppn(token: usize) -> usize {
    token & SATP_PPN_MASK
}

pub fn page_table_asid(token: usize) -> usize {
    (token & SATP_ASID_MASK) >> SATP_ASID_SHIFT
}

pub fn alloc_page_table_asid() -> usize {
    // CONTEXT: Keep ASID 0 until Phase 5 adds a global ASID generation and
    // acknowledged all-CPU rollover. Reusing a tag after only a local fence
    // would permit a remote CPU to retain a stale translation.
    0
}

pub fn activate_page_table(token: usize) {
    satp::write(token);
    flush_tlb_all();
}

pub fn flush_tlb_all() {
    mark_return_tlb_dirty();
    unsafe {
        asm!("sfence.vma");
    }
}

pub fn flush_tlb_page(va: usize) {
    mark_return_tlb_dirty();
    unsafe {
        asm!("sfence.vma {va}, x0", va = in(reg) va);
    }
}

pub fn flush_tlb_range(start: usize, size: usize) {
    mark_return_tlb_dirty();
    if size == usize::MAX
        || (start == 0 && size == 0)
        || size / crate::config::PAGE_SIZE > LOCAL_TLB_RANGE_PAGE_LIMIT
    {
        unsafe {
            asm!("sfence.vma");
        }
        return;
    }
    for offset in (0..size).step_by(crate::config::PAGE_SIZE) {
        let va = start.checked_add(offset).expect("TLB flush range overflow");
        unsafe {
            asm!("sfence.vma {va}, x0", va = in(reg) va);
        }
    }
}

pub fn should_flush_tlb_on_return(user_token: usize) -> bool {
    if !asid_supported() {
        return true;
    }
    let state = crate::cpu::current().mmu();
    let previous = state.swap_last_return_user_token(user_token);
    let dirty = state.take_return_tlb_dirty();
    previous != user_token || dirty
}

pub fn should_flush_tlb_on_kernel_entry(kernel_token: usize) -> bool {
    if !asid_supported() {
        return true;
    }
    let state = crate::cpu::current().mmu();
    let previous = state.swap_last_entry_kernel_token(kernel_token);
    let dirty = state.take_kernel_tlb_dirty();
    previous != kernel_token || dirty
}

pub fn mark_kernel_tlb_dirty() {
    crate::cpu::current().mmu().mark_kernel_tlb_dirty();
}

fn mark_return_tlb_dirty() {
    crate::cpu::current().mmu().mark_return_tlb_dirty();
}

fn asid_supported() -> bool {
    match ASID_SUPPORT.load(Ordering::Relaxed) {
        ASID_SUPPORT_YES => true,
        ASID_SUPPORT_NO => false,
        _ => probe_asid_supported(),
    }
}

fn probe_asid_supported() -> bool {
    let current = read_satp_bits();
    let probe = (current & !SATP_ASID_MASK) | SATP_ASID_MASK;
    write_satp_bits(probe);
    let observed = read_satp_bits();
    write_satp_bits(current);
    unsafe {
        asm!("sfence.vma");
    }
    let supported = observed & SATP_ASID_MASK != 0;
    ASID_SUPPORT.store(
        if supported {
            ASID_SUPPORT_YES
        } else {
            ASID_SUPPORT_NO
        },
        Ordering::Relaxed,
    );
    supported
}

fn read_satp_bits() -> usize {
    let bits: usize;
    unsafe {
        asm!("csrr {bits}, satp", bits = out(reg) bits, options(nomem, nostack));
    }
    bits
}

fn write_satp_bits(bits: usize) {
    unsafe {
        asm!("csrw satp, {bits}", bits = in(reg) bits, options(nomem, nostack));
    }
}

pub fn publish_pte_barrier() {
    unsafe {
        asm!("fence rw, rw");
    }
}

pub fn instruction_barrier() {
    crate::perf::record_arch_instruction_barrier_call();
    unsafe {
        asm!("fence.i");
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

pub fn pte_new_bits(ppn: usize, flags: crate::mm::page_table::PTEFlags) -> usize {
    ppn << 10 | flags.bits()
}

pub fn pte_ppn(bits: usize) -> usize {
    bits >> 10 & ((1usize << PPN_WIDTH) - 1)
}

pub fn pte_flags(bits: usize) -> crate::mm::page_table::PTEFlags {
    crate::mm::page_table::PTEFlags::from_bits_truncate(bits)
}

pub fn pte_is_valid(bits: usize) -> bool {
    pte_flags(bits).contains(crate::mm::page_table::PTEFlags::V)
}
