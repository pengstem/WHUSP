// Keep this repr(C) field order synchronized with trap.S fixed offsets:
// x[0..31], PRMD at 32*8, ERA at 33*8, and kernel metadata at 34..36*8.
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
}

const _: () = {
    assert!(core::mem::size_of::<TrapContext>() == 40 * 8);
};

#[derive(Debug, Clone, Copy, Eq, PartialEq, Ord, PartialOrd)]
#[repr(u8)]
pub enum UserFpMode {
    Scalar = 1,
    Lsx = 2,
    Lasx = 3,
}

/// Lazily allocated LoongArch user FP/vector state.
///
/// LSX and LASX overlap the low 128/256 bits of the scalar FP register file,
/// so one widest-form buffer preserves every mode without copying it through
/// the ordinary integer-only TrapContext.
#[repr(C, align(32))]
#[derive(Debug, Clone, Copy)]
pub struct UserFpState {
    pub vector: [[u64; 4]; 32],
    pub fcc: u64,
    pub fcsr: u32,
    mode: u8,
    restore_pending: bool,
    _reserved: [u8; 18],
}

const _: () = {
    assert!(core::mem::offset_of!(UserFpState, vector) == 0);
    assert!(core::mem::offset_of!(UserFpState, fcc) == 128 * 8);
    assert!(core::mem::offset_of!(UserFpState, fcsr) == 129 * 8);
    assert!(core::mem::size_of::<UserFpState>() == 132 * 8);
    assert!(core::mem::align_of::<UserFpState>() == 32);
};

impl UserFpState {
    pub fn new(mode: UserFpMode) -> Self {
        Self {
            vector: [[0; 4]; 32],
            fcc: 0,
            fcsr: 0,
            mode: mode as u8,
            restore_pending: true,
            _reserved: [0; 18],
        }
    }

    pub fn mode(&self) -> Option<UserFpMode> {
        match self.mode {
            1 => Some(UserFpMode::Scalar),
            2 => Some(UserFpMode::Lsx),
            3 => Some(UserFpMode::Lasx),
            _ => None,
        }
    }

    pub fn upgrade(&mut self, mode: UserFpMode) {
        if self.mode().is_none_or(|current| current < mode) {
            self.mode = mode as u8;
        }
        self.restore_pending = true;
    }

    pub fn needs_restore(&self) -> bool {
        self.restore_pending
    }

    pub fn mark_live(&mut self) {
        self.restore_pending = false;
    }

    pub fn mark_saved(&mut self) {
        self.restore_pending = true;
    }

    pub fn validate(&self) -> bool {
        self.mode().is_some()
    }
}

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
        };
        cx.set_sp(sp);
        cx
    }
}
