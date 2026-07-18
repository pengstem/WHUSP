use crate::cpu::CpuId;
use core::arch::asm;

const HART_STOPPED: usize = 1;

pub fn validate_startup_extensions() -> Result<(), &'static str> {
    if !crate::sbi::hsm_available() {
        return Err("SBI HSM extension is unavailable");
    }
    if !crate::sbi::ipi_available() {
        return Err("SBI IPI extension is unavailable");
    }
    Ok(())
}

pub fn start_secondary(logical_id: CpuId, hardware_id: usize) -> Result<(), usize> {
    let status = crate::sbi::hart_status(hardware_id)?;
    if status != HART_STOPPED {
        return Err(status);
    }
    unsafe extern "C" {
        safe fn secondary_entry();
    }
    crate::sbi::start_hart(hardware_id, secondary_entry as usize, logical_id)
}

pub fn send_ipi(logical_id: CpuId) -> Result<(), usize> {
    let hardware_id = crate::cpu::topology().hardware_id(logical_id);
    crate::sbi::send_ipi(hardware_id)
}

pub fn enable_local_ipi() {
    unsafe {
        asm!("csrci sip, 2", options(nomem, nostack));
        riscv::register::sie::set_ssoft();
    }
}

pub fn clear_local_ipi() {
    unsafe {
        asm!("csrci sip, 2", options(nomem, nostack));
    }
}

pub fn park_without_interrupts() -> ! {
    loop {
        unsafe {
            asm!("wfi");
        }
    }
}
