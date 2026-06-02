use riscv::register::sstatus::{self, FS, SPP, Sstatus};

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
}

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

    pub fn mark_user_fp_disabled(&mut self) {
        self.sstatus = Self::user_sstatus_with_fs(FS::Off);
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
        };
        cx.set_sp(sp);
        cx
    }
}
