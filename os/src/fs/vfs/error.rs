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
