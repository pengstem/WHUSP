core::arch::global_asm!(include_str!("switch.S"));

unsafe extern "C" {
    pub fn __switch(
        current_task_cx_ptr: *mut super::TaskContext,
        next_task_cx_ptr: *const super::TaskContext,
    );
}
