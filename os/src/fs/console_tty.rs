use super::{PollEvents, PollWaitQueue, PollWaiter};
use crate::drivers::chardev::{CharDevice, UART};
use crate::mm::UserBuffer;
use crate::sync::{SpinNoIrqLock, UPIntrFreeCell};
#[cfg(target_arch = "loongarch64")]
use crate::task::suspend_current_and_run_next;
use crate::task::{
    SignalFlags, TaskControlBlock, block_current_task_no_schedule_unless_unmasked_signal,
    current_has_interrupting_signal, current_process, current_process_group_id, current_task,
    processes_snapshot, schedule, send_tty_signal_to_process_group, wakeup_task,
};
use alloc::collections::{BTreeMap, VecDeque};
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
const TOSTOP: u32 = 0x0100;
const NOFLSH: u32 = 0x0080;

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
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
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

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub(crate) enum TtyId {
    Console,
    Pty(u32),
}

impl TtyId {
    pub(crate) fn proc_tty_nr(self) -> i32 {
        match self {
            Self::Console => (4 << 8) | 64,
            Self::Pty(id) => (136 << 8) | id as i32,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct TtyControlState {
    pub(crate) session: Option<usize>,
    pub(crate) foreground_pgid: Option<usize>,
    pub(crate) hung_up: bool,
}

#[derive(Clone, Copy, Debug)]
struct ConsoleTtySettings {
    termios: LinuxTermios,
    winsize: LinuxWinsize,
    control: TtyControlState,
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
            control: TtyControlState {
                session: None,
                foreground_pgid: None,
                hung_up: false,
            },
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
        if self.settings.control.foreground_pgid.is_none() {
            self.settings.control.foreground_pgid = pgid;
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
    static ref PTY_TTY_SETTINGS: UPIntrFreeCell<BTreeMap<u32, ConsoleTtySettings>> =
        unsafe { UPIntrFreeCell::new(BTreeMap::new()) };
    static ref SESSION_TTYS: UPIntrFreeCell<BTreeMap<usize, TtyId>> =
        unsafe { UPIntrFreeCell::new(BTreeMap::new()) };
}

enum ReadAttempt {
    Data(Vec<u8>),
    Eof,
    Block,
}

fn with_tty_settings<R>(id: TtyId, f: impl FnOnce(&ConsoleTtySettings) -> R) -> Option<R> {
    match id {
        TtyId::Console => Some(
            CONSOLE_TTY
                .state
                .exclusive_session(|state| f(&state.settings)),
        ),
        TtyId::Pty(id) => PTY_TTY_SETTINGS.exclusive_session(|settings| settings.get(&id).map(f)),
    }
}

fn with_tty_settings_mut<R>(id: TtyId, f: impl FnOnce(&mut ConsoleTtySettings) -> R) -> Option<R> {
    match id {
        TtyId::Console => Some(
            CONSOLE_TTY
                .state
                .exclusive_session(|state| f(&mut state.settings)),
        ),
        TtyId::Pty(id) => {
            PTY_TTY_SETTINGS.exclusive_session(|settings| settings.get_mut(&id).map(f))
        }
    }
}

pub(crate) fn register_pty_tty(id: u32) {
    PTY_TTY_SETTINGS.exclusive_session(|settings| {
        settings.entry(id).or_insert_with(ConsoleTtySettings::new);
    });
}

pub(crate) fn unregister_pty_tty(id: u32) {
    tty_hangup(TtyId::Pty(id));
    PTY_TTY_SETTINGS.exclusive_session(|settings| {
        settings.remove(&id);
    });
}

pub(crate) fn tty_control_state(id: TtyId) -> Option<TtyControlState> {
    with_tty_settings(id, |settings| settings.control)
}

pub(crate) fn tty_for_session(sid: usize) -> Option<TtyId> {
    SESSION_TTYS.exclusive_session(|sessions| sessions.get(&sid).copied())
}

pub(crate) fn tty_attach(
    id: TtyId,
    sid: usize,
    foreground_pgid: usize,
    force: bool,
) -> super::FsResult {
    let mut sessions = SESSION_TTYS.exclusive_access();
    if sessions.get(&sid).is_some_and(|existing| *existing != id) {
        return Err(super::FsError::PermissionDenied);
    }
    let control = tty_control_state(id).ok_or(super::FsError::NoDeviceOrAddress)?;
    if let Some(owner) = control.session
        && owner != sid
        && !force
    {
        return Err(super::FsError::Busy);
    }
    if let Some(owner) = control.session
        && owner != sid
    {
        sessions.remove(&owner);
    }
    with_tty_settings_mut(id, |settings| {
        settings.control.session = Some(sid);
        settings.control.foreground_pgid = Some(foreground_pgid);
        settings.control.hung_up = false;
    })
    .ok_or(super::FsError::NoDeviceOrAddress)?;
    sessions.insert(sid, id);
    Ok(())
}

pub(crate) fn tty_release(id: TtyId, sid: usize) -> super::FsResult<Option<usize>> {
    // Keep the session-to-TTY map and the TTY owner fields under one
    // serialization boundary so concurrent release/acquire cannot leave only
    // one side of the relationship installed.
    let mut sessions = SESSION_TTYS.exclusive_access();
    let control = tty_control_state(id).ok_or(super::FsError::NoDeviceOrAddress)?;
    if control.session != Some(sid) || sessions.get(&sid) != Some(&id) {
        return Err(super::FsError::NoDeviceOrAddress);
    }
    let foreground = control.foreground_pgid;
    with_tty_settings_mut(id, |settings| {
        settings.control.session = None;
        settings.control.foreground_pgid = None;
    })
    .ok_or(super::FsError::NoDeviceOrAddress)?;
    sessions.remove(&sid);
    Ok(foreground)
}

pub(crate) fn tty_hangup(id: TtyId) {
    let mut sessions = SESSION_TTYS.exclusive_access();
    let Some(control) = tty_control_state(id) else {
        return;
    };
    if let Some(sid) = control.session {
        if sessions.get(&sid) == Some(&id) {
            sessions.remove(&sid);
        }
    }
    with_tty_settings_mut(id, |settings| {
        settings.control.session = None;
        settings.control.foreground_pgid = None;
        settings.control.hung_up = true;
    });
    drop(sessions);
    if let Some(pgid) = control.foreground_pgid {
        send_tty_signal_to_process_group(pgid, SignalFlags::SIGHUP);
        send_tty_signal_to_process_group(pgid, SignalFlags::SIGCONT);
    }
}

pub(crate) fn tty_detach_session(sid: usize, hangup: bool) {
    let Some(id) = tty_for_session(sid) else {
        return;
    };
    if hangup {
        tty_hangup(id);
    } else {
        let _ = tty_release(id, sid);
    }
}

pub(crate) fn tty_termios(id: TtyId) -> Option<LinuxTermios> {
    with_tty_settings(id, |settings| settings.termios)
}

pub(crate) fn tty_termios2(id: TtyId) -> Option<LinuxTermios2> {
    tty_termios(id).map(|termios| LinuxTermios2 {
        c_iflag: termios.c_iflag,
        c_oflag: termios.c_oflag,
        c_cflag: termios.c_cflag,
        c_lflag: termios.c_lflag,
        c_line: termios.c_line,
        c_cc: termios.c_cc,
        c_ispeed: 38400,
        c_ospeed: 38400,
    })
}

pub(crate) fn set_tty_termios(id: TtyId, termios: LinuxTermios) -> bool {
    with_tty_settings_mut(id, |settings| settings.termios = termios).is_some()
}

pub(crate) fn set_tty_termios2(id: TtyId, termios: LinuxTermios2) -> bool {
    set_tty_termios(
        id,
        LinuxTermios {
            c_iflag: termios.c_iflag,
            c_oflag: termios.c_oflag,
            c_cflag: termios.c_cflag,
            c_lflag: termios.c_lflag,
            c_line: termios.c_line,
            c_cc: termios.c_cc,
        },
    )
}

pub(crate) fn tty_termio(id: TtyId) -> Option<LinuxTermio> {
    tty_termios(id).map(termios_to_termio)
}

pub(crate) fn apply_tty_termio(id: TtyId, termio: LinuxTermio) -> bool {
    with_tty_settings_mut(id, |settings| apply_termio(&mut settings.termios, termio)).is_some()
}

pub(crate) fn tty_winsize(id: TtyId) -> Option<LinuxWinsize> {
    with_tty_settings(id, |settings| settings.winsize)
}

pub(crate) fn set_tty_winsize(id: TtyId, winsize: LinuxWinsize) -> bool {
    let mut foreground = None;
    let changed = with_tty_settings_mut(id, |settings| {
        if settings.winsize == winsize {
            return false;
        }
        settings.winsize = winsize;
        foreground = settings.control.foreground_pgid;
        true
    })
    .unwrap_or(false);
    if changed && let Some(pgid) = foreground {
        send_tty_signal_to_process_group(pgid, SignalFlags::SIGWINCH);
    }
    changed
}

pub(crate) fn set_tty_foreground_pgid(id: TtyId, sid: usize, pgid: usize) -> super::FsResult {
    let control = tty_control_state(id).ok_or(super::FsError::NoDeviceOrAddress)?;
    if control.session != Some(sid) || control.hung_up {
        return Err(super::FsError::NoDeviceOrAddress);
    }
    with_tty_settings_mut(id, |settings| {
        settings.control.foreground_pgid = Some(pgid);
    })
    .ok_or(super::FsError::NoDeviceOrAddress)?;
    Ok(())
}

fn process_group_is_orphaned(pgid: usize, sid: usize) -> bool {
    let members: Vec<_> = processes_snapshot()
        .into_iter()
        .filter(|process| {
            !process.is_zombie()
                && process.process_group_id() == pgid
                && process.session_id() == sid
        })
        .collect();
    !members.is_empty()
        && members.iter().all(|member| {
            member.parent_process().is_none_or(|parent| {
                parent.process_group_id() == pgid || parent.session_id() != sid
            })
        })
}

pub(crate) fn tty_job_control_check(id: TtyId, write: bool) -> super::FsResult {
    let process = current_process();
    let sid = process.session_id();
    let pgid = process.process_group_id();
    let Some(control) = tty_control_state(id) else {
        return Err(super::FsError::NoDeviceOrAddress);
    };
    if control.hung_up {
        return Err(super::FsError::Io);
    }
    if process.controlling_tty_detached()
        || control.session != Some(sid)
        || control.foreground_pgid == Some(pgid)
    {
        return Ok(());
    }
    if write && !tty_termios(id).is_some_and(|termios| has_lflag(termios, TOSTOP)) {
        return Ok(());
    }

    let signal = if write {
        SignalFlags::SIGTTOU
    } else {
        SignalFlags::SIGTTIN
    };
    if process_group_is_orphaned(pgid, sid) {
        return Err(super::FsError::Io);
    }
    let task = current_task().ok_or(super::FsError::Io)?;
    let blocked = task.inner_exclusive_access().signal_mask.contains(signal);
    let signum = signal.bits().trailing_zeros() as usize;
    let ignored = process.inner_exclusive_access().signal_actions[signum].is_ignore();
    if blocked || ignored {
        return if write {
            Ok(())
        } else {
            Err(super::FsError::Io)
        };
    }
    send_tty_signal_to_process_group(pgid, signal);
    Err(super::FsError::Io)
}

/// Classifies an N_TTY signal-generating input byte for a PTY master write.
/// The caller owns the PTY queues, so it performs any requested flush before
/// delivering the returned signal without holding the PTY lock.
pub(crate) fn tty_input_signal_action(
    id: TtyId,
    ch: u8,
) -> Option<(SignalFlags, Option<usize>, bool)> {
    with_tty_settings(id, |settings| {
        let termios = settings.termios;
        if !has_lflag(termios, ISIG) {
            return None;
        }
        let signal = if is_special_char(termios, VINTR, ch) {
            SignalFlags::SIGINT
        } else if is_special_char(termios, VQUIT, ch) {
            SignalFlags::SIGQUIT
        } else if is_special_char(termios, VSUSP, ch) {
            SignalFlags::SIGTSTP
        } else {
            return None;
        };
        Some((
            signal,
            settings.control.foreground_pgid,
            !has_lflag(termios, NOFLSH),
        ))
    })
    .flatten()
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
            if is_special_char(termios, VINTR, ch) {
                flush_input_after_signal(state, termios);
                return InputAction {
                    echo: signal_echo(termios, ch),
                    signal: Some(SignalFlags::SIGINT),
                    wake_readers: true,
                };
            }
            if is_special_char(termios, VQUIT, ch) {
                flush_input_after_signal(state, termios);
                return InputAction {
                    echo: signal_echo(termios, ch),
                    signal: Some(SignalFlags::SIGQUIT),
                    wake_readers: true,
                };
            }
            if is_special_char(termios, VSUSP, ch) {
                flush_input_after_signal(state, termios);
                return InputAction {
                    echo: signal_echo(termios, ch),
                    signal: Some(SignalFlags::SIGTSTP),
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

        if is_special_char(termios, VEOF, ch) {
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
        if is_special_char(termios, VERASE, ch) {
            if state.line_buf.pop().is_some() {
                return InputAction {
                    echo: erase_echo(termios),
                    signal: None,
                    wake_readers: false,
                };
            }
            return InputAction::none();
        }
        if is_special_char(termios, VKILL, ch) {
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

fn flush_input_after_signal(state: &mut ConsoleTtyState, termios: LinuxTermios) {
    if !has_lflag(termios, NOFLSH) {
        state.line_buf.clear();
        state.read_buf.clear();
        state.pending_eof = false;
    }
}

fn signal_foreground_process_group(signal: SignalFlags) {
    let current_pgid = current_process_group_id();
    let pgid = CONSOLE_TTY.state.exclusive_session(|state| {
        state.ensure_foreground_pgid(current_pgid);
        state.settings.control.foreground_pgid.or(current_pgid)
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

fn is_special_char(termios: LinuxTermios, index: usize, ch: u8) -> bool {
    let special = special_char(termios, index);
    special != 0 && ch == special
}

fn is_eol(termios: LinuxTermios, ch: u8) -> bool {
    ch == b'\n'
        || is_special_char(termios, VEOL, ch)
        || (has_lflag(termios, IEXTEN) && is_special_char(termios, VEOL2, ch))
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
