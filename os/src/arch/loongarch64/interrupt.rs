use loongArch64::register::crmd;

pub fn supervisor_interrupt_enabled() -> bool {
    crmd::read().ie()
}

pub fn enable_supervisor_interrupt() {
    crmd::set_ie(true);
}

pub fn disable_supervisor_interrupt() {
    crmd::set_ie(false);
}
