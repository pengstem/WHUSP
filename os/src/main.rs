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
mod cpu;
mod drivers;
mod fs;
mod lang_items;
mod logging;
mod mm;
mod perf;
mod shutdown;
mod sync;
mod syscall;
mod task;
mod vdso;

pub(crate) use arch::{board, sbi, timer, trap};

use crate::drivers::chardev::CharDevice;
use crate::drivers::chardev::UART;
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

lazy_static! {
    pub static ref DEV_NON_BLOCKING_ACCESS: UPIntrFreeCell<bool> =
        unsafe { UPIntrFreeCell::new(false) };
}

#[unsafe(no_mangle)]
pub extern "C" fn rust_main(hart_id: usize, dtb_addr: usize) -> ! {
    clear_bss();
    cpu::record_boot_entry();
    cpu::record_global_init();
    board::init_from_dtb(dtb_addr, hart_id);
    cpu::install_current(0, hart_id);
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
    let topology = cpu::topology();
    let online = cpu::online_mask();
    info!(
        "cpu topology: possible={} online={} possible_mask={:#x} online_mask={:#x} boot_logical=0 boot_hw_id={} hw_ids={:?}",
        topology.possible_count(),
        online.count(),
        topology.possible_mask().bits(),
        online.bits(),
        topology.boot_hw_id(),
        topology.hardware_ids(),
    );
    info!(
        "smp invariants: boot_entries={} global_init_entries={}",
        cpu::boot_entry_count(),
        cpu::global_init_count(),
    );
    let kernel_mapping = mm::kernel_mapping_stats();
    info!(
        "kernel mapping: elapsed_us={} page_table_frames={} leaves_4k={} leaves_2m={} leaves_1g={}",
        kernel_mapping.elapsed_us,
        kernel_mapping.page_table_frames,
        kernel_mapping.leaves_4k,
        kernel_mapping.leaves_2m,
        kernel_mapping.leaves_1g,
    );

    // CONTEXT: Headless contest QEMU may omit these optional devices, but
    // smoke checks key on the unavailable-device log lines below.
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
    cpu::start_parked_secondaries();
    cpu::activate_scheduler_aps();
    // CONTEXT: Task-context block I/O can use the nonblocking path only after
    // the active architecture has wired device IRQ completion. The driver still
    // falls back to sync I/O when a read happens from an unsafe context such as
    // interrupt-disabled lazy fault-in.
    *DEV_NON_BLOCKING_ACCESS.exclusive_access() = board::block_irq_available();
    task::run_tasks()
}

#[unsafe(no_mangle)]
pub extern "C" fn rust_secondary_main(hardware_id: usize, logical_id: usize) -> ! {
    if !cpu::secondary_mark_early(hardware_id, logical_id) {
        crate::arch::smp::park_without_interrupts();
    }
    cpu::install_current(logical_id, hardware_id);
    mm::activate_kernel_page_table_for_secondary();
    trap::init();
    crate::arch::smp::enable_local_ipi();
    trap::enable_timer_interrupt();
    timer::set_next_trigger();
    crate::arch::interrupt::enable_supervisor_interrupt();
    cpu::secondary_publish_online(logical_id);
    while !cpu::scheduler_aps_active() {
        cpu::run_pending_parked_probe(logical_id);
        crate::arch::hart::enable_interrupt_and_wait();
    }
    task::run_tasks()
}
