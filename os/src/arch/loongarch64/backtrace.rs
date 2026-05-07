pub fn frame_pointer() -> usize {
    0
}

// CONTEXT: stubs for LA stack-walking; LA backtrace is not yet implemented but
// the API mirrors `arch/riscv64/backtrace.rs` for future symmetry.
#[allow(dead_code)]
pub fn previous_frame_pointer(_fp: usize) -> usize {
    0
}

#[allow(dead_code)]
pub fn return_address(_fp: usize) -> usize {
    0
}
