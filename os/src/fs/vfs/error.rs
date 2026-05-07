#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum FsError {
    NotFound,
    NotDir,
    IsDir,
    AlreadyExists,
    PermissionDenied,
    InvalidInput,
    NotEmpty,
    Busy,
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
