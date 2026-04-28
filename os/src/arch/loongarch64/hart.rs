use crate::arch::interrupt;
use core::arch::asm;

pub fn enable_interrupt_and_wait() {
    interrupt::enable_supervisor_interrupt();
    unsafe {
        asm!("idle 0");
    }
}

pub fn boot_stack_top() -> usize {
    let boot_stack_top;
    unsafe {
        asm!("la.global {}, boot_stack_top", out(reg) boot_stack_top);
    }
    boot_stack_top
}
