use riscv::register::sstatus::{self, FS, SPP, Sstatus};

// Keep this repr(C) field order synchronized with trap.S fixed offsets:
// x[0..31], sstatus at 32*8, sepc at 33*8, kernel metadata at 34..36*8,
// FP state at 37..69*8, kernel_entry_flush at 70*8, and kernel_tp at 71*8.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct TrapContext {
    pub x: [usize; 32],
    pub sstatus: Sstatus,
    pub sepc: usize,
    pub kernel_satp: usize,
    pub kernel_sp: usize,
    pub trap_handler: usize,
    pub f: [u64; 32],
    pub fcsr: u32,
    pub fpu_state_valid: u32,
    pub kernel_entry_flush: usize,
    pub kernel_tp: usize,
}

// RISC-V psABI register indexes used by set_*: x2=sp, x4=tp, x10=a0.
impl TrapContext {
    pub fn set_sp(&mut self, sp: usize) {
        self.x[2] = sp;
    }
    pub fn set_tp(&mut self, tp: usize) {
        self.x[4] = tp;
    }
    pub fn set_a0(&mut self, a0: usize) {
        self.x[10] = a0;
    }

    fn user_sstatus_with_fs(fs: FS) -> Sstatus {
        let original_fs = sstatus::read().fs();
        unsafe {
            sstatus::set_fs(fs);
        }
        let mut sstatus = sstatus::read();
        unsafe {
            sstatus::set_fs(original_fs);
        }
        // set CPU privilege to User after trapping back
        sstatus.set_spp(SPP::User);
        sstatus
    }

    fn app_init_sstatus() -> Sstatus {
        Self::user_sstatus_with_fs(FS::Off)
    }

    pub fn user_fp_is_off(&self) -> bool {
        self.sstatus.fs() == FS::Off
    }

    pub fn user_fp_is_dirty(&self) -> bool {
        self.sstatus.fs() == FS::Dirty
    }

    pub fn mark_user_fp_active(&mut self) {
        self.sstatus = Self::user_sstatus_with_fs(FS::Dirty);
        self.fpu_state_valid = 1;
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
            sstatus: Self::app_init_sstatus(),
            sepc: entry,
            kernel_satp,
            kernel_sp,
            trap_handler,
            f: [0; 32],
            fcsr: 0,
            fpu_state_valid: 0,
            kernel_entry_flush: 1,
            kernel_tp: 0,
        };
        cx.set_sp(sp);
        cx
    }
}
