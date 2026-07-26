use super::status_flags::StatusFlagsCell;
use super::{
    File, FileStat, FsResult, OpenFlags, PollEvents, PollWaiter, S_IFCHR, TtyId, console_tty_poll,
    console_tty_poll_with_wait, console_tty_read, tty_job_control_check,
};
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
    fn poll_with_wait(
        &self,
        events: PollEvents,
        waiter: Option<&alloc::sync::Arc<PollWaiter>>,
    ) -> PollEvents {
        console_tty_poll_with_wait(events, waiter)
    }
    fn stat(&self) -> FsResult<FileStat> {
        Ok(FileStat::with_mode(S_IFCHR | 0o666))
    }
    fn check_read(&self, _len: usize) -> FsResult {
        tty_job_control_check(TtyId::Console, false)
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
    fn tty_id(&self) -> Option<TtyId> {
        Some(TtyId::Console)
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
        UART.write_byte_slices(user_buf.buffers.iter().map(|buffer| &(**buffer)[..]));
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
    fn check_write(&self, _len: usize, _append: bool) -> FsResult {
        tty_job_control_check(TtyId::Console, true)
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
    fn tty_id(&self) -> Option<TtyId> {
        Some(TtyId::Console)
    }
}
