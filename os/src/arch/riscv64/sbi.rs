/// use sbi call to set timer
pub fn set_timer(timer: usize) {
    sbi_rt::set_timer(timer as _);
}

pub fn hsm_available() -> bool {
    sbi_rt::probe_extension(sbi_rt::Hsm).is_available()
}

pub fn ipi_available() -> bool {
    sbi_rt::probe_extension(sbi_rt::Ipi).is_available()
}

pub fn hart_status(hart_id: usize) -> Result<usize, usize> {
    let result = sbi_rt::hart_get_status(hart_id);
    if result.is_ok() {
        Ok(result.value)
    } else {
        Err(result.error)
    }
}

pub fn start_hart(hart_id: usize, start_addr: usize, opaque: usize) -> Result<(), usize> {
    let result = sbi_rt::hart_start(hart_id, start_addr, opaque);
    if result.is_ok() {
        Ok(())
    } else {
        Err(result.error)
    }
}

pub fn send_ipi(hart_id: usize) -> Result<(), usize> {
    let mask = sbi_rt::HartMask::from_mask_base(1, hart_id);
    let result = sbi_rt::send_ipi(mask);
    if result.is_ok() {
        Ok(())
    } else {
        Err(result.error)
    }
}

/// use sbi call to shutdown the kernel
pub fn shutdown(failure: bool) -> ! {
    use sbi_rt::{NoReason, Shutdown, SystemFailure, system_reset};
    if !failure {
        system_reset(Shutdown, NoReason);
    } else {
        system_reset(Shutdown, SystemFailure);
    }
    unreachable!()
}
