use super::*;

const WAIT_PENDING: isize = -2;
const WNOHANG: i32 = 1;
pub const WEXITED: i32 = 4;
pub const WNOWAIT: i32 = 0x01000000;
pub const P_ALL: i32 = 0;
pub const P_PID: i32 = 1;

#[repr(C)]
#[derive(Clone, Copy)]
pub struct UtsName {
    pub sysname: [u8; 65],
    pub nodename: [u8; 65],
    pub release: [u8; 65],
    pub version: [u8; 65],
    pub machine: [u8; 65],
    pub domainname: [u8; 65],
}

impl Default for UtsName {
    fn default() -> Self {
        Self {
            sysname: [0; 65],
            nodename: [0; 65],
            release: [0; 65],
            version: [0; 65],
            machine: [0; 65],
            domainname: [0; 65],
        }
    }
}

fn wait_exit_code(status: i32) -> i32 {
    if status < 0 {
        status
    } else {
        (status >> 8) & 0xff
    }
}

pub fn exit(exit_code: i32) -> ! {
    sys_exit(exit_code);
}
pub fn yield_() -> isize {
    sys_yield()
}
pub fn get_time() -> isize {
    sys_get_time()
}
pub fn getpid() -> isize {
    sys_getpid()
}
pub fn getppid() -> isize {
    sys_getppid()
}
pub fn uname(buf: &mut UtsName) -> isize {
    sys_uname(buf as *mut _ as *mut u8)
}
pub fn brk(addr: usize) -> usize {
    sys_brk(addr) as usize
}
pub fn fork() -> isize {
    let ret = sys_fork();
    if ret < 0 { -1 } else { ret }
}
pub fn exec(path: &str, args: &[*const u8]) -> isize {
    let ret = sys_exec(path, args);
    if ret < 0 { -1 } else { ret }
}
pub fn execve(path: &str, args: &[*const u8], envs: &[*const u8]) -> isize {
    let ret = sys_execve(path, args, envs);
    if ret < 0 { -1 } else { ret }
}

pub fn wait(exit_code: &mut i32) -> isize {
    loop {
        let mut status = 0;
        match sys_wait4(-1, &mut status, WNOHANG, core::ptr::null_mut()) {
            0 | WAIT_PENDING => {
                yield_();
            }
            exit_pid if exit_pid > 0 => {
                *exit_code = wait_exit_code(status);
                return exit_pid;
            }
            exit_pid if exit_pid < 0 => return -1,
            exit_pid => return exit_pid,
        }
    }
}

pub fn waitpid(pid: usize, exit_code: &mut i32) -> isize {
    loop {
        let mut status = 0;
        match sys_wait4(pid as isize, &mut status, WNOHANG, core::ptr::null_mut()) {
            0 | WAIT_PENDING => {
                yield_();
            }
            exit_pid if exit_pid > 0 => {
                *exit_code = wait_exit_code(status);
                return exit_pid;
            }
            exit_pid if exit_pid < 0 => return -1,
            exit_pid => return exit_pid,
        }
    }
}

pub fn waitpid_nb(pid: usize, exit_code: &mut i32) -> isize {
    let mut status = 0;
    let ret = sys_wait4(pid as isize, &mut status, WNOHANG, core::ptr::null_mut());
    if ret > 0 {
        *exit_code = wait_exit_code(status);
    } else if ret < 0 {
        return -1;
    }
    ret
}

pub fn waitid(idtype: i32, id: i32, infop: &mut SigInfo, options: i32) -> isize {
    sys_waitid(idtype, id, infop as *mut _, options, core::ptr::null_mut())
}

bitflags! {
    pub struct SignalFlags: i32 {
        const SIGINT    = 1 << 2;
        const SIGILL    = 1 << 4;
        const SIGABRT   = 1 << 6;
        const SIGFPE    = 1 << 8;
        const SIGSEGV   = 1 << 11;
    }
}

pub fn kill(pid: usize, signal: i32) -> isize {
    sys_kill(pid, signal)
}

pub fn sleep(sleep_ms: usize) {
    sys_sleep(sleep_ms);
}

pub fn thread_create(entry: usize, arg: usize) -> isize {
    sys_thread_create(entry, arg)
}
pub fn gettid() -> isize {
    sys_gettid()
}
pub fn waittid(tid: usize) -> isize {
    loop {
        match sys_waittid(tid) {
            -2 => {
                yield_();
            }
            exit_code => return exit_code,
        }
    }
}
