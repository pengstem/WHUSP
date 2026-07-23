use core::arch::asm;

const LOCAL_TLB_RANGE_PAGE_LIMIT: usize = 64;

pub const VIRT_ADDR_START: usize = 0x9000_0000_0000_0000;
const DIRECT_MAP_MASK: usize = 0xf000_0000_0000_0000;
const PHYS_ADDR_MASK: usize = 0x0000_ffff_ffff_ffff;
const PTE_ADDR_MASK: usize = 0x0000_ffff_ffff_f000;
const LA_CSR_PGDL: usize = 0x19;
const LA_CSR_PGDH: usize = 0x1a;
const PA_WIDTH: usize = 48;
const VA_WIDTH: usize = 48;
const PPN_WIDTH: usize = PA_WIDTH - crate::config::PAGE_SIZE_BITS;
const VPN_WIDTH: usize = VA_WIDTH - crate::config::PAGE_SIZE_BITS;

pub const MAX_KERNEL_LEAF_LEVEL: usize = 1;

const LA_PTE_V: usize = 1 << 0;
const LA_PTE_D: usize = 1 << 1;
const LA_PTE_PLV_USER: usize = 0b11 << 2;
const LA_PTE_MAT_CC: usize = 0b01 << 4;
const LA_PTE_HUGE: usize = 1 << 6;
const LA_PTE_P: usize = 1 << 7;
const LA_PTE_W: usize = 1 << 8;
const LA_PTE_COW: usize = 1 << 58;
// LA64 leaf PTEs encode read/execute denial as NR/NX; absence of those bits
// means read or execute permission is allowed.
const LA_PTE_NR: usize = 1 << 61;
const LA_PTE_NX: usize = 1 << 62;

pub fn page_table_token(root_ppn: usize) -> usize {
    root_ppn << crate::config::PAGE_SIZE_BITS
}

pub fn page_table_token_with_asid(root_ppn: usize, _asid: usize) -> usize {
    page_table_token(root_ppn)
}

pub fn page_table_root_ppn(token: usize) -> usize {
    token >> crate::config::PAGE_SIZE_BITS
}

pub fn page_table_asid(_token: usize) -> usize {
    0
}

pub fn alloc_page_table_asid() -> usize {
    0
}

pub fn activate_page_table(token: usize) {
    // CONTEXT: The current LA64 port installs one MemorySet root into both
    // PGDL and PGDH. Splitting low/high roots requires auditing trap.S, the
    // refill entry, and direct-map address translation together.
    write_page_table_roots(token, token);
    flush_tlb_all();
}

fn write_page_table_roots(pgdl_token: usize, pgdh_token: usize) {
    unsafe {
        asm!(
            "csrwr {pgdl}, {pgdl_csr}",
            "csrwr {pgdh}, {pgdh_csr}",
            pgdl = inout(reg) pgdl_token => _,
            pgdh = inout(reg) pgdh_token => _,
            pgdl_csr = const LA_CSR_PGDL,
            pgdh_csr = const LA_CSR_PGDH,
            options(nomem, nostack),
        );
    }
}

pub fn flush_tlb_all() {
    mark_return_tlb_dirty();
    unsafe {
        asm!("dbar 0", "invtlb 0x00, $r0, $r0", "dbar 0");
    }
}

pub fn flush_tlb_range(start: usize, size: usize) {
    mark_return_tlb_dirty();
    unsafe {
        asm!("dbar 0");
    }
    if size == usize::MAX
        || (start == 0 && size == 0)
        || size / crate::config::PAGE_SIZE > LOCAL_TLB_RANGE_PAGE_LIMIT
    {
        unsafe {
            asm!("invtlb 0x00, $r0, $r0");
        }
    } else {
        for offset in (0..size).step_by(crate::config::PAGE_SIZE) {
            let va = start.checked_add(offset).expect("TLB flush range overflow");
            unsafe {
                asm!("invtlb 0x05, $r0, {va}", va = in(reg) va);
            }
        }
    }
    unsafe {
        asm!("dbar 0");
    }
}

pub fn should_flush_tlb_on_return(user_token: usize) -> bool {
    // The current LA64 path has no ASID allocation, so returning to a different
    // page-table root or after any PTE edit must request a guest TLB flush.
    let state = crate::cpu::current().mmu();
    let previous = state.swap_last_return_user_token(user_token);
    let dirty = state.take_return_tlb_dirty();
    previous != user_token || dirty
}

#[allow(dead_code)]
pub fn should_flush_tlb_on_kernel_entry(_kernel_token: usize) -> bool {
    true
}

fn mark_return_tlb_dirty() {
    crate::cpu::current().mmu().mark_return_tlb_dirty();
}

pub fn publish_pte_barrier() {
    memory_barrier();
}

pub fn memory_barrier() {
    unsafe {
        asm!("dbar 0");
    }
}

pub fn instruction_barrier() {
    crate::perf::record_arch_instruction_barrier_call();
    unsafe {
        asm!("ibar 0");
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
    addr | VIRT_ADDR_START
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
    if !flags.intersects(leaf_flags) && !flags.contains(crate::mm::page_table::PTEFlags::U) {
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
    if flags.contains(crate::mm::page_table::PTEFlags::COW) {
        bits |= LA_PTE_COW;
    }
    bits
}

pub fn pte_new_leaf_bits(
    ppn: usize,
    flags: crate::mm::page_table::PTEFlags,
    level: usize,
) -> usize {
    assert!(
        level <= MAX_KERNEL_LEAF_LEVEL,
        "LoongArch leaf level {level} is out of range"
    );
    let bits = pte_new_bits(ppn, flags);
    if level == 1 { bits | LA_PTE_HUGE } else { bits }
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
    if bits & LA_PTE_COW != 0 {
        flags |= crate::mm::page_table::PTEFlags::COW;
    }
    flags
}

pub fn pte_is_valid(bits: usize) -> bool {
    bits != 0
}

pub fn pte_is_leaf(bits: usize) -> bool {
    bits & (LA_PTE_V | LA_PTE_HUGE) != 0
}
