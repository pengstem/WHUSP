use crate::arch::interrupt;
use core::arch::asm;

pub fn enable_interrupt_and_wait() {
    interrupt::enable_supervisor_interrupt();
    unsafe {
        asm!("idle 0");
    }
}

pub fn boot_stack_bounds() -> (usize, usize) {
    let boot_stack_lower_bound;
    let boot_stack_top;
    unsafe {
        asm!(
            "la.global {bottom}, boot_stack_lower_bound",
            "la.global {top}, boot_stack_top",
            bottom = out(reg) boot_stack_lower_bound,
            top = out(reg) boot_stack_top,
        );
    }
    (boot_stack_lower_bound, boot_stack_top)
}
