use crate::arch::loongarch64::mm::phys_to_virt;

const QEMU_GED_POWEROFF: usize = 0x100e_001c;

pub fn set_timer(timer: usize) {
    let now = loongArch64::time::Time::read();
    let delta = timer.saturating_sub(now).max(4) & !0b11;
    loongArch64::register::tcfg::set_periodic(false);
    loongArch64::register::tcfg::set_init_val(delta);
    loongArch64::register::tcfg::set_en(true);
}

pub fn shutdown(_failure: bool) -> ! {
    let poweroff = phys_to_virt(QEMU_GED_POWEROFF) as *mut u8;
    unsafe {
        poweroff.write_volatile(0x34);
    }
    loop {
        unsafe {
            core::arch::asm!("idle 0");
        }
    }
}
