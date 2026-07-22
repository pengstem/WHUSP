use crate::task::try_current_kstack_bounds;
use core::panic::PanicInfo;

const MAX_BACKTRACE_FRAMES: usize = 10;

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    let leader = crate::shutdown::begin(true);
    if let Some(location) = info.location() {
        crate::console::emergency_print(format_args!(
            "[kernel] Panicked at {}:{} {}\n",
            location.file(),
            location.line(),
            info.message()
        ));
    } else {
        crate::console::emergency_print(format_args!("[kernel] Panicked: {}\n", info.message()));
    }
    backtrace();
    crate::shutdown::complete(leader)
}

fn backtrace() {
    let mut fp = crate::arch::backtrace::frame_pointer();
    let Some((stack_bottom, stack_top)) = try_current_kstack_bounds() else {
        crate::console::emergency_print(format_args!(
            "backtrace unavailable: processor lock busy\n"
        ));
        return;
    };
    crate::console::emergency_print(format_args!("---START BACKTRACE---\n"));
    if fp == 0 {
        crate::console::emergency_print(format_args!(
            "backtrace unavailable: frame pointer is zero\n---END   BACKTRACE---\n"
        ));
        return;
    }
    for i in 0..MAX_BACKTRACE_FRAMES {
        if !frame_pointer_in_current_stack(fp, stack_bottom, stack_top) {
            break;
        }
        unsafe {
            crate::console::emergency_print(format_args!(
                "#{}:ra={:#x}\n",
                i,
                *((fp - 8) as *const usize)
            ));
            let next_fp = *((fp - 16) as *const usize);
            if next_fp <= fp {
                break;
            }
            fp = next_fp;
        }
    }
    crate::console::emergency_print(format_args!("---END   BACKTRACE---\n"));
}

fn frame_pointer_in_current_stack(fp: usize, stack_bottom: usize, stack_top: usize) -> bool {
    fp >= stack_bottom.saturating_add(16)
        && fp <= stack_top
        && fp.checked_sub(16).is_some_and(|slot| slot >= stack_bottom)
        && fp % core::mem::align_of::<usize>() == 0
}
