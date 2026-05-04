#![no_std]
#![no_main]
#![feature(alloc_error_handler)]

use crate::drivers::{KEYBOARD_DEVICE, MOUSE_DEVICE};
extern crate alloc;

#[macro_use]
extern crate bitflags;

use log::*;

#[macro_use]
mod console;
mod arch;
mod config;
mod drivers;
mod fs;
mod lang_items;
mod logging;
mod mm;
mod sync;
mod syscall;
mod task;

pub(crate) use arch::{board, sbi, timer, trap};

use crate::drivers::chardev::CharDevice;
use crate::drivers::chardev::UART;
use core::sync::atomic::{AtomicUsize, Ordering};

fn clear_bss() {
    unsafe extern "C" {
        safe fn sbss();
        safe fn ebss();
    }
    unsafe {
        core::slice::from_raw_parts_mut(sbss as usize as *mut u8, ebss as usize - sbss as usize)
            .fill(0);
    }
}

use lazy_static::*;
use sync::UPIntrFreeCell;

static BOOT_HART_ID: AtomicUsize = AtomicUsize::new(0);
static DTB_ADDR: AtomicUsize = AtomicUsize::new(0);

lazy_static! {
    pub static ref DEV_NON_BLOCKING_ACCESS: UPIntrFreeCell<bool> =
        unsafe { UPIntrFreeCell::new(false) };
}

#[unsafe(no_mangle)]
pub extern "C" fn rust_main(hart_id: usize, dtb_addr: usize) -> ! {
    clear_bss();
    BOOT_HART_ID.store(hart_id, Ordering::Relaxed);
    DTB_ADDR.store(dtb_addr, Ordering::Relaxed);
    board::init_from_dtb(dtb_addr);
    mm::init();
    timer::init_wall_clock();
    UART.init();
    logging::init();
    info!("boot hart_id={hart_id}, dtb_addr={dtb_addr:#x}");
    info!(
        "board config: clock_freq={}, memory_end={:#x}, uart={:#x}, plic={:#x}",
        board::clock_freq(),
        board::memory_end(),
        board::uart_base(),
        board::plic_base(),
    );

    // TODO: we could remove these devices
    if board::gpu_device().is_some() {
        info!("KERN: init gpu");
    } else {
        info!("KERN: gpu device unavailable");
    }

    if let Some(_keyboard) = KEYBOARD_DEVICE.as_ref() {
        info!("KERN: init keyboard");
    } else {
        info!("KERN: keyboard device unavailable");
    }

    if let Some(_mouse) = MOUSE_DEVICE.as_ref() {
        info!("KERN: init mouse");
    } else {
        info!("KERN: mouse device unavailable");
    }

    info!("KERN: init trap");
    trap::init();
    trap::enable_timer_interrupt();
    timer::set_next_trigger();
    board::device_init(hart_id);
    fs::init();
    fs::list_apps();
    task::add_initproc();
    // CONTEXT: Keep contest block I/O synchronous by default. Runtime DSO
    // loading can fault in file-backed pages after init, and the current
    // nonblocking VirtIO/Condvar path can leave that read asleep until the
    // test harness kills the process.
    *DEV_NON_BLOCKING_ACCESS.exclusive_access() = false;
    task::run_tasks();
    panic!("Unreachable in rust_main!");
}
