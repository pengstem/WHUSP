use super::dirent::{DT_CHR, DT_DIR, LINUX_DIRENT64_ALIGN, LINUX_DIRENT64_HEADER_SIZE};
use super::status_flags::StatusFlagsCell;
use super::{File, FileStat, FsError, FsResult, OpenFlags, PollEvents, S_IFCHR, S_IFDIR};
use crate::drivers::chardev::{CharDevice, UART};
use crate::mm::UserBuffer;
use crate::sync::UPIntrFreeCell;
use alloc::sync::Arc;
use alloc::vec;

const DEVFS_DEV: u64 = 0x646576;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DevNode {
    Root,
    Null,
    Zero,
    Tty,
    TtyS0,
    Rtc,
}

impl DevNode {
    fn ino(self) -> u64 {
        match self {
            Self::Root => 1,
            Self::Null => 2,
            Self::Zero => 3,
            Self::Tty => 4,
            Self::TtyS0 => 5,
            Self::Rtc => 6,
        }
    }

    fn rdev(self) -> u64 {
        match self {
            Self::Root => 0,
            Self::Null => linux_makedev(1, 3),
            Self::Zero => linux_makedev(1, 5),
            Self::Tty => linux_makedev(5, 0),
            Self::TtyS0 => linux_makedev(4, 64),
            Self::Rtc => linux_makedev(253, 0),
        }
    }

    fn is_tty(self) -> bool {
        matches!(self, Self::Tty | Self::TtyS0)
    }
}

struct DevFsFile {
    node: DevNode,
    readable: bool,
    writable: bool,
    offset: UPIntrFreeCell<usize>,
    status_flags: StatusFlagsCell,
}

impl DevFsFile {
    fn new(node: DevNode, readable: bool, writable: bool, status_flags: OpenFlags) -> Self {
        Self {
            node,
            readable,
            writable,
            offset: unsafe { UPIntrFreeCell::new(0) },
            status_flags: StatusFlagsCell::new(status_flags),
        }
    }
}

struct DevDirEntry {
    node: DevNode,
    name: &'static [u8],
    dtype: u8,
}

const DEV_DIR_ENTRIES: [DevDirEntry; 7] = [
    DevDirEntry {
        node: DevNode::Root,
        name: b".",
        dtype: DT_DIR,
    },
    DevDirEntry {
        node: DevNode::Root,
        name: b"..",
        dtype: DT_DIR,
    },
    DevDirEntry {
        node: DevNode::Null,
        name: b"null",
        dtype: DT_CHR,
    },
    DevDirEntry {
        node: DevNode::Zero,
        name: b"zero",
        dtype: DT_CHR,
    },
    DevDirEntry {
        node: DevNode::Tty,
        name: b"tty",
        dtype: DT_CHR,
    },
    DevDirEntry {
        node: DevNode::TtyS0,
        name: b"ttyS0",
        dtype: DT_CHR,
    },
    DevDirEntry {
        node: DevNode::Rtc,
        name: b"rtc",
        dtype: DT_CHR,
    },
];

fn linux_makedev(major: u64, minor: u64) -> u64 {
    (minor & 0xff) | ((major & 0xfff) << 8) | ((minor & !0xff) << 12) | ((major & !0xfff) << 32)
}

use super::align_up;

fn lookup_absolute(path: &str) -> Option<DevNode> {
    // UNFINISHED: This lightweight devfs is not a mountable filesystem yet.
    // Only absolute /dev paths and explicit /dev directory fds are handled.
    match path {
        "/dev" | "/dev/" => Some(DevNode::Root),
        "/dev/null" => Some(DevNode::Null),
        "/dev/zero" => Some(DevNode::Zero),
        "/dev/tty" => Some(DevNode::Tty),
        "/dev/ttyS0" => Some(DevNode::TtyS0),
        "/dev/rtc" | "/dev/rtc0" | "/dev/misc/rtc" => Some(DevNode::Rtc),
        _ => None,
    }
}

