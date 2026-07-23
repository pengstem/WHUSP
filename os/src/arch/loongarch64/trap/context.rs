// Keep this repr(C) field order synchronized with trap.S fixed offsets:
// x[0..31], PRMD at 32*8, ERA at 33*8, kernel metadata at 34..36*8,
// vector registers at 40*8 (32-byte aligned), FCC at 168*8, and FCSR at 169*8.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct TrapContext {
    pub x: [usize; 32],
    pub prmd: usize,
    pub era: usize,
    pub kernel_satp: usize,
    pub kernel_sp: usize,
    pub trap_handler: usize,
    pub _vector_align: [u64; 3],
    /// Full LASX-width state. FPU and LSX machines use the low 64/128 bits.
    pub vector: [[u64; 4]; 32],
    pub fcc: u64,
    pub fcsr: u32,
    pub _fpu_reserved: u32,
}

const _: () = {
    assert!(core::mem::offset_of!(TrapContext, vector) == 40 * 8);
    assert!(core::mem::offset_of!(TrapContext, fcc) == 168 * 8);
    assert!(core::mem::offset_of!(TrapContext, fcsr) == 169 * 8);
    assert!(core::mem::size_of::<TrapContext>() == 170 * 8);
};

// LoongArch LP64 ABI register indexes used by set_*: r3=sp, r2=tp, r4=a0.
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
            // PRMD.PPLV=3 and PRMD.PIE=1 so ertn enters user mode with
            // interrupts enabled.
            prmd: 0b0111,
            era: entry,
            kernel_satp,
            kernel_sp,
            trap_handler,
            _vector_align: [0; 3],
            vector: [[0; 4]; 32],
            fcc: 0,
            fcsr: 0,
            _fpu_reserved: 0,
        };
        cx.set_sp(sp);
        cx
    }
}
