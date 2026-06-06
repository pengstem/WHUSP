use crate::arch::interrupt;
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
    let boot_stack_lower_bound;
    let boot_stack_top;
    unsafe {
        asm!(
            "la {bottom},boot_stack_lower_bound",
            "la {top},boot_stack_top",
            bottom = out(reg) boot_stack_lower_bound,
            top = out(reg) boot_stack_top,
        );
    }
    (boot_stack_lower_bound, boot_stack_top)
}