fn lookup_child(path: &str) -> Option<DevNode> {
    match path {
        "." | ".." => Some(DevNode::Root),
        "null" => Some(DevNode::Null),
        "zero" => Some(DevNode::Zero),
        "tty" => Some(DevNode::Tty),
        "ttyS0" => Some(DevNode::TtyS0),
        "rtc" | "rtc0" => Some(DevNode::Rtc),
        _ => None,
    }
}

fn open_node(node: DevNode, flags: OpenFlags) -> FsResult<Arc<dyn File + Send + Sync>> {
    if flags.contains(OpenFlags::CREATE | OpenFlags::EXCL) {
        return Err(FsError::AlreadyExists);
    }
    if node == DevNode::Root {
        if !flags.can_open_directory() {
            return Err(FsError::IsDir);
        }
        return Ok(Arc::new(DevFsFile::new(
            node,
            false,
            false,
            OpenFlags::file_status_flags(flags),
        )));
    }
    if flags.contains(OpenFlags::DIRECTORY) {
        return Err(FsError::NotDir);
    }
    let (readable, writable) = flags.read_write();
    Ok(Arc::new(DevFsFile::new(
        node,
        readable,
        writable,
        OpenFlags::file_status_flags(flags),
    )))
}

pub(crate) fn open(path: &str, flags: OpenFlags) -> FsResult<Option<Arc<dyn File + Send + Sync>>> {
    lookup_absolute(path)
        .map(|node| open_node(node, flags))
        .transpose()
}

pub(crate) fn open_child(
    path: &str,
    flags: OpenFlags,
) -> FsResult<Option<Arc<dyn File + Send + Sync>>> {
    if path.contains('/') {
        return Ok(None);
    }
    lookup_child(path)
        .map(|node| open_node(node, flags))
        .transpose()
}

fn stat_node(node: DevNode) -> FileStat {
    let mode = if node == DevNode::Root {
        S_IFDIR | 0o755
    } else {
        S_IFCHR | 0o666
    };
    let mut stat = FileStat::with_mode(mode);
    stat.dev = DEVFS_DEV;
    stat.ino = node.ino();
    stat.rdev = node.rdev();
    stat.nlink = if node == DevNode::Root { 2 } else { 1 };
    stat.blocks = 0;
    stat
}

pub(crate) fn stat(path: &str) -> Option<FileStat> {
    Some(stat_node(lookup_absolute(path)?))
}

pub(crate) fn stat_child(path: &str) -> Option<FileStat> {
    if path.contains('/') {
        return None;
    }
    Some(stat_node(lookup_child(path)?))
}

fn read_console(user_buf: UserBuffer) -> usize {
    let want_to_read = user_buf.len();
    if want_to_read == 0 {
        return 0;
    }

    let mut buf_iter = user_buf.into_iter();
    let Some(byte_ref) = buf_iter.next() else {
        return 0;
    };
    unsafe {
        byte_ref.write_volatile(UART.read());
    }

    let mut already_read = 1usize;
    while already_read < want_to_read {
        let Some(ch) = UART.try_read() else {
            break;
        };
        let Some(byte_ref) = buf_iter.next() else {
            break;
        };
        unsafe {
            byte_ref.write_volatile(ch);
        }
        already_read += 1;
    }
    already_read
}

fn write_console(user_buf: UserBuffer) -> usize {
    let len = user_buf.len();
    for buffer in user_buf.buffers.iter() {
        for byte in buffer.iter() {
            UART.write(*byte);
        }
    }
    len
}

fn read_zero(user_buf: UserBuffer) -> usize {
    let len = user_buf.len();
    for byte_ref in user_buf {
        unsafe {
            byte_ref.write_volatile(0);
        }
    }
    len
}

