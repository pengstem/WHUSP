const SYSCALL_GETCWD: usize = 17;
const SYSCALL_DUP: usize = 23;
const SYSCALL_DUP3: usize = 24;
const SYSCALL_FCNTL: usize = 25;
const SYSCALL_IOCTL: usize = 29;
const SYSCALL_MKDIRAT: usize = 34;
const SYSCALL_UNLINKAT: usize = 35;
const SYSCALL_UMOUNT2: usize = 39;
const SYSCALL_MOUNT: usize = 40;
const SYSCALL_CHDIR: usize = 49;
const SYSCALL_OPENAT: usize = 56;
const SYSCALL_CLOSE: usize = 57;
const SYSCALL_PIPE2: usize = 59;
const SYSCALL_GETDENTS64: usize = 61;
const SYSCALL_READ: usize = 63;
const SYSCALL_WRITE: usize = 64;
const SYSCALL_EXIT: usize = 93;
const SYSCALL_WAITID: usize = 95;
const SYSCALL_SLEEP: usize = 101;
const SYSCALL_YIELD: usize = 124;
const SYSCALL_KILL: usize = 129;
const SYSCALL_GET_TIME: usize = 169;
const SYSCALL_GETPID: usize = 172;
const SYSCALL_BRK: usize = 214;
const SYSCALL_CLONE: usize = 220;
const SYSCALL_EXEC: usize = 221;
const SYSCALL_WAIT4: usize = 260;
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

const AT_FDCWD: isize = -100;

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct SigInfo {
    pub si_signo: i32,
    pub si_errno: i32,
    pub si_code: i32,
    pub si_trapno: i32,
    pub si_pid: i32,
    pub si_uid: u32,
    pub si_status: i32,
    pub si_utime: u32,
    pub si_stime: u32,
    pub si_value: u64,
    pad: [u32; 20],
    align: [u64; 0],
}

fn syscall(id: usize, args: [usize; 6]) -> isize {
    let mut ret: isize;
    unsafe {
        core::arch::asm!(
            "ecall",
            inlateout("x10") args[0] => ret,
            in("x11") args[1],
            in("x12") args[2],
            in("x13") args[3],
            in("x14") args[4],
            in("x15") args[5],
            in("x17") id
        );
    }
    ret
}

pub fn sys_dup(fd: usize) -> isize {
    syscall(SYSCALL_DUP, [fd, 0, 0, 0, 0, 0])
}

pub fn sys_dup3(old_fd: usize, new_fd: usize, flags: u32) -> isize {
    syscall(SYSCALL_DUP3, [old_fd, new_fd, flags as usize, 0, 0, 0])
}

pub fn sys_fcntl(fd: usize, op: usize, arg: usize) -> isize {
    syscall(SYSCALL_FCNTL, [fd, op, arg, 0, 0, 0])
}

pub fn sys_ioctl(fd: usize, request: usize, argp: usize) -> isize {
    syscall(SYSCALL_IOCTL, [fd, request, argp, 0, 0, 0])
}

pub fn sys_connect(dest: u32, sport: u16, dport: u16) -> isize {
    syscall(
        SYSCALL_NET_CONNECT,
        [dest as usize, sport as usize, dport as usize, 0, 0, 0],
    )
}

// just listen for tcp connections now
pub fn sys_listen(sport: u16) -> isize {
    syscall(SYSCALL_NET_LISTEN, [sport as usize, 0, 0, 0, 0, 0])
}

pub fn sys_accept(socket_fd: usize) -> isize {
    syscall(SYSCALL_NET_ACCEPT, [socket_fd, 0, 0, 0, 0, 0])
}

pub fn sys_open(path: &str, flags: u32) -> isize {
    syscall(
        SYSCALL_OPENAT,
        [
            AT_FDCWD as usize,
            path.as_ptr() as usize,
            flags as usize,
            0o644,
            0,
            0,
        ],
    )
}

pub fn sys_openat(dirfd: isize, path: &str, flags: u32, mode: u32) -> isize {
    syscall(
        SYSCALL_OPENAT,
        [
            dirfd as usize,
            path.as_ptr() as usize,
            flags as usize,
            mode as usize,
            0,
            0,
        ],
    )
}

pub fn sys_getcwd(buf: *mut u8, size: usize) -> isize {
    syscall(SYSCALL_GETCWD, [buf as usize, size, 0, 0, 0, 0])
}

pub fn sys_chdir(path: &str) -> isize {
    syscall(SYSCALL_CHDIR, [path.as_ptr() as usize, 0, 0, 0, 0, 0])
}

pub fn sys_mkdirat(dirfd: isize, path: &str, mode: u32) -> isize {
    syscall(
        SYSCALL_MKDIRAT,
        [
            dirfd as usize,
            path.as_ptr() as usize,
            mode as usize,
            0,
            0,
            0,
        ],
    )
}

pub fn sys_unlinkat(dirfd: isize, path: &str, flags: u32) -> isize {
    syscall(
        SYSCALL_UNLINKAT,
        [
            dirfd as usize,
            path.as_ptr() as usize,
            flags as usize,
            0,
            0,
            0,
        ],
    )
}

pub fn sys_umount2(target: &str, flags: i32) -> isize {
    syscall(
        SYSCALL_UMOUNT2,
        [target.as_ptr() as usize, flags as usize, 0, 0, 0, 0],
    )
}

pub fn sys_mount(source: &str, target: &str, fstype: &str, flags: usize, data: *const u8) -> isize {
    syscall(
        SYSCALL_MOUNT,
        [
            source.as_ptr() as usize,
            target.as_ptr() as usize,
            fstype.as_ptr() as usize,
            flags,
            data as usize,
            0,
        ],
    )
}

