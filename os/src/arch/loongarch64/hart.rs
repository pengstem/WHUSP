use crate::arch::interrupt;
use crate::config::{BOOT_STACK_SIZE, MAX_CPUS};
use crate::trap::TrapContext;
use core::arch::{asm, global_asm};

global_asm!(include_str!("idle.S"));

pub fn enable_interrupt_and_wait() {
    interrupt::enable_supervisor_interrupt();
    unsafe {
        asm!("idle 0");
    }
}

pub fn wait_for_interrupt_disabled() {
    debug_assert!(!interrupt::supervisor_interrupt_enabled());
    unsafe extern "C" {
        safe fn __whusp_arch_cpu_idle();
    }
    __whusp_arch_cpu_idle();
    interrupt::disable_supervisor_interrupt();
}

pub fn redirect_idle_interrupt(trap_cx: &mut TrapContext) {
    unsafe extern "C" {
        safe static __whusp_idle_enter: u8;
        safe static __whusp_idle_exit: u8;
    }
    let idle_enter = &__whusp_idle_enter as *const u8 as usize;
    let idle_exit = &__whusp_idle_exit as *const u8 as usize;
    if (idle_enter..idle_exit).contains(&trap_cx.era) {
        trap_cx.era = idle_exit;
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
            "la.global {bottom}, boot_stack_lower_bound",
            bottom = out(reg) boot_stack_lower_bound,
        );
    }
    let bottom = boot_stack_lower_bound + logical_id * BOOT_STACK_SIZE;
    (bottom, bottom + BOOT_STACK_SIZE)
}
