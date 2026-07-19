use super::{PollEvents, PollWaitQueue, PollWaiter};
use crate::drivers::chardev::{CharDevice, UART};
use crate::mm::UserBuffer;
use crate::sync::{SpinNoIrqLock, UPIntrFreeCell};
#[cfg(target_arch = "loongarch64")]
use crate::task::suspend_current_and_run_next;
use crate::task::{
    SignalFlags, TaskControlBlock, block_current_task_no_schedule_unless_unmasked_signal,
    current_has_interrupting_signal, current_process_group_id, current_task, schedule,
    send_tty_signal_to_process_group, wakeup_task,
};
use alloc::collections::VecDeque;
use alloc::sync::Arc;
use alloc::vec::Vec;
use lazy_static::lazy_static;

const IGNCR: u32 = 0x0080;
const BRKINT: u32 = 0x0002;
const ICRNL: u32 = 0x0100;
const IXON: u32 = 0x0400;
const OPOST: u32 = 0x0001;
const ONLCR: u32 = 0x0004;
const CS8: u32 = 0x0030;
const CREAD: u32 = 0x0080;
const B38400: u32 = 0x000f;
const ISIG: u32 = 0x0001;
const ICANON: u32 = 0x0002;
const ECHO: u32 = 0x0008;
const ECHOE: u32 = 0x0010;
const ECHOK: u32 = 0x0020;
const ECHOCTL: u32 = 0x0200;
const ECHOKE: u32 = 0x0800;
const IEXTEN: u32 = 0x8000;

