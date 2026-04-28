use crate::trap::trap_return;

#[repr(C)]
pub struct TaskContext {
    sp: usize,
    tp: usize,
    s: [usize; 10],
    ra: usize,
}

impl TaskContext {
    pub fn zero_init() -> Self {
        Self {
            sp: 0,
            tp: 0,
            s: [0; 10],
            ra: 0,
        }
    }

    pub fn goto_trap_return(kstack_ptr: usize) -> Self {
        Self {
            sp: kstack_ptr,
            tp: 0,
            s: [0; 10],
            ra: trap_return as usize,
        }
    }
}
