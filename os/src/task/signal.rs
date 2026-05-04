use bitflags::*;

pub const SIGNAL_INFO_SLOTS: usize = 65;

pub const SI_USER: i32 = 0;
pub const SIGKILL: u32 = 9;
pub const SIGCHLD: u32 = 17;
pub const SIGSTOP: u32 = 19;
pub const CLD_EXITED: i32 = 1;

bitflags! {
    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
    pub struct SignalFlags: u128 {
        const SIGHUP    = 1u128 << 1;
        const SIGINT    = 1u128 << 2;
        const SIGQUIT   = 1u128 << 3;
        const SIGILL    = 1u128 << 4;
        const SIGTRAP   = 1u128 << 5;
        const SIGABRT   = 1u128 << 6;
        const SIGBUS    = 1u128 << 7;
        const SIGFPE    = 1u128 << 8;
        const SIGKILL   = 1u128 << 9;
        const SIGUSR1   = 1u128 << 10;
        const SIGSEGV   = 1u128 << 11;
        const SIGUSR2   = 1u128 << 12;
        const SIGPIPE   = 1u128 << 13;
        const SIGALRM   = 1u128 << 14;
        const SIGTERM   = 1u128 << 15;
        const SIGCHLD   = 1u128 << 17;
        const SIGCONT   = 1u128 << 18;
        const SIGSTOP   = 1u128 << 19;
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct SignalAction {
    pub handler: usize,
    pub flags: usize,
    pub mask: SignalFlags,
}

impl SignalAction {
    pub fn is_ignore(&self) -> bool {
        self.handler == 1
    }

    pub fn has_user_handler(&self) -> bool {
        self.handler > 1
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct SignalInfo {
    pub signo: i32,
    pub code: i32,
    pub pid: i32,
    pub uid: u32,
    pub status: i32,
}

impl SignalInfo {
    pub fn user(signo: i32, pid: i32) -> Self {
        Self {
            signo,
            code: SI_USER,
            pid,
            uid: 0,
            status: 0,
        }
    }

    pub fn child_exit(signo: i32, pid: i32, status: i32) -> Self {
        Self {
            signo,
            code: CLD_EXITED,
            pid,
            uid: 0,
            status,
        }
    }
}

impl SignalFlags {
    pub fn from_signum(signum: u32) -> Option<Self> {
        if signum == 0 {
            Some(Self::empty())
        } else if signum >= SIGNAL_INFO_SLOTS as u32 {
            None
        } else {
            // CONTEXT: Linux real-time signals are ABI-visible even when this
            // kernel has no named per-signal semantics for them yet. musl uses
            // signal 33 as SIGCANCEL for pthread cancellation.
            Some(Self::from_bits_retain(1u128 << signum))
        }
    }

    pub fn check_error(&self) -> Option<(i32, &'static str)> {
        if self.contains(Self::SIGKILL) {
            Some((-9, "Killed, SIGKILL=9"))
        } else if self.contains(Self::SIGINT) {
            Some((-2, "Killed, SIGINT=2"))
        } else if self.contains(Self::SIGQUIT) {
            Some((-3, "Quit, SIGQUIT=3"))
        } else if self.contains(Self::SIGILL) {
            Some((-4, "Illegal Instruction, SIGILL=4"))
        } else if self.contains(Self::SIGTRAP) {
            Some((-5, "Trace/Breakpoint Trap, SIGTRAP=5"))
        } else if self.contains(Self::SIGABRT) {
            Some((-6, "Aborted, SIGABRT=6"))
        } else if self.contains(Self::SIGBUS) {
            Some((-7, "Bus Error, SIGBUS=7"))
        } else if self.contains(Self::SIGFPE) {
            Some((-8, "Erroneous Arithmetic Operation, SIGFPE=8"))
        } else if self.contains(Self::SIGUSR1) {
            Some((-10, "User Signal 1, SIGUSR1=10"))
        } else if self.contains(Self::SIGSEGV) {
            Some((-11, "Segmentation Fault, SIGSEGV=11"))
        } else if self.contains(Self::SIGUSR2) {
            Some((-12, "User Signal 2, SIGUSR2=12"))
        } else if self.contains(Self::SIGPIPE) {
            Some((-13, "Broken Pipe, SIGPIPE=13"))
        } else if self.contains(Self::SIGALRM) {
            Some((-14, "Alarm Clock, SIGALRM=14"))
        } else if self.contains(Self::SIGTERM) {
            Some((-15, "Terminated, SIGTERM=15"))
        } else if self.contains(Self::SIGHUP) {
            Some((-1, "Hangup, SIGHUP=1"))
        } else {
            None
        }
    }
}
