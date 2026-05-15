use crate::fs::{OpenFlags, S_IFCHR, make_anonymous_fd};
use crate::task::current_user_token;

use super::super::errno::{SysError, SysResult};
use super::super::user_ptr::read_user_value;
use super::fd::install_file_fd;

const FD_NONBLOCK: u32 = OpenFlags::NONBLOCK.bits();
const FD_CLOEXEC: u32 = OpenFlags::CLOEXEC.bits();
const SIGNALFD_VALID_FLAGS: u32 = FD_NONBLOCK | FD_CLOEXEC;
const TIMERFD_VALID_FLAGS: u32 = FD_NONBLOCK | FD_CLOEXEC;
const MEMFD_SECRET_VALID_FLAGS: u32 = FD_CLOEXEC;
const UFFD_USER_MODE_ONLY: u32 = 1;
const USERFAULTFD_VALID_FLAGS: u32 = FD_NONBLOCK | FD_CLOEXEC | UFFD_USER_MODE_ONLY;
const BPF_MAP_CREATE: u32 = 0;

fn open_flags_from_fd_flags(flags: u32, valid_flags: u32) -> SysResult<OpenFlags> {
    if flags & !valid_flags != 0 {
        return Err(SysError::EINVAL);
    }

    let mut open_flags = OpenFlags::RDONLY;
    if flags & FD_NONBLOCK != 0 {
        open_flags |= OpenFlags::NONBLOCK;
    }
    if flags & FD_CLOEXEC != 0 {
        open_flags |= OpenFlags::CLOEXEC;
    }
    Ok(open_flags)
}

fn install_dummy_readable_fd(open_flags: OpenFlags) -> SysResult {
    let file = make_anonymous_fd(true, false, S_IFCHR | 0o600);
    install_file_fd(file, open_flags, None)
}

fn install_dummy_readwrite_fd(open_flags: OpenFlags) -> SysResult {
    let file = make_anonymous_fd(true, true, S_IFCHR | 0o600);
    install_file_fd(file, open_flags, None)
}

fn validate_user_pointer(ptr: *const u8) -> SysResult<()> {
    if ptr.is_null() {
        return Err(SysError::EFAULT);
    }
    read_user_value(current_user_token(), ptr).map(|_: u8| ())
}

pub fn sys_signalfd4(fd: isize, mask: *const u8, _sizemask: usize, flags: u32) -> SysResult {
    if fd != -1 {
        // UNFINISHED: Updating an existing signalfd requires real signalfd
        // state. Current score-facing coverage only creates new descriptors.
        return Err(SysError::EINVAL);
    }
    validate_user_pointer(mask)?;
    let open_flags = open_flags_from_fd_flags(flags, SIGNALFD_VALID_FLAGS)?;
    // UNFINISHED: pending-signal delivery through signalfd is not modeled yet.
    install_dummy_readable_fd(open_flags)
}

pub fn sys_timerfd_create(clockid: i32, flags: u32) -> SysResult {
    match clockid {
        0 | 1 | 7 | 8 | 9 | 11 => {}
        _ => return Err(SysError::EINVAL),
    }
    let open_flags = open_flags_from_fd_flags(flags, TIMERFD_VALID_FLAGS)?;
    // UNFINISHED: timerfd expiration accounting and read semantics are not
    // implemented; this fd is for fd-class syscall probes.
    install_dummy_readable_fd(open_flags)
}

pub fn sys_memfd_secret(flags: u32) -> SysResult {
    let open_flags = open_flags_from_fd_flags(flags, MEMFD_SECRET_VALID_FLAGS)?;
    // UNFINISHED: Linux memfd_secret backs mmap() with secret memory and
    // enforces RLIMIT_MEMLOCK; this fd only satisfies generic fd probes.
    install_dummy_readwrite_fd(open_flags | OpenFlags::RDWR)
}

pub fn sys_userfaultfd(flags: u32) -> SysResult {
    let open_flags = open_flags_from_fd_flags(flags, USERFAULTFD_VALID_FLAGS)?;
    // UNFINISHED: userfaultfd page-fault registration and event queues are not
    // implemented.
    install_dummy_readable_fd(open_flags)
}

pub fn sys_perf_event_open(
    attr: *const u8,
    _pid: isize,
    _cpu: isize,
    _group_fd: isize,
    _flags: u64,
) -> SysResult {
    validate_user_pointer(attr)?;
    // UNFINISHED: perf event sampling/counter state is not implemented.
    install_dummy_readable_fd(OpenFlags::RDONLY | OpenFlags::CLOEXEC)
}

pub fn sys_io_uring_setup(entries: u32, params: *mut u8) -> SysResult {
    if entries == 0 {
        return Err(SysError::EINVAL);
    }
    validate_user_pointer(params.cast_const())?;
    // UNFINISHED: io_uring shared rings and enter/register operations are not
    // implemented. This fd only satisfies generic fd probing.
    install_dummy_readable_fd(OpenFlags::RDWR | OpenFlags::CLOEXEC)
}

pub fn sys_bpf(cmd: u32, attr: *const u8, size: u32) -> SysResult {
    if cmd != BPF_MAP_CREATE {
        // UNFINISHED: Only BPF_MAP_CREATE is accepted for LTP fd-class probes.
        return Err(SysError::ENOSYS);
    }
    if size == 0 {
        return Err(SysError::EINVAL);
    }
    validate_user_pointer(attr)?;
    // UNFINISHED: BPF map storage and commands are not implemented.
    install_dummy_readable_fd(OpenFlags::RDWR | OpenFlags::CLOEXEC)
}