const VINTR: usize = 0;
const VQUIT: usize = 1;
const VERASE: usize = 2;
const VKILL: usize = 3;
const VEOF: usize = 4;
const VTIME: usize = 5;
const VMIN: usize = 6;
const VSTART: usize = 8;
const VSTOP: usize = 9;
const VSUSP: usize = 10;
const VEOL: usize = 11;
const VREPRINT: usize = 12;
const VDISCARD: usize = 13;
const VWERASE: usize = 14;
const VLNEXT: usize = 15;
const VEOL2: usize = 16;

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub(crate) struct LinuxTermios {
    pub(crate) c_iflag: u32,
    pub(crate) c_oflag: u32,
    pub(crate) c_cflag: u32,
    pub(crate) c_lflag: u32,
    pub(crate) c_line: u8,
    pub(crate) c_cc: [u8; 19],
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub(crate) struct LinuxTermios2 {
    pub(crate) c_iflag: u32,
    pub(crate) c_oflag: u32,
    pub(crate) c_cflag: u32,
    pub(crate) c_lflag: u32,
    pub(crate) c_line: u8,
    pub(crate) c_cc: [u8; 19],
    pub(crate) c_ispeed: u32,
    pub(crate) c_ospeed: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub(crate) struct LinuxTermio {
    pub(crate) c_iflag: u16,
    pub(crate) c_oflag: u16,
    pub(crate) c_cflag: u16,
    pub(crate) c_lflag: u16,
    pub(crate) c_line: u8,
    pub(crate) c_cc: [u8; 8],
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct LinuxWinsize {
    pub(crate) ws_row: u16,
    pub(crate) ws_col: u16,
    pub(crate) ws_xpixel: u16,
    pub(crate) ws_ypixel: u16,
}

#[derive(Clone, Copy, Debug)]
enum EchoAction {
    None,
    Byte(u8),
    Control(u8),
    ControlNewline(u8),
    Backspace,
    Newline,
}

#[derive(Clone, Copy, Debug)]
struct InputAction {
    echo: EchoAction,
    signal: Option<SignalFlags>,
    wake_readers: bool,
}

impl InputAction {
    const fn none() -> Self {
        Self {
            echo: EchoAction::None,
            signal: None,
            wake_readers: false,
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct ConsoleTtySettings {
    termios: LinuxTermios,
    winsize: LinuxWinsize,
    foreground_pgid: Option<usize>,
}

impl ConsoleTtySettings {
    fn new() -> Self {
        let mut c_cc = [0u8; 19];
        c_cc[VINTR] = 3;
        c_cc[VQUIT] = 28;
        c_cc[VERASE] = 127;
        c_cc[VKILL] = 21;
        c_cc[VEOF] = 4;
        c_cc[VTIME] = 0;
        c_cc[VMIN] = 1;
        c_cc[VSTART] = 17;
        c_cc[VSTOP] = 19;
        c_cc[VSUSP] = 26;
        c_cc[VEOL] = 0;
        c_cc[VREPRINT] = 18;
        c_cc[VDISCARD] = 15;
        c_cc[VWERASE] = 23;
        c_cc[VLNEXT] = 22;
        c_cc[VEOL2] = 0;

        Self {
            termios: LinuxTermios {
                c_iflag: BRKINT | ICRNL | IXON,
                c_oflag: OPOST | ONLCR,
                c_cflag: B38400 | CS8 | CREAD,
                c_lflag: ISIG | ICANON | ECHO | ECHOE | ECHOK | ECHOCTL | ECHOKE | IEXTEN,
                c_line: 0,
                c_cc,
            },
            winsize: LinuxWinsize {
                ws_row: 80,
                ws_col: 240,
                ws_xpixel: 0,
                ws_ypixel: 0,
            },
            foreground_pgid: None,
        }
    }
}

struct ConsoleTtyState {
    settings: ConsoleTtySettings,
    line_buf: Vec<u8>,
    read_buf: VecDeque<u8>,
    pending_eof: bool,
    read_wait_queue: VecDeque<Arc<TaskControlBlock>>,
}

impl ConsoleTtyState {
    fn new() -> Self {
        Self {
            settings: ConsoleTtySettings::new(),
            line_buf: Vec::new(),
            read_buf: VecDeque::new(),
            pending_eof: false,
            read_wait_queue: VecDeque::new(),
        }
    }

    fn ensure_foreground_pgid(&mut self, pgid: Option<usize>) {
        if self.settings.foreground_pgid.is_none() {
            self.settings.foreground_pgid = pgid;
        }
    }
}

struct ConsoleTty {
    state: UPIntrFreeCell<ConsoleTtyState>,
    input_drain_lock: SpinNoIrqLock<()>,
    poll_waiters: UPIntrFreeCell<PollWaitQueue>,
}

lazy_static! {
    static ref CONSOLE_TTY: ConsoleTty = ConsoleTty {
        state: unsafe { UPIntrFreeCell::new(ConsoleTtyState::new()) },
        input_drain_lock: SpinNoIrqLock::new(()),
        poll_waiters: unsafe { UPIntrFreeCell::new(PollWaitQueue::new()) },
    };
}

enum ReadAttempt {
    Data(Vec<u8>),
    Eof,
    Block,
}

pub(crate) fn console_tty_termios() -> LinuxTermios {
    CONSOLE_TTY
        .state
        .exclusive_session(|state| state.settings.termios)
}

pub(crate) fn console_tty_termios2() -> LinuxTermios2 {
    let termios = console_tty_termios();
    LinuxTermios2 {
        c_iflag: termios.c_iflag,
        c_oflag: termios.c_oflag,
        c_cflag: termios.c_cflag,
        c_lflag: termios.c_lflag,
        c_line: termios.c_line,
        c_cc: termios.c_cc,
        c_ispeed: 38400,
        c_ospeed: 38400,
    }
}

pub(crate) fn set_console_tty_termios(termios: LinuxTermios) {
    CONSOLE_TTY.state.exclusive_session(|state| {
        state.settings.termios = termios;
        state.line_buf.clear();
        state.read_buf.clear();
        state.pending_eof = false;
    });
}

pub(crate) fn set_console_tty_termios2(termios: LinuxTermios2) {
    set_console_tty_termios(LinuxTermios {
        c_iflag: termios.c_iflag,
        c_oflag: termios.c_oflag,
        c_cflag: termios.c_cflag,
        c_lflag: termios.c_lflag,
        c_line: termios.c_line,
        c_cc: termios.c_cc,
    });
}

pub(crate) fn console_tty_termio() -> LinuxTermio {
    termios_to_termio(console_tty_termios())
}

pub(crate) fn apply_console_tty_termio(termio: LinuxTermio) {
    CONSOLE_TTY.state.exclusive_session(|state| {
        apply_termio(&mut state.settings.termios, termio);
        state.line_buf.clear();
        state.read_buf.clear();
        state.pending_eof = false;
    });
}

pub(crate) fn console_tty_winsize() -> LinuxWinsize {
    CONSOLE_TTY
        .state
        .exclusive_session(|state| state.settings.winsize)
}

pub(crate) fn set_console_tty_winsize(winsize: LinuxWinsize) {
    CONSOLE_TTY
        .state
        .exclusive_session(|state| state.settings.winsize = winsize);
}

pub(crate) fn console_tty_foreground_pgid() -> usize {
    let current_pgid = current_process_group_id();
    CONSOLE_TTY.state.exclusive_session(|state| {
        state.ensure_foreground_pgid(current_pgid);
        state
            .settings
            .foreground_pgid
            .or(current_pgid)
            .unwrap_or_default()
    })
}

pub(crate) fn set_console_tty_foreground_pgid(pgid: usize) {
    CONSOLE_TTY
        .state
        .exclusive_session(|state| state.settings.foreground_pgid = Some(pgid));
}

pub(crate) fn console_tty_available_bytes() -> usize {
    console_tty_drain_uart();
    CONSOLE_TTY
        .state
        .exclusive_session(|state| state.read_buf.len() + usize::from(state.pending_eof))
}

pub(crate) fn console_tty_poll(events: PollEvents) -> PollEvents {
    console_tty_poll_with_wait(events, None)
}

pub(crate) fn console_tty_poll_with_wait(
    events: PollEvents,
    waiter: Option<&alloc::sync::Arc<PollWaiter>>,
) -> PollEvents {
    if !events.intersects(PollEvents::POLLIN | PollEvents::POLLPRI) {
        return PollEvents::empty();
    }
    CONSOLE_TTY
        .state
        .exclusive_session(|state| state.ensure_foreground_pgid(current_process_group_id()));
    if let Some(waiter) = waiter {
        CONSOLE_TTY
            .poll_waiters
            .exclusive_session(|waiters| waiters.register(waiter));
    }
    console_tty_drain_uart();
    let readable = CONSOLE_TTY
        .state
        .exclusive_session(|state| !state.read_buf.is_empty() || state.pending_eof);
    if readable {
        PollEvents::POLLIN
    } else {
        PollEvents::empty()
    }
}

pub(crate) fn console_tty_read(user_buf: UserBuffer) -> usize {
    let want_to_read = user_buf.len();
    if want_to_read == 0 {
        return 0;
    }
    CONSOLE_TTY
        .state
        .exclusive_session(|state| state.ensure_foreground_pgid(current_process_group_id()));

    loop {
        console_tty_drain_uart();
        let mut state = CONSOLE_TTY.state.exclusive_access();
        match try_read_buffered(&mut state, want_to_read) {
            ReadAttempt::Data(data) => {
                drop(state);
                let mut user_buf = user_buf;
                return user_buf.copy_from_slice(data.as_slice());
            }
            ReadAttempt::Eof => return 0,
            ReadAttempt::Block => {}
        }
        if let Some(task) = current_task() {
            state
                .read_wait_queue
                .retain(|waiter| !Arc::ptr_eq(waiter, &task));
        }
        if current_has_interrupting_signal() {
            return 0;
        }

        #[cfg(target_arch = "loongarch64")]
        if !crate::board::external_irq_available() {
            drop(state);
            suspend_current_and_run_next();
            continue;
        }
        let Some((task, task_cx_ptr)) = block_current_task_no_schedule_unless_unmasked_signal()
        else {
            return 0;
        };
        state.read_wait_queue.push_back(task);
        drop(state);
        schedule(task_cx_ptr);
    }
}

pub(crate) fn console_tty_drain_uart() {
    let _drain_guard = CONSOLE_TTY.input_drain_lock.lock();
    let mut should_signal = false;
    let mut echo_bytes = Vec::new();
    while let Some(ch) = UART.try_read() {
        let action = process_input(ch);
        append_echo(&mut echo_bytes, action.echo);
        if let Some(signal) = action.signal {
            signal_foreground_process_group(signal);
        }
        should_signal |= action.wake_readers;
    }
    if !echo_bytes.is_empty() {
        UART.write_bytes(&echo_bytes);
    }
    if should_signal {
        let task = CONSOLE_TTY
            .state
            .exclusive_session(|state| state.read_wait_queue.pop_front());
        let poll_waiters = CONSOLE_TTY
            .poll_waiters
            .exclusive_session(|waiters| waiters.drain());
        if let Some(task) = task {
            wakeup_task(task);
        }
        PollWaiter::wake_all(poll_waiters);
    }
}

fn try_read_buffered(state: &mut ConsoleTtyState, want_to_read: usize) -> ReadAttempt {
    if !state.read_buf.is_empty() {
        let count = want_to_read.min(state.read_buf.len());
        let mut data = Vec::with_capacity(count);
        for _ in 0..count {
            if let Some(ch) = state.read_buf.pop_front() {
                data.push(ch);
            }
        }
        return ReadAttempt::Data(data);
    }
    if state.pending_eof {
        state.pending_eof = false;
        return ReadAttempt::Eof;
    }
    ReadAttempt::Block
}

fn process_input(mut ch: u8) -> InputAction {
    let current_pgid = current_process_group_id();
    CONSOLE_TTY.state.exclusive_session(|state| {
        state.ensure_foreground_pgid(current_pgid);
        let termios = state.settings.termios;
        if ch == b'\r' {
            if has_iflag(termios, IGNCR) {
                return InputAction::none();
            }
            if has_iflag(termios, ICRNL) {
                ch = b'\n';
            }
        }

        if has_lflag(termios, ISIG) {
            if ch == special_char(termios, VINTR) {
                state.line_buf.clear();
                state.read_buf.clear();
                state.pending_eof = false;
                return InputAction {
                    echo: signal_echo(termios, ch),
                    signal: Some(SignalFlags::SIGINT),
                    wake_readers: true,
                };
            }
            if ch == special_char(termios, VQUIT) {
                state.line_buf.clear();
                state.read_buf.clear();
                state.pending_eof = false;
                return InputAction {
                    echo: signal_echo(termios, ch),
                    signal: Some(SignalFlags::SIGQUIT),
                    wake_readers: true,
                };
            }
        }

        if !has_lflag(termios, ICANON) {
            state.read_buf.push_back(ch);
            return InputAction {
                echo: echo_char(termios, ch),
                signal: None,
                wake_readers: true,
            };
        }

        if ch == special_char(termios, VEOF) {
            if state.line_buf.is_empty() {
                state.pending_eof = true;
            } else {
                flush_line_buf(state);
            }
            return InputAction {
                echo: EchoAction::None,
                signal: None,
                wake_readers: true,
            };
        }
        if ch == special_char(termios, VERASE) {
            if state.line_buf.pop().is_some() {
                return InputAction {
                    echo: erase_echo(termios),
                    signal: None,
                    wake_readers: false,
                };
            }
            return InputAction::none();
        }
        if ch == special_char(termios, VKILL) {
            if !state.line_buf.is_empty() {
                state.line_buf.clear();
                return InputAction {
                    echo: kill_echo(termios),
                    signal: None,
                    wake_readers: false,
                };
            }
            return InputAction::none();
        }

        state.line_buf.push(ch);
        if is_eol(termios, ch) {
            flush_line_buf(state);
            InputAction {
                echo: echo_char(termios, ch),
                signal: None,
                wake_readers: true,
            }
        } else {
            InputAction {
                echo: echo_char(termios, ch),
                signal: None,
                wake_readers: false,
            }
        }
    })
}

fn signal_foreground_process_group(signal: SignalFlags) {
    let current_pgid = current_process_group_id();
    let pgid = CONSOLE_TTY.state.exclusive_session(|state| {
        state.ensure_foreground_pgid(current_pgid);
        state.settings.foreground_pgid.or(current_pgid)
    });
    if let Some(pgid) = pgid {
        send_tty_signal_to_process_group(pgid, signal);
    }
}

fn flush_line_buf(state: &mut ConsoleTtyState) {
    for ch in state.line_buf.drain(..) {
        state.read_buf.push_back(ch);
    }
}

fn has_iflag(termios: LinuxTermios, flag: u32) -> bool {
    termios.c_iflag & flag != 0
}

fn has_lflag(termios: LinuxTermios, flag: u32) -> bool {
    termios.c_lflag & flag != 0
}

fn special_char(termios: LinuxTermios, index: usize) -> u8 {
    termios.c_cc[index]
}

fn is_eol(termios: LinuxTermios, ch: u8) -> bool {
    ch == b'\n'
        || ch == special_char(termios, VEOL)
        || (has_lflag(termios, IEXTEN) && ch == special_char(termios, VEOL2))
}

fn echo_char(termios: LinuxTermios, ch: u8) -> EchoAction {
    if !has_lflag(termios, ECHO) {
        return EchoAction::None;
    }
    if ch == b'\n' || ch == b'\r' {
        return EchoAction::Newline;
    }
    if ch == b' ' || ch.is_ascii_graphic() {
        return EchoAction::Byte(ch);
    }
    if ch.is_ascii_control() && has_lflag(termios, ECHOCTL) {
        EchoAction::Control(ch)
    } else {
        EchoAction::None
    }
}

fn signal_echo(termios: LinuxTermios, ch: u8) -> EchoAction {
    if !has_lflag(termios, ECHO) {
        return EchoAction::None;
    }
    if has_lflag(termios, ECHOCTL) {
        EchoAction::ControlNewline(ch)
    } else {
        EchoAction::Newline
    }
}

fn erase_echo(termios: LinuxTermios) -> EchoAction {
    if has_lflag(termios, ECHO) && has_lflag(termios, ECHOE) {
        EchoAction::Backspace
    } else {
        EchoAction::None
    }
}

fn kill_echo(termios: LinuxTermios) -> EchoAction {
    if has_lflag(termios, ECHO) && has_lflag(termios, ECHOK) {
        EchoAction::Newline
    } else {
        EchoAction::None
    }
}

fn append_echo(output: &mut Vec<u8>, action: EchoAction) {
    match action {
        EchoAction::None => {}
        EchoAction::Byte(ch) => output.push(ch),
        EchoAction::Control(ch) => output.extend_from_slice(&[b'^', ch ^ 0x40]),
        EchoAction::ControlNewline(ch) => output.extend_from_slice(&[b'^', ch ^ 0x40, b'\n']),
        EchoAction::Backspace => output.extend_from_slice(&[8, b' ', 8]),
        EchoAction::Newline => output.push(b'\n'),
    }
}

fn termios_to_termio(termios: LinuxTermios) -> LinuxTermio {
    let mut c_cc = [0u8; 8];
    c_cc.copy_from_slice(&termios.c_cc[..8]);
    LinuxTermio {
        c_iflag: termios.c_iflag as u16,
        c_oflag: termios.c_oflag as u16,
        c_cflag: termios.c_cflag as u16,
        c_lflag: termios.c_lflag as u16,
        c_line: termios.c_line,
        c_cc,
    }
}

fn apply_termio(termios: &mut LinuxTermios, termio: LinuxTermio) {
    termios.c_iflag = (termios.c_iflag & !0xffff) | termio.c_iflag as u32;
    termios.c_oflag = (termios.c_oflag & !0xffff) | termio.c_oflag as u32;
    termios.c_cflag = (termios.c_cflag & !0xffff) | termio.c_cflag as u32;
    termios.c_lflag = (termios.c_lflag & !0xffff) | termio.c_lflag as u32;
    termios.c_line = termio.c_line;
    termios.c_cc[..8].copy_from_slice(&termio.c_cc);
}
