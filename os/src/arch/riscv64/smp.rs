use crate::cpu::{CpuId, CpuMask};
use core::arch::asm;

const HART_STOPPED: usize = 1;

pub fn validate_startup_extensions() -> Result<(), &'static str> {
    if !crate::sbi::hsm_available() {
        return Err("SBI HSM extension is unavailable");
    }
    if !crate::sbi::ipi_available() {
        return Err("SBI IPI extension is unavailable");
    }
    if !crate::sbi::opensbi_rfence_completion_available() {
        return Err("synchronous OpenSBI RFENCE is unavailable");
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

pub fn remote_tlb_flush(targets: CpuMask, start: usize, size: usize) -> Result<(), usize> {
    assert_eq!(
        targets.bits() & !crate::cpu::online_mask().bits(),
        0,
        "SBI RFENCE targets an offline CPU"
    );
    assert!(
        !targets.contains(crate::cpu::current_id()),
        "SBI RFENCE target mask contains the caller"
    );
    assert_eq!(
        start % crate::config::PAGE_SIZE,
        0,
        "SBI RFENCE start is not page aligned"
    );
    assert!(
        size == usize::MAX || size % crate::config::PAGE_SIZE == 0,
        "SBI RFENCE size is not page aligned"
    );
    assert!(
        size != 0 || start == 0,
        "zero-size SBI RFENCE must select the full address space"
    );
    crate::arch::mm::publish_pte_barrier();
    let topology = crate::cpu::topology();
    let mut pending = targets.bits();
    while pending != 0 {
        let mut hart_mask_base = usize::MAX;
        for logical_id in 0..topology.possible_count() {
            if pending & (1u64 << logical_id) != 0 {
                hart_mask_base = hart_mask_base.min(topology.hardware_id(logical_id));
            }
        }
        assert_ne!(
            hart_mask_base,
            usize::MAX,
            "nonempty logical CPU mask had no hardware CPU"
        );

        let mut hart_mask = 0usize;
        let mut covered = 0u64;
        for logical_id in 0..topology.possible_count() {
            let logical_bit = 1u64 << logical_id;
            if pending & logical_bit == 0 {
                continue;
            }
            let hardware_id = topology.hardware_id(logical_id);
            if let Some(bit) = hardware_id.checked_sub(hart_mask_base)
                && bit < usize::BITS as usize
            {
                hart_mask |= 1usize << bit;
                covered |= logical_bit;
            }
        }
        assert_ne!(covered, 0, "failed to encode SBI RFENCE hart mask");
        crate::sbi::remote_sfence_vma(hart_mask, hart_mask_base, start, size)?;
        pending &= !covered;
    }
    Ok(())
}

pub fn handle_tlb_ipi() -> bool {
    // SBI RFENCE is handled in M-mode and completes before the issuing call
    // returns on the supported OpenSBI path. It does not raise an S-mode IPI.
    false
}

pub const fn tlb_backend_name() -> &'static str {
    "sbi-rfence"
}

pub fn install_cpu_local(pointer: usize) {
    unsafe {
        asm!("mv tp, {pointer}", pointer = in(reg) pointer, options(nomem, nostack));
    }
}

pub fn cpu_local_ptr() -> usize {
    let pointer: usize;
    unsafe {
        asm!("mv {pointer}, tp", pointer = out(reg) pointer, options(nomem, nostack));
    }
    pointer
}

pub fn park_without_interrupts() -> ! {
    loop {
        unsafe {
            asm!("wfi");
        }
    }
}
