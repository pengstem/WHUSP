use super::status_flags::StatusFlagsCell;
use super::{File, FileStat, FsResult, OpenFlags, PollEvents, S_IFCHR};
use crate::drivers::chardev::CharDevice;
use crate::drivers::chardev::UART;
use crate::mm::UserBuffer;

pub struct Stdin {
    status_flags: StatusFlagsCell,
}

pub struct Stdout {
    status_flags: StatusFlagsCell,
}

impl Stdin {
    pub fn new() -> Self {
        Self {
            status_flags: StatusFlagsCell::new(OpenFlags::RDONLY),
        }
    }
}

impl Stdout {
    pub fn new() -> Self {
        Self {
            status_flags: StatusFlagsCell::new(OpenFlags::WRONLY),
        }
    }
}

impl File for Stdin {
    fn readable(&self) -> bool {
        true
    }
    fn writable(&self) -> bool {
        false
    }
    fn read(&self, user_buf: UserBuffer) -> usize {
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
    fn write(&self, _user_buf: UserBuffer) -> usize {
        panic!("Cannot write to stdin!");
    }
    fn poll(&self, events: PollEvents) -> PollEvents {
        if events.intersects(PollEvents::POLLIN | PollEvents::POLLPRI) && UART.has_input() {
            PollEvents::POLLIN
        } else {
            PollEvents::empty()
        }
    }
    fn stat(&self) -> FsResult<FileStat> {
        Ok(FileStat::with_mode(S_IFCHR | 0o666))
    }
    fn status_flags(&self) -> OpenFlags {
        self.status_flags.get()
    }
    fn set_status_flags(&self, flags: OpenFlags) {
        self.status_flags.set(flags);
    }
    fn is_tty(&self) -> bool {
        true
    }
}

impl File for Stdout {
    fn readable(&self) -> bool {
        false
    }
    fn writable(&self) -> bool {
        true
    }
    fn read(&self, _user_buf: UserBuffer) -> usize {
        panic!("Cannot read from stdout!");
    }
    fn write(&self, user_buf: UserBuffer) -> usize {
        let len = user_buf.len();
        for buffer in user_buf.buffers.iter() {
            for byte in buffer.iter() {
                UART.write(*byte);
            }
        }
        len
    }
    fn poll(&self, events: PollEvents) -> PollEvents {
        if events.contains(PollEvents::POLLOUT) {
            PollEvents::POLLOUT
        } else {
            PollEvents::empty()
        }
    }
    fn stat(&self) -> FsResult<FileStat> {
        Ok(FileStat::with_mode(S_IFCHR | 0o666))
    }
    fn status_flags(&self) -> OpenFlags {
        self.status_flags.get()
    }
    fn set_status_flags(&self, flags: OpenFlags) {
        self.status_flags.set(flags);
    }
    fn is_tty(&self) -> bool {
        true
    }
}
