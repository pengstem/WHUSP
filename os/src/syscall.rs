const SYSCALL_GETCWD: usize = 17;
const SYSCALL_DUP: usize = 24;
const SYSCALL_IOCTL: usize = 29;
const SYSCALL_MKDIRAT: usize = 34;
const SYSCALL_UNLINKAT: usize = 35;
const SYSCALL_CHDIR: usize = 49;
const SYSCALL_OPENAT: usize = 56;
const SYSCALL_CLOSE: usize = 57;
const SYSCALL_PIPE: usize = 59;
const SYSCALL_GETDENTS64: usize = 61;
const SYSCALL_READ: usize = 63;
const SYSCALL_WRITE: usize = 64;
const SYSCALL_READV: usize = 65;
const SYSCALL_WRITEV: usize = 66;
const SYSCALL_PPOLL: usize = 73;
const SYSCALL_NEWFSTATAT: usize = 79;
const SYSCALL_FSTAT: usize = 80;
const SYSCALL_EXIT: usize = 93;
const SYSCALL_WAITID: usize = 95;
const SYSCALL_SLEEP: usize = 101;
const SYSCALL_YIELD: usize = 124;
const SYSCALL_KILL: usize = 129;
const SYSCALL_GET_TIME: usize = 169;
const SYSCALL_GETPID: usize = 172;
const SYSCALL_BRK: usize = 214;
const SYSCALL_MUNMAP: usize = 215;
const SYSCALL_CLONE: usize = 220;
const SYSCALL_EXEC: usize = 221;
const SYSCALL_MMAP: usize = 222;
const SYSCALL_WAIT4: usize = 260;

// TODO: remove or unify these syscalls
const SYSCALL_NET_CONNECT: usize = 2000;
const SYSCALL_NET_LISTEN: usize = 2001;
const SYSCALL_NET_ACCEPT: usize = 2002;
const SYSCALL_THREAD_CREATE: usize = 1000;
const SYSCALL_GETTID: usize = 1001;
const SYSCALL_WAITTID: usize = 1002;
const SYSCALL_MUTEX_CREATE: usize = 1010;
const SYSCALL_MUTEX_LOCK: usize = 1011;
const SYSCALL_MUTEX_UNLOCK: usize = 1012;
const SYSCALL_SEMAPHORE_CREATE: usize = 1020;
const SYSCALL_SEMAPHORE_UP: usize = 1021;
const SYSCALL_SEMAPHORE_DOWN: usize = 1022;
const SYSCALL_CONDVAR_CREATE: usize = 1030;
const SYSCALL_CONDVAR_SIGNAL: usize = 1031;
const SYSCALL_CONDVAR_WAIT: usize = 1032;

mod errno;
mod fs;
mod memory;
mod net;
mod process;
mod sync;
mod thread;
mod wait;

use errno::{SysError, ret};
use fs::*;
use memory::*;
use net::*;
use process::*;
use sync::*;
use thread::*;
use wait::*;

