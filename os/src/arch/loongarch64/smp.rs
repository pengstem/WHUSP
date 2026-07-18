use crate::cpu::CpuId;
use loongArch64::consts::{
    LOONGARCH_IOCSR_IPI_CLEAR, LOONGARCH_IOCSR_IPI_EN, LOONGARCH_IOCSR_IPI_STATUS,
};
use loongArch64::iocsr::{iocsr_read_w, iocsr_write_w};
use loongArch64::ipi::{csr_mail_send, send_ipi_single};

const BOOT_IPI_ACTION: u32 = 1;
const BOOT_MAILBOX: usize = 0;

pub fn validate_startup_extensions() -> Result<(), &'static str> {
    if !loongArch64::cpu::get_support_iocsr() {
        return Err("LoongArch IOCSR is unavailable");
    }
    Ok(())
}

pub fn start_secondary(_logical_id: CpuId, hardware_id: usize) -> Result<(), usize> {
    unsafe extern "C" {
        safe fn secondary_entry();
    }
    let entry = crate::arch::loongarch64::mm::virt_to_phys(secondary_entry as usize) as u64;
    csr_mail_send(entry, hardware_id, BOOT_MAILBOX);
    send_ipi_single(hardware_id, BOOT_IPI_ACTION);
    Ok(())
}

pub fn send_ipi(logical_id: CpuId) -> Result<(), usize> {
    let hardware_id = crate::cpu::topology().hardware_id(logical_id);
    send_ipi_single(hardware_id, BOOT_IPI_ACTION);
    Ok(())
}

pub fn enable_local_ipi() {
    clear_local_ipi();
    iocsr_write_w(LOONGARCH_IOCSR_IPI_EN, u32::MAX);
}

pub fn clear_local_ipi() {
    let pending = iocsr_read_w(LOONGARCH_IOCSR_IPI_STATUS);
    if pending != 0 {
        iocsr_write_w(LOONGARCH_IOCSR_IPI_CLEAR, pending);
    }
}

pub fn park_without_interrupts() -> ! {
    loop {
        unsafe {
            core::arch::asm!("idle 0");
        }
    }
}
