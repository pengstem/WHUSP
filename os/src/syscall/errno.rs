#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(isize)]
#[allow(dead_code)]
pub enum SysError {
    EPERM = 1,
    ENOENT = 2,
    ESRCH = 3,
    EINTR = 4,
    EIO = 5,
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
    ENOSPC = 28,
    ESPIPE = 29,
    EROFS = 30,
    ERANGE = 34,
    EDEADLK = 35,
    ENAMETOOLONG = 36,
    ENOSYS = 38,
    ENOTEMPTY = 39,
    ELOOP = 40,
    ENOTSUP = 95,
    ETIMEDOUT = 110,
}

pub type SysResult<T = isize> = Result<T, SysError>;

impl From<crate::fs::FsError> for SysError {
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
            crate::fs::FsError::CrossDevice => Self::EXDEV,
            crate::fs::FsError::Io => Self::EIO,
            crate::fs::FsError::NameTooLong => Self::ENAMETOOLONG,
            crate::fs::FsError::Loop => Self::ELOOP,
            crate::fs::FsError::Unsupported => Self::ENOTSUP,
            crate::fs::FsError::ReadOnly => Self::EROFS,
            crate::fs::FsError::NoSpace => Self::ENOSPC,
            crate::fs::FsError::IllegalSeek => Self::ESPIPE,
        }
    }
}

pub fn ret(result: SysResult<isize>) -> isize {
    match result {
        Ok(value) => value,
        Err(err) => -(err as isize),
    }
}
