use crate::arch::interrupt;
use crate::config::{BOOT_STACK_SIZE, MAX_CPUS};
use core::arch::asm;

pub fn enable_interrupt_and_wait() {
    interrupt::enable_supervisor_interrupt();
    wait_for_interrupt();
}

fn wait_for_interrupt() {
    unsafe {
        asm!("wfi");
    }
}

pub fn boot_stack_bounds() -> (usize, usize) {
    boot_stack_bounds_for(0)
}

pub fn boot_stack_bounds_for(logical_id: usize) -> (usize, usize) {
    assert!(logical_id < MAX_CPUS, "boot stack CPU exceeds MAX_CPUS");
    let boot_stack_lower_bound: usize;
    unsafe {
        asm!(
            "la {bottom},boot_stack_lower_bound",
            bottom = out(reg) boot_stack_lower_bound,
        );
    }
    let bottom = boot_stack_lower_bound + logical_id * BOOT_STACK_SIZE;
    (bottom, bottom + BOOT_STACK_SIZE)
}
