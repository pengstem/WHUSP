use crate::arch::loongarch64::mm::phys_to_virt;

const QEMU_GED_POWEROFF: usize = 0x100e_001c; // QEMU virt GED poweroff register.
#[cfg(feature = "loongarch-board-2k1000")]
const LS2K1000_RESET_CONTROL: usize = 0x1fe2_7030;
#[cfg(feature = "loongarch-board-2k1000")]
const LS2K1000_OS_RESET: u32 = 1 << 0;
#[cfg(feature = "loongarch-board-2k1000")]
const LOONGARCH_DMW0_UNCACHED: usize = 0x8000_0000_0000_0000;

pub fn set_timer(timer: usize) {
    let now = loongArch64::time::Time::read();
    let delta = timer.saturating_sub(now).max(4) & !0b11;
    loongArch64::register::tcfg::set_periodic(false);
    loongArch64::register::tcfg::set_init_val(delta);
    loongArch64::register::tcfg::set_en(true);
}

pub fn cancel_timer() {
    loongArch64::register::tcfg::set_en(false);
    loongArch64::register::ticlr::clear_timer_interrupt();
}

#[cfg(feature = "loongarch-board-2k1000")]
fn reset_ls2k1000() -> ! {
    let reset_control = (LOONGARCH_DMW0_UNCACHED | LS2K1000_RESET_CONTROL) as *mut u32;
    unsafe {
        reset_control.write_volatile(LS2K1000_OS_RESET);
    }
    loop {
        unsafe {
            core::arch::asm!("idle 0");
        }
    }
}

pub fn shutdown(_failure: bool) -> ! {
    #[cfg(feature = "loongarch-board-2k1000")]
    {
        // The physical board exposes reset but no independent poweroff path.
        // Returning to U-Boot is safer than touching QEMU's nonexistent GED.
        reset_ls2k1000()
    }

    #[cfg(not(feature = "loongarch-board-2k1000"))]
    {
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
}

pub fn reboot() -> ! {
    #[cfg(feature = "loongarch-board-2k1000")]
    {
        reset_ls2k1000()
    }
    #[cfg(not(feature = "loongarch-board-2k1000"))]
    {
        // UNFINISHED: The current LoongArch board backend exposes only the QEMU
        // GED poweroff register. Under the contest QEMU `-no-reboot` contract this
        // still terminates the run.
        shutdown(false)
    }
}
