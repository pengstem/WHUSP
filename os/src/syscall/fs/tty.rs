use crate::sync::UPIntrFreeCell;
use crate::task::current_user_token;
use lazy_static::lazy_static;

use super::super::errno::{SysError, SysResult};
use super::fd::get_file_by_fd;
use super::user_ptr::{read_user_value, write_user_value};

const TCGETS: usize = 0x5401;
const TCSETS: usize = 0x5402;
const TCSETSW: usize = 0x5403;
const TCSETSF: usize = 0x5404;
const TIOCGWINSZ: usize = 0x5413;
const RTC_RD_TIME: usize = 0x80247009;

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
struct LinuxTermios {
    c_iflag: u32,
    c_oflag: u32,
    c_cflag: u32,
    c_lflag: u32,
    c_line: u8,
    c_cc: [u8; 19],
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
struct LinuxWinsize {
    ws_row: u16,
    ws_col: u16,
    ws_xpixel: u16,
    ws_ypixel: u16,
}

#[derive(Clone, Copy, Debug)]
struct ConsoleTtyState {
    termios: LinuxTermios,
    winsize: LinuxWinsize,
}

impl ConsoleTtyState {
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
                ws_row: 24,
                ws_col: 80,
                ws_xpixel: 0,
                ws_ypixel: 0,
            },
        }
    }
}

lazy_static! {
    // CONTEXT: stdin/stdout/stderr all point at the same UART-backed console, so a single shared
    // tty state is sufficient until the kernel grows a real per-session tty layer.
    static ref CONSOLE_TTY_STATE: UPIntrFreeCell<ConsoleTtyState> =
        unsafe { UPIntrFreeCell::new(ConsoleTtyState::new()) };
}

pub fn sys_ioctl(fd: usize, request: usize, argp: usize) -> SysResult {
    let file = get_file_by_fd(fd)?;
    // CONTEXT: musl-style ioctl callers may sign-extend 32-bit request
    // numbers such as RTC_RD_TIME into the syscall register. Linux ioctl
    // command numbers are matched on their low 32 bits here.
    let request = request & 0xffff_ffff;

    if file.is_rtc() {
        return handle_rtc_ioctl(request, argp);
    }

    if !file.is_tty() {
        return Err(SysError::ENOTTY);
    }

    let token = current_user_token();
    match request {
        TCGETS => {
            let termios = CONSOLE_TTY_STATE.exclusive_session(|state| state.termios);
            write_user_value(token, argp as *mut LinuxTermios, &termios)?;
            Ok(0)
        }
        TCSETS | TCSETSW | TCSETSF => {
            let termios = read_user_value(token, argp as *const LinuxTermios)?;
            // CONTEXT: Linux differentiates drain/flush behavior across TCSETS*, but for the
            // contest shell path we only need the termios state to round-trip and persist.
            CONSOLE_TTY_STATE.exclusive_session(|state| state.termios = termios);
            Ok(0)
        }
        TIOCGWINSZ => {
            let winsize = CONSOLE_TTY_STATE.exclusive_session(|state| state.winsize);
            write_user_value(token, argp as *mut LinuxWinsize, &winsize)?;
            Ok(0)
        }
        _ => Err(SysError::ENOTTY),
    }
}

#[repr(C)]
#[derive(Clone, Copy)]
struct LinuxRtcTime {
    tm_sec: i32,
    tm_min: i32,
    tm_hour: i32,
    tm_mday: i32,
    tm_mon: i32,
    tm_year: i32,
    tm_wday: i32,
    tm_yday: i32,
    tm_isdst: i32,
}

fn handle_rtc_ioctl(request: usize, argp: usize) -> SysResult {
    match request {
        RTC_RD_TIME => {
            let nanos = crate::timer::wall_time_nanos();
            let rtc_time = nanos_to_rtc_time(nanos);
            let token = current_user_token();
            write_user_value(token, argp as *mut LinuxRtcTime, &rtc_time)?;
            Ok(0)
        }
        _ => Err(SysError::ENOTTY),
    }
}

fn nanos_to_rtc_time(nanos: u64) -> LinuxRtcTime {
    let total_secs = (nanos / 1_000_000_000) as i64;

    let secs_in_day = (total_secs.rem_euclid(86400)) as i32;
    let mut days = total_secs.div_euclid(86400);

    let tm_sec = secs_in_day % 60;
    let tm_min = (secs_in_day % 3600) / 60;
    let tm_hour = secs_in_day / 3600;

    // Jan 1 1970 was Thursday (4)
    let tm_wday = ((days.rem_euclid(7)) as i32 + 4) % 7;

    // Howard Hinnant's civil_from_days algorithm
    days += 719468;
    let era = (if days >= 0 { days } else { days - 146096 }) / 146097;
    let doe = (days - era * 146097) as u32;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = (yoe as i64) + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };

    let is_leap = (y % 4 == 0 && y % 100 != 0) || (y % 400 == 0);
    let month_days: [i32; 12] = [
        31,
        if is_leap { 29 } else { 28 },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ];
    let mut tm_yday = (d as i32) - 1;
    for i in 0..(m as usize - 1) {
        tm_yday += month_days[i];
    }

    LinuxRtcTime {
        tm_sec,
        tm_min,
        tm_hour,
        tm_mday: d as i32,
        tm_mon: (m as i32) - 1,
        tm_year: (y as i32) - 1900,
        tm_wday,
        tm_yday,
        tm_isdst: 0,
    }
}
