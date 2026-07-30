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
pub enum Errno {
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
    EKEYEXPIRED = 127,
    EKEYREVOKED = 128,
}

pub type KResult<T = isize> = Result<T, Errno>;