fn copy_dirents(entries_offset: &mut usize, user_buf: UserBuffer) -> FsResult<isize> {
    let mut kernel_buf = vec![0u8; user_buf.len()];
    let mut written = 0usize;

    while *entries_offset < DEV_DIR_ENTRIES.len() {
        let entry = &DEV_DIR_ENTRIES[*entries_offset];
        let d_reclen = align_up(
            LINUX_DIRENT64_HEADER_SIZE + entry.name.len() + 1,
            LINUX_DIRENT64_ALIGN,
        );
        if d_reclen > kernel_buf.len().saturating_sub(written) {
            if written == 0 {
                return Err(FsError::InvalidInput);
            }
            break;
        }

        let next_offset = *entries_offset + 1;
        let entry_buf = &mut kernel_buf[written..written + d_reclen];
        entry_buf.fill(0);
        entry_buf[0..8].copy_from_slice(&entry.node.ino().to_ne_bytes());
        entry_buf[8..16].copy_from_slice(&(next_offset as i64).to_ne_bytes());
        entry_buf[16..18].copy_from_slice(&(d_reclen as u16).to_ne_bytes());
        entry_buf[18] = entry.dtype;
        entry_buf[LINUX_DIRENT64_HEADER_SIZE..LINUX_DIRENT64_HEADER_SIZE + entry.name.len()]
            .copy_from_slice(entry.name);

        written += d_reclen;
        *entries_offset = next_offset;
    }

    if written == 0 {
        return Ok(0);
    }
    for (idx, byte_ref) in user_buf.into_iter().take(written).enumerate() {
        unsafe {
            *byte_ref = kernel_buf[idx];
        }
    }
    Ok(written as isize)
}

impl File for DevFsFile {
    fn readable(&self) -> bool {
        self.readable
    }

    fn writable(&self) -> bool {
        self.writable
    }

    fn read(&self, user_buf: UserBuffer) -> usize {
        match self.node {
            DevNode::Root | DevNode::Null | DevNode::Rtc => 0,
            DevNode::Zero => read_zero(user_buf),
            DevNode::Tty | DevNode::TtyS0 => read_console(user_buf),
        }
    }

    fn write(&self, user_buf: UserBuffer) -> usize {
        match self.node {
            DevNode::Root | DevNode::Rtc => 0,
            DevNode::Null | DevNode::Zero => user_buf.len(),
            DevNode::Tty | DevNode::TtyS0 => write_console(user_buf),
        }
    }

    fn poll(&self, events: PollEvents) -> PollEvents {
        match self.node {
            DevNode::Tty | DevNode::TtyS0 => {
                let mut ready = PollEvents::empty();
                if events.intersects(PollEvents::POLLIN | PollEvents::POLLPRI) && UART.has_input() {
                    ready |= PollEvents::POLLIN;
                }
                if events.contains(PollEvents::POLLOUT) && self.writable {
                    ready |= PollEvents::POLLOUT;
                }
                ready
            }
            _ => {
                let mut ready = PollEvents::empty();
                if events.intersects(PollEvents::POLLIN | PollEvents::POLLPRI) && self.readable {
                    ready |= PollEvents::POLLIN;
                }
                if events.contains(PollEvents::POLLOUT) && self.writable {
                    ready |= PollEvents::POLLOUT;
                }
                ready
            }
        }
    }

    fn stat(&self) -> FileStat {
        stat_node(self.node)
    }

    fn status_flags(&self) -> OpenFlags {
        self.status_flags.get()
    }

    fn set_status_flags(&self, flags: OpenFlags) {
        self.status_flags.set(flags);
    }

    fn read_dirent64(&self, user_buf: UserBuffer) -> FsResult<isize> {
        if self.node != DevNode::Root {
            return Err(FsError::NotDir);
        }
        let mut offset = self.offset.exclusive_access();
        copy_dirents(&mut *offset, user_buf)
    }

    fn is_tty(&self) -> bool {
        self.node.is_tty()
    }

    fn is_rtc(&self) -> bool {
        self.node == DevNode::Rtc
    }

    fn is_devfs_dir(&self) -> bool {
        self.node == DevNode::Root
    }
}