pub fn sys_getdents64(fd: usize, buf: *mut u8, len: usize) -> isize {
    syscall(SYSCALL_GETDENTS64, [fd, buf as usize, len, 0, 0, 0])
}

pub fn sys_close(fd: usize) -> isize {
    syscall(SYSCALL_CLOSE, [fd, 0, 0, 0, 0, 0])
}

pub fn sys_pipe2(pipe: &mut [i32; 2], flags: u32) -> isize {
    syscall(
        SYSCALL_PIPE2,
        [pipe.as_mut_ptr() as usize, flags as usize, 0, 0, 0, 0],
    )
}

pub fn sys_read(fd: usize, buffer: &mut [u8]) -> isize {
    syscall(
        SYSCALL_READ,
        [fd, buffer.as_mut_ptr() as usize, buffer.len(), 0, 0, 0],
    )
}

pub fn sys_write(fd: usize, buffer: &[u8]) -> isize {
    syscall(
        SYSCALL_WRITE,
        [fd, buffer.as_ptr() as usize, buffer.len(), 0, 0, 0],
    )
}

pub fn sys_exit(exit_code: i32) -> ! {
    syscall(SYSCALL_EXIT, [exit_code as usize, 0, 0, 0, 0, 0]);
    panic!("sys_exit never returns!");
}

pub fn sys_sleep(sleep_ms: usize) -> isize {
    syscall(SYSCALL_SLEEP, [sleep_ms, 0, 0, 0, 0, 0])
}

pub fn sys_yield() -> isize {
    syscall(SYSCALL_YIELD, [0, 0, 0, 0, 0, 0])
}

pub fn sys_kill(pid: usize, signal: i32) -> isize {
    syscall(SYSCALL_KILL, [pid, signal as usize, 0, 0, 0, 0])
}

pub fn sys_get_time() -> isize {
    syscall(SYSCALL_GET_TIME, [0, 0, 0, 0, 0, 0])
}

pub fn sys_getpid() -> isize {
    syscall(SYSCALL_GETPID, [0, 0, 0, 0, 0, 0])
}

pub fn sys_brk(addr: usize) -> isize {
    syscall(SYSCALL_BRK, [addr, 0, 0, 0, 0, 0])
}

pub fn sys_fork() -> isize {
    // libc-style: fork() == clone(SIGCHLD, 0, 0, 0, 0)
    syscall(SYSCALL_CLONE, [17, 0, 0, 0, 0, 0])
}

pub fn sys_exec(path: &str, args: &[*const u8]) -> isize {
    let empty_env: [*const u8; 1] = [core::ptr::null()];
    sys_execve(path, args, &empty_env)
}

pub fn sys_execve(path: &str, args: &[*const u8], envs: &[*const u8]) -> isize {
    syscall(
        SYSCALL_EXEC,
        [
            path.as_ptr() as usize,
            args.as_ptr() as usize,
            envs.as_ptr() as usize,
            0,
            0,
            0,
        ],
    )
}

pub fn sys_wait4(pid: isize, status: *mut i32, options: i32, rusage: *mut u8) -> isize {
    syscall(
        SYSCALL_WAIT4,
        [
            pid as usize,
            status as usize,
            options as usize,
            rusage as usize,
            0,
            0,
        ],
    )
}

pub fn sys_waitid(
    idtype: i32,
    id: i32,
    infop: *mut SigInfo,
    options: i32,
    rusage: *mut u8,
) -> isize {
    syscall(
        SYSCALL_WAITID,
        [
            idtype as usize,
            id as usize,
            infop as usize,
            options as usize,
            rusage as usize,
            0,
        ],
    )
}

pub fn sys_thread_create(entry: usize, arg: usize) -> isize {
    syscall(SYSCALL_THREAD_CREATE, [entry, arg, 0, 0, 0, 0])
}

pub fn sys_gettid() -> isize {
    syscall(SYSCALL_GETTID, [0; 6])
}

pub fn sys_waittid(tid: usize) -> isize {
    syscall(SYSCALL_WAITTID, [tid, 0, 0, 0, 0, 0])
}

pub fn sys_mutex_create(blocking: bool) -> isize {
    syscall(SYSCALL_MUTEX_CREATE, [blocking as usize, 0, 0, 0, 0, 0])
}

pub fn sys_mutex_lock(id: usize) -> isize {
    syscall(SYSCALL_MUTEX_LOCK, [id, 0, 0, 0, 0, 0])
}

pub fn sys_mutex_unlock(id: usize) -> isize {
    syscall(SYSCALL_MUTEX_UNLOCK, [id, 0, 0, 0, 0, 0])
}

pub fn sys_semaphore_create(res_count: usize) -> isize {
    syscall(SYSCALL_SEMAPHORE_CREATE, [res_count, 0, 0, 0, 0, 0])
}

pub fn sys_semaphore_up(sem_id: usize) -> isize {
    syscall(SYSCALL_SEMAPHORE_UP, [sem_id, 0, 0, 0, 0, 0])
}

pub fn sys_semaphore_down(sem_id: usize) -> isize {
    syscall(SYSCALL_SEMAPHORE_DOWN, [sem_id, 0, 0, 0, 0, 0])
}

pub fn sys_condvar_create() -> isize {
    syscall(SYSCALL_CONDVAR_CREATE, [0, 0, 0, 0, 0, 0])
}

pub fn sys_condvar_signal(condvar_id: usize) -> isize {
    syscall(SYSCALL_CONDVAR_SIGNAL, [condvar_id, 0, 0, 0, 0, 0])
}

pub fn sys_condvar_wait(condvar_id: usize, mutex_id: usize) -> isize {
    syscall(SYSCALL_CONDVAR_WAIT, [condvar_id, mutex_id, 0, 0, 0, 0])
}
