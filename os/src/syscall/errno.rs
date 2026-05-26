/// Linux errno values used by syscall adapters before `ret()` encodes them.
///
/// Syscall implementations return these positive enum variants internally; the
/// architecture trap path exposes failures to userspace as negative `-errno`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(isize)]
#[allow(dead_code)]
#[expect(
    clippy::upper_case_acronyms,
    reason = "Linux errno names intentionally keep their ABI spelling"
)]
pub enum SysError {
    EPERM = 1,
    ENOENT = 2,
    ESRCH = 3,
    EINTR = 4,
    EIO = 5,
    ENXIO = 6,
    E2BIG = 7,
    ENOEXEC = 8,
    EBADF = 9,
    ECHILD = 10,
    EAGAIN = 11,
    ENOMEM = 12,
    EACCES = 13,
    EFAULT = 14,
    ENOTBLK = 15,
    EBUSY = 16,
    EEXIST = 17,
    EXDEV = 18,
    ENODEV = 19,
    ENOTDIR = 20,
    EISDIR = 21,
    EINVAL = 22,
    EMFILE = 24,
    ENOTTY = 25,
    ETXTBSY = 26,
    EFBIG = 27,
    ENOSPC = 28,
    ESPIPE = 29,
    EROFS = 30,
    EPIPE = 32,
    ERANGE = 34,
    EDEADLK = 35,
    ENAMETOOLONG = 36,
    ENOSYS = 38,
    ENOTEMPTY = 39,
    ELOOP = 40,
    ENOMSG = 42,
    EIDRM = 43,
    ENODATA = 61,
    EOVERFLOW = 75,
    EBADMSG = 74,
    EDESTADDRREQ = 89,
    ENOPROTOOPT = 92,
    ENOTSUP = 95,
    ENOTSOCK = 88,
    EPROTONOSUPPORT = 93,
    EAFNOSUPPORT = 97,
    EADDRINUSE = 98,
    EADDRNOTAVAIL = 99,
    EISCONN = 106,
    ENOTCONN = 107,
    ECONNREFUSED = 111,
    ETIMEDOUT = 110,
    ESTALE = 116,
    EDQUOT = 122,
    ENOKEY = 126,
}

pub type SysResult<T = isize> = Result<T, SysError>;

impl From<crate::fs::FsError> for SysError {
    /// Maps VFS-layer errors onto Linux-visible errno values.
    fn from(error: crate::fs::FsError) -> Self {
        match error {
            crate::fs::FsError::NotFound => Self::ENOENT,
            crate::fs::FsError::NotDir => Self::ENOTDIR,
            crate::fs::FsError::IsDir => Self::EISDIR,
            crate::fs::FsError::AlreadyExists => Self::EEXIST,
            crate::fs::FsError::PermissionDenied => Self::EPERM,
            crate::fs::FsError::InvalidInput => Self::EINVAL,
            crate::fs::FsError::NotEmpty => Self::ENOTEMPTY,
            crate::fs::FsError::Busy => Self::EBUSY,
            crate::fs::FsError::TextBusy => Self::ETXTBSY,
            crate::fs::FsError::CrossDevice => Self::EXDEV,
            crate::fs::FsError::Io => Self::EIO,
            crate::fs::FsError::NameTooLong => Self::ENAMETOOLONG,
            crate::fs::FsError::Loop => Self::ELOOP,
            crate::fs::FsError::Unsupported => Self::ENOTSUP,
            crate::fs::FsError::ReadOnly => Self::EROFS,
            crate::fs::FsError::NoSpace => Self::ENOSPC,
            crate::fs::FsError::IllegalSeek => Self::ESPIPE,
            crate::fs::FsError::NoDeviceOrAddress => Self::ENXIO,
        }
    }
}

/// Converts a typed syscall result into the Linux register return convention.
pub fn ret(result: SysResult<isize>) -> isize {
    match result {
        Ok(value) => value,
        Err(err) => -(err as isize),
    }
}
