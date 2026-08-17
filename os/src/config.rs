// Static contest layout limits. Changing these affects loader stack layout,
// mmap placement, trampoline/trap-context spacing, and address-space tests.
pub const USER_STACK_SIZE: usize = 4096 * 1024;
pub const USER_HEAP_SIZE: usize = 0x20_0000;
#[cfg(target_arch = "riscv64")]
pub const USER_MMAP_BASE: usize = 0x8_0000_0000;
#[cfg(not(target_arch = "riscv64"))]
pub const USER_MMAP_BASE: usize = 0x6000_0000;
pub const USER_MMAP_LIMIT: usize = 0x20_0000_0000;
pub const DL_INTERP_OFFSET: usize = 0x30_0000_0000;
pub const KERNEL_STACK_SIZE: usize = 64 * 1024;

/// Maximum number of logical CPUs supported by the current contest machine.
/// Keep the QEMU SMP guards in the root/kernel Makefiles, entry.asm boot
/// stacks, and host-side SMP tools synchronized with this value.
pub const MAX_CPUS: usize = 12;
pub const BOOT_STACK_SIZE: usize = 4096 * 16;

// The 2K1000LA board has 1 GiB of RAM split across two physical banks. Keep
// enough space for userspace and the page cache instead of reserving half of
// the machine for the kernel heap as the larger contest QEMU configuration
// does.
#[cfg(all(target_arch = "loongarch64", feature = "loongarch-board-2k1000"))]
pub const KERNEL_HEAP_SIZE: usize = 128 * 1024 * 1024;
#[cfg(not(all(target_arch = "loongarch64", feature = "loongarch-board-2k1000")))]
pub const KERNEL_HEAP_SIZE: usize = 512 * 1024 * 1024;

// U-Boot places the board DTB at this cached DMW address before bootelf. QEMU
// retains its existing fixed DTB address when the board feature is disabled.
#[cfg(all(target_arch = "loongarch64", feature = "loongarch-board-2k1000"))]
#[used]
#[unsafe(no_mangle)]
pub static LOONGARCH_BOOT_DTB_ADDRESS: usize = 0x9000_0000_0a00_0000;
#[cfg(all(target_arch = "loongarch64", not(feature = "loongarch-board-2k1000")))]
#[used]
#[unsafe(no_mangle)]
pub static LOONGARCH_BOOT_DTB_ADDRESS: usize = 0x9000_0000_0010_0000;

pub const PAGE_SIZE: usize = 0x1000;
pub const PAGE_SIZE_BITS: usize = 0xc;

pub const TRAMPOLINE: usize = usize::MAX - PAGE_SIZE + 1;
pub const TRAP_CONTEXT_BASE: usize = TRAMPOLINE - PAGE_SIZE;
// Keep shared kernel stacks out of the per-process trampoline/trap-context
// root entry. Sv39 root entry 510 covers [0xffff_ffff_8000_0000,
// 0xffff_ffff_c000_0000), while the process-private trampoline lives in 511.
#[cfg(target_arch = "riscv64")]
pub const KERNEL_STACK_TOP: usize = 0xffff_ffff_c000_0000;
#[cfg(not(target_arch = "riscv64"))]
pub const KERNEL_STACK_TOP: usize = TRAMPOLINE;

pub fn clock_freq() -> usize {
    crate::board::clock_freq()
}

pub fn memory_end() -> usize {
    crate::board::memory_end()
}

pub fn mmio_regions() -> &'static [crate::board::MmioRange] {
    crate::board::mmio_regions()
}
