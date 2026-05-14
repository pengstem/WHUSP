use super::status_flags::StatusFlagsCell;
use super::{
    File, FileStat, FsResult, OpenFlags, PollEvents, S_IFCHR, console_tty_poll, console_tty_read,
};
use crate::drivers::chardev::{CharDevice, UART};
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
    fn as_any(&self) -> &dyn core::any::Any {
        self
    }

    fn readable(&self) -> bool {
        true
    }
    fn writable(&self) -> bool {
        false
    }
    fn read(&self, user_buf: UserBuffer) -> usize {
        console_tty_read(user_buf)
    }
    fn write(&self, _user_buf: UserBuffer) -> usize {
        panic!("Cannot write to stdin!");
    }
    fn poll(&self, events: PollEvents) -> PollEvents {
        console_tty_poll(events)
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
    fn as_any(&self) -> &dyn core::any::Any {
        self
    }

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
