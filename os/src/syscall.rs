const SYSCALL_GETCWD: usize = 17;
const SYSCALL_DUP: usize = 23;
const SYSCALL_DUP3: usize = 24;
const SYSCALL_FCNTL: usize = 25;
const SYSCALL_IOCTL: usize = 29;
const SYSCALL_MKDIRAT: usize = 34;
const SYSCALL_UNLINKAT: usize = 35;
const SYSCALL_LINKAT: usize = 37;
const SYSCALL_UMOUNT2: usize = 39;
const SYSCALL_MOUNT: usize = 40;
const SYSCALL_CHDIR: usize = 49;
const SYSCALL_OPENAT: usize = 56;
const SYSCALL_CLOSE: usize = 57;
const SYSCALL_PIPE2: usize = 59;
const SYSCALL_GETDENTS64: usize = 61;
const SYSCALL_READ: usize = 63;
const SYSCALL_WRITE: usize = 64;
const SYSCALL_READV: usize = 65;
const SYSCALL_WRITEV: usize = 66;
const SYSCALL_PPOLL: usize = 73;
const SYSCALL_NEWFSTATAT: usize = 79;
const SYSCALL_FSTAT: usize = 80;
const SYSCALL_EXIT: usize = 93;
const SYSCALL_EXIT_GROUP: usize = 94;
const SYSCALL_WAITID: usize = 95;
const SYSCALL_NANOSLEEP: usize = 101;
const SYSCALL_CLOCK_NANOSLEEP: usize = 115;
const SYSCALL_SCHED_YIELD: usize = 124;
const SYSCALL_KILL: usize = 129;
const SYSCALL_TIMES: usize = 153;
const SYSCALL_UNAME: usize = 160;
const SYSCALL_GETTIMEOFDAY: usize = 169;
const SYSCALL_GETPID: usize = 172;
const SYSCALL_GETPPID: usize = 173;
const SYSCALL_BRK: usize = 214;
const SYSCALL_MUNMAP: usize = 215;
const SYSCALL_CLONE: usize = 220;
const SYSCALL_EXECVE: usize = 221;
const SYSCALL_MMAP: usize = 222;
const SYSCALL_MPROTECT: usize = 226;
const SYSCALL_WAIT4: usize = 260;
const SYSCALL_RENAMEAT2: usize = 276;
const SYSCALL_STATX: usize = 291;

mod errno;
mod fs;
mod memory;
mod process;
mod sync;
mod wait;

use errno::{SysError, ret};
use fs::*;
use memory::*;
use process::*;
use sync::*;
use wait::*;

pub fn syscall(syscall_id: usize, args: [usize; 6]) -> isize {
    if syscall_id == SYSCALL_EXIT {
        sys_exit(args[0] as i32);
    }
    if syscall_id == SYSCALL_EXIT_GROUP {
        sys_exit_group(args[0] as i32);
    }

    ret(match syscall_id {
        SYSCALL_GETCWD => sys_getcwd(args[0] as *mut u8, args[1]),
        SYSCALL_DUP => sys_dup(args[0]),
        SYSCALL_DUP3 => sys_dup3(args[0], args[1], args[2] as u32),
        SYSCALL_FCNTL => sys_fcntl(args[0], args[1], args[2]),
        SYSCALL_IOCTL => sys_ioctl(args[0], args[1], args[2]),
        SYSCALL_MKDIRAT => sys_mkdirat(args[0] as isize, args[1] as *const u8, args[2] as u32),
        SYSCALL_UNLINKAT => sys_unlinkat(args[0] as isize, args[1] as *const u8, args[2] as u32),
        SYSCALL_LINKAT => sys_linkat(
            args[0] as isize,
            args[1] as *const u8,
            args[2] as isize,
            args[3] as *const u8,
            args[4] as u32,
        ),
        SYSCALL_RENAMEAT2 => sys_renameat2(
            args[0] as isize,
            args[1] as *const u8,
            args[2] as isize,
            args[3] as *const u8,
            args[4] as u32,
        ),
        SYSCALL_UMOUNT2 => sys_umount2(args[0] as *const u8, args[1] as i32),
        SYSCALL_MOUNT => sys_mount(
            args[0] as *const u8,
            args[1] as *const u8,
            args[2] as *const u8,
            args[3],
            args[4] as *const u8,
        ),
        SYSCALL_CHDIR => sys_chdir(args[0] as *const u8),
        SYSCALL_OPENAT => sys_openat(
            args[0] as isize,
            args[1] as *const u8,
            args[2] as u32,
            args[3] as u32,
        ),
        SYSCALL_CLOSE => sys_close(args[0]),
        SYSCALL_PIPE2 => sys_pipe2(args[0] as *mut i32, args[1] as u32),
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
        SYSCALL_NEWFSTATAT => sys_newfstatat(
            args[0] as isize,
            args[1] as *const u8,
            args[2] as *mut LinuxKstat,
            args[3] as i32,
        ),
        SYSCALL_FSTAT => sys_fstat(args[0], args[1] as *mut LinuxKstat),
        SYSCALL_STATX => sys_statx(
            args[0] as isize,
            args[1] as *const u8,
            args[2] as i32,
            args[3] as u32,
            args[4] as *mut LinuxStatx,
        ),
        SYSCALL_WAITID => sys_waitid(
            args[0] as i32,
            args[1] as i32,
            args[2] as *mut LinuxSigInfo,
            args[3] as i32,
            args[4] as *mut RUsage,
        ),
        SYSCALL_NANOSLEEP => sys_nanosleep(
            args[0] as *const LinuxTimeSpec,
            args[1] as *mut LinuxTimeSpec,
        ),
        SYSCALL_CLOCK_NANOSLEEP => sys_clock_nanosleep(
            args[0] as i32,
            args[1] as u32,
            args[2] as *const LinuxTimeSpec,
            args[3] as *mut LinuxTimeSpec,
        ),
        SYSCALL_SCHED_YIELD => Ok(sys_sched_yield()),
        SYSCALL_KILL => sys_kill(args[0], args[1] as u32),
        SYSCALL_TIMES => sys_times(args[0] as *mut LinuxTms),
        SYSCALL_UNAME => sys_uname(args[0] as *mut LinuxUtsName),
        SYSCALL_GETTIMEOFDAY => {
            sys_gettimeofday(args[0] as *mut LinuxTimeVal, args[1] as *mut LinuxTimezone)
        }
        SYSCALL_GETPID => Ok(sys_getpid()),
        SYSCALL_GETPPID => Ok(sys_getppid()),
        SYSCALL_BRK => sys_brk(args[0]),
        SYSCALL_MUNMAP => sys_munmap(args[0], args[1]),
        SYSCALL_MPROTECT => sys_mprotect(args[0], args[1], args[2]),
        SYSCALL_CLONE => sys_clone(args[0], args[1], args[2], args[3], args[4]),
        SYSCALL_EXECVE => sys_execve(
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
        _ => Err(SysError::ENOSYS),
    })
}
