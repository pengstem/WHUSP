#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct TrapContext {
    pub x: [usize; 32],
    pub prmd: usize,
    pub era: usize,
    pub kernel_satp: usize,
    pub kernel_sp: usize,
    pub trap_handler: usize,
}

impl TrapContext {
    pub fn set_sp(&mut self, sp: usize) {
        self.x[3] = sp;
    }

    pub fn set_tp(&mut self, tp: usize) {
        self.x[2] = tp;
    }

    pub fn set_a0(&mut self, a0: usize) {
        self.x[4] = a0;
    }

    pub fn app_init_context(
        entry: usize,
        sp: usize,
        kernel_satp: usize,
        kernel_sp: usize,
        trap_handler: usize,
    ) -> Self {
        let mut cx = Self {
            x: [0; 32],
            prmd: 0b0111,
            era: entry,
            kernel_satp,
            kernel_sp,
            trap_handler,
        };
        cx.set_sp(sp);
        cx
    }
}
