use crate::sbi::shutdown;
use crate::task::current_kstack_top;
use core::panic::PanicInfo;
use log::*;

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    if let Some(location) = info.location() {
        error!(
            "[kernel] Panicked at {}:{} {}",
            location.file(),
            location.line(),
            info.message()
        );
    } else {
        error!("[kernel] Panicked: {}", info.message());
    }
    backtrace();
    shutdown(true)
}

fn backtrace() {
    let mut fp = crate::arch::backtrace::frame_pointer();
    let stop = current_kstack_top();
    println!("---START BACKTRACE---");
    if fp == 0 {
        println!("backtrace unavailable: frame pointer is zero");
        println!("---END   BACKTRACE---");
        return;
    }
    for i in 0..10 {
        if fp == stop || fp < 16 {
            break;
        }
        unsafe {
            println!("#{}:ra={:#x}", i, *((fp - 8) as *const usize));
            fp = *((fp - 16) as *const usize);
        }
    }
    println!("---END   BACKTRACE---");
}