pub fn syscall(syscall_id: usize, args: [usize; 6]) -> isize {
    if syscall_id == SYSCALL_EXIT {
        sys_exit(args[0] as i32);
    }

    ret(match syscall_id {
        SYSCALL_GETCWD => sys_getcwd(args[0] as *mut u8, args[1]),
        SYSCALL_DUP => sys_dup(args[0]),
        SYSCALL_IOCTL => sys_ioctl(args[0], args[1], args[2]),
        SYSCALL_MKDIRAT => sys_mkdirat(args[0] as isize, args[1] as *const u8, args[2] as u32),
        SYSCALL_UNLINKAT => sys_unlinkat(args[0] as isize, args[1] as *const u8, args[2] as u32),
        SYSCALL_CHDIR => sys_chdir(args[0] as *const u8),
        SYSCALL_OPENAT => sys_openat(
            args[0] as isize,
            args[1] as *const u8,
            args[2] as u32,
            args[3] as u32,
        ),
        SYSCALL_CLOSE => sys_close(args[0]),
        SYSCALL_PIPE => sys_pipe(args[0] as *mut usize),
        SYSCALL_GETDENTS64 => sys_getdents64(args[0], args[1] as *mut u8, args[2]),
        SYSCALL_READV => sys_readv(args[0], args[1] as *const LinuxIovec, args[2]),
        SYSCALL_READ => sys_read(args[0], args[1] as *const u8, args[2]),
        SYSCALL_WRITE => sys_write(args[0], args[1] as *const u8, args[2]),
        SYSCALL_WRITEV => sys_writev(args[0], args[1] as *const LinuxIovec, args[2]),
        SYSCALL_PPOLL => sys_ppoll(
            args[0] as *mut LinuxPollFd,
            args[1],
            args[2] as *const LinuxTimeSpec,
            args[3] as *const u8,
            args[4],
        ),
        SYSCALL_NEWFSTATAT => sys_fstatat(
            args[0] as isize,
            args[1] as *const u8,
            args[2] as *mut LinuxKstat,
            args[3] as i32,
        ),
        SYSCALL_FSTAT => sys_fstat(args[0], args[1] as *mut LinuxKstat),
        SYSCALL_WAITID => sys_waitid(
            args[0] as i32,
            args[1] as i32,
            args[2] as *mut LinuxSigInfo,
            args[3] as i32,
            args[4] as *mut RUsage,
        ),
        SYSCALL_SLEEP => Ok(sys_sleep(args[0])),
        SYSCALL_YIELD => Ok(sys_yield()),
        SYSCALL_KILL => sys_kill(args[0], args[1] as u32),
        SYSCALL_GET_TIME => Ok(sys_get_time()),
        SYSCALL_GETPID => Ok(sys_getpid()),
        SYSCALL_BRK => sys_brk(args[0]),
        SYSCALL_MUNMAP => sys_munmap(args[0], args[1]),
        SYSCALL_CLONE => sys_clone(args[0], args[1], args[2], args[3], args[4]),
        SYSCALL_EXEC => sys_exec(
            args[0] as *const u8,
            args[1] as *const usize,
            args[2] as *const usize,
        ),
        SYSCALL_MMAP => sys_mmap(args[0], args[1], args[2], args[3], args[4], args[5]),
        SYSCALL_WAIT4 => sys_wait4(
            args[0] as isize,
            args[1] as *mut i32,
            args[2] as i32,
            args[3] as *mut RUsage,
        ),
        SYSCALL_NET_CONNECT => Ok(sys_connect(args[0] as _, args[1] as _, args[2] as _)),
        SYSCALL_NET_LISTEN => Ok(sys_listen(args[0] as _)),
        SYSCALL_NET_ACCEPT => Ok(sys_accept(args[0] as _)),
        SYSCALL_THREAD_CREATE => Ok(sys_thread_create(args[0], args[1])),
        SYSCALL_GETTID => Ok(sys_gettid()),
        SYSCALL_WAITTID => Ok(sys_waittid(args[0]) as isize),
        SYSCALL_MUTEX_CREATE => Ok(sys_mutex_create(args[0] == 1)),
        SYSCALL_MUTEX_LOCK => Ok(sys_mutex_lock(args[0])),
        SYSCALL_MUTEX_UNLOCK => Ok(sys_mutex_unlock(args[0])),
        SYSCALL_SEMAPHORE_CREATE => Ok(sys_semaphore_create(args[0])),
        SYSCALL_SEMAPHORE_UP => Ok(sys_semaphore_up(args[0])),
        SYSCALL_SEMAPHORE_DOWN => Ok(sys_semaphore_down(args[0])),
        SYSCALL_CONDVAR_CREATE => Ok(sys_condvar_create()),
        SYSCALL_CONDVAR_SIGNAL => Ok(sys_condvar_signal(args[0])),
        SYSCALL_CONDVAR_WAIT => Ok(sys_condvar_wait(args[0], args[1])),
        _ => Err(SysError::ENOSYS),
    })
}
