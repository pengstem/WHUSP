#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum FsError {
    NotFound,
    NotDir,
    IsDir,
    AlreadyExists,
    PermissionDenied,
    AccessDenied,
    InvalidInput,
    NotEmpty,
    Busy,
    TextBusy,
    CrossDevice,
    Io,
    NameTooLong,
    Loop,
    Unsupported,
    ReadOnly,
    NoSpace,
    IllegalSeek,
    NoDeviceOrAddress,
}

pub(crate) type FsResult<T = ()> = Result<T, FsError>;

impl From<FsError> for crate::uapi::errno::Errno {
    /// Maps VFS-layer errors onto Linux-visible errno values.
    fn from(error: FsError) -> Self {
        match error {
            FsError::NotFound => Self::ENOENT,
            FsError::NotDir => Self::ENOTDIR,
            FsError::IsDir => Self::EISDIR,
            FsError::AlreadyExists => Self::EEXIST,
            FsError::PermissionDenied => Self::EPERM,
            FsError::AccessDenied => Self::EACCES,
            FsError::InvalidInput => Self::EINVAL,
            FsError::NotEmpty => Self::ENOTEMPTY,
            FsError::Busy => Self::EBUSY,
            FsError::TextBusy => Self::ETXTBSY,
            FsError::CrossDevice => Self::EXDEV,
            FsError::Io => Self::EIO,
            FsError::NameTooLong => Self::ENAMETOOLONG,
            FsError::Loop => Self::ELOOP,
            FsError::Unsupported => Self::ENOTSUP,
            FsError::ReadOnly => Self::EROFS,
            FsError::NoSpace => Self::ENOSPC,
            FsError::IllegalSeek => Self::ESPIPE,
            FsError::NoDeviceOrAddress => Self::ENXIO,
        }
    }
}
