use crate::sbi::shutdown;
use crate::task::current_kstack_bounds;
use core::panic::PanicInfo;
use log::*;

const MAX_BACKTRACE_FRAMES: usize = 10;

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
    let (stack_bottom, stack_top) = current_kstack_bounds();
    println!("---START BACKTRACE---");
    if fp == 0 {
        println!("backtrace unavailable: frame pointer is zero");
        println!("---END   BACKTRACE---");
        return;
    }
    for i in 0..MAX_BACKTRACE_FRAMES {
        if !frame_pointer_in_current_stack(fp, stack_bottom, stack_top) {
            break;
        }
        unsafe {
            println!("#{}:ra={:#x}", i, *((fp - 8) as *const usize));
            let next_fp = *((fp - 16) as *const usize);
            if next_fp <= fp {
                break;
            }
            fp = next_fp;
        }
    }
    println!("---END   BACKTRACE---");
}

fn frame_pointer_in_current_stack(fp: usize, stack_bottom: usize, stack_top: usize) -> bool {
    fp >= stack_bottom.saturating_add(16)
        && fp <= stack_top
        && fp.checked_sub(16).is_some_and(|slot| slot >= stack_bottom)
        && fp % core::mem::align_of::<usize>() == 0
}
