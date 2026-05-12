use crate::sync::UPIntrFreeCell;
use crate::task::current_user_token;
use lazy_static::lazy_static;

use super::super::errno::{SysError, SysResult};
use super::super::user_ptr::{read_user_value, write_user_value};
use super::fd::get_file_by_fd;

const LOOP_SET_FD: usize = 0x4c00;
const LOOP_CLR_FD: usize = 0x4c01;
const LOOP_SET_STATUS: usize = 0x4c02;
const LOOP_GET_STATUS: usize = 0x4c03;
const LOOP_CTL_GET_FREE: usize = 0x4c82;
const BLKSSZGET: usize = 0x1268;
const BLKGETSIZE64: usize = 0x8008_1272;
const FS_IOC_GETFLAGS: usize = 0x8008_6601;
const FS_IOC_SETFLAGS: usize = 0x4008_6602;
const FS_IOC32_GETFLAGS: usize = 0x8004_6601;
const FS_IOC32_SETFLAGS: usize = 0x4004_6602;
const TCGETS: usize = 0x5401;
const TCSETS: usize = 0x5402;
const TCSETSW: usize = 0x5403;
const TCSETSF: usize = 0x5404;
const TCGETA: usize = 0x5405;
const TCSETA: usize = 0x5406;
const TCSETAW: usize = 0x5407;
const TCSETAF: usize = 0x5408;
const TCSBRK: usize = 0x5409;
const TCXONC: usize = 0x540a;
const TCFLSH: usize = 0x540b;
const TIOCSCTTY: usize = 0x540e;
const TIOCGWINSZ: usize = 0x5413;
const TIOCSWINSZ: usize = 0x5414;
const FIONREAD: usize = 0x541b;
const TIOCNOTTY: usize = 0x5422;
const TIOCSETD: usize = 0x5423;
const TIOCGETD: usize = 0x5424;
const TCSBRKP: usize = 0x5425;
const TIOCVHANGUP: usize = 0x5437;
const TIOCGPTN: usize = 0x8004_5430;
const TIOCSPTLCK: usize = 0x4004_5431;
const TIOCGPTLCK: usize = 0x8004_5439;
const VT_GETSTATE: usize = 0x5603;
const VT_ACTIVATE: usize = 0x5606;
const VT_DISALLOCATE: usize = 0x5608;
const VT_RESIZE: usize = 0x5609;
const VT_RESIZEX: usize = 0x560a;
const RTC_RD_TIME: usize = 0x80247009;

const N_TTY: i32 = 0;
const TCOOFF: usize = 0;
const TCOON: usize = 1;
const TCIOFF: usize = 2;
const TCION: usize = 3;
const TCIFLUSH: usize = 0;
const TCOFLUSH: usize = 1;
const TCIOFLUSH: usize = 2;

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
#[derive(Clone, Copy, Debug)]
struct LinuxTermio {
    c_iflag: u16,
    c_oflag: u16,
    c_cflag: u16,
    c_lflag: u16,
    c_line: u8,
    c_cc: [u8; 8],
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
struct LinuxWinsize {
    ws_row: u16,
    ws_col: u16,
    ws_xpixel: u16,
    ws_ypixel: u16,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
struct LinuxVtStat {
    v_active: u16,
    v_signal: u16,
    v_state: u16,
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
struct LinuxLoopInfo {
    lo_device: u64,
    lo_inode: u64,
    lo_rdevice: u64,
    lo_offset: u64,
    lo_sizelimit: u64,
    lo_number: u32,
    lo_encrypt_type: u32,
    lo_encrypt_key_size: u32,
    lo_flags: u32,
    lo_name: [u8; 64],
    lo_encrypt_key: [u8; 32],
    lo_init: [u64; 2],
    reserved: [u8; 4],
}

impl Default for LinuxLoopInfo {
    fn default() -> Self {
        Self {
            lo_device: 0,
            lo_inode: 0,
            lo_rdevice: 0,
            lo_offset: 0,
            lo_sizelimit: 0,
            lo_number: 0,
            lo_encrypt_type: 0,
            lo_encrypt_key_size: 0,
            lo_flags: 0,
            lo_name: [0; 64],
            lo_encrypt_key: [0; 32],
            lo_init: [0; 2],
            reserved: [0; 4],
        }
    }
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
                ws_row: 80,
                ws_col: 240,
                ws_xpixel: 0,
                ws_ypixel: 0,
            },
        }
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

    if crate::fs::is_devfs_loop_control(file.as_ref()) {
        return match request {
            LOOP_CTL_GET_FREE => {
                // CONTEXT: LTP uses /dev/loop-control only to discover one
                // scratch loop device before mounting syscall filesystem
                // tests. The kernel currently exposes a single lightweight
                // loop slot, not the full Linux loop-control device model.
                Ok(crate::fs::find_free_loop_device()? as isize)
            }
            _ => Err(SysError::EINVAL),
        };
    }

    if let Some(loop_id) = crate::fs::devfs_loop_device_id(file.as_ref()) {
        return handle_loop_ioctl(loop_id, request, argp);
    }

    if file.is_rtc() {
        return handle_rtc_ioctl(request, argp);
    }

    match request {
        TIOCGPTN => {
            let pty_number = crate::fs::devfs_pty_number(file.as_ref()).ok_or(SysError::ENOTTY)?;
            let token = current_user_token();
            write_user_value(token, argp as *mut u32, &pty_number)?;
            return Ok(0);
        }
        TIOCSPTLCK => {
            let token = current_user_token();
            let locked = read_user_value(token, argp as *const i32)? != 0;
            if !crate::fs::set_devfs_pty_locked(file.as_ref(), locked)? {
                return Err(SysError::ENOTTY);
            }
            return Ok(0);
        }
        TIOCGPTLCK => {
            let locked = crate::fs::devfs_pty_lock_state(file.as_ref()).ok_or(SysError::ENOTTY)?;
            let locked = if locked { 1i32 } else { 0i32 };
            let token = current_user_token();
            write_user_value(token, argp as *mut i32, &locked)?;
            return Ok(0);
        }
        _ => {}
    }

    if request == FIONREAD {
        let unread = file.pipe_occupied().ok_or(SysError::ENOTTY)? as i32;
        let token = current_user_token();
        write_user_value(token, argp as *mut i32, &unread)?;
        return Ok(0);
    }

    match request {
        FS_IOC_GETFLAGS | FS_IOC32_GETFLAGS => {
            let flags = file.inode_flags().map_err(fs_flag_ioctl_error)? as i32;
            let token = current_user_token();
            write_user_value(token, argp as *mut i32, &flags)?;
            return Ok(0);
        }
        FS_IOC_SETFLAGS | FS_IOC32_SETFLAGS => {
            let token = current_user_token();
            let flags = read_user_value(token, argp as *const i32)? as u32;
            file.set_inode_flags(flags).map_err(fs_flag_ioctl_error)?;
            return Ok(0);
        }
        _ => {}
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
        TCGETA => {
            let termio =
                CONSOLE_TTY_STATE.exclusive_session(|state| termios_to_termio(state.termios));
            write_user_value(token, argp as *mut LinuxTermio, &termio)?;
            Ok(0)
        }
        TCSETA | TCSETAW | TCSETAF => {
            let termio = read_user_value(token, argp as *const LinuxTermio)?;
            CONSOLE_TTY_STATE.exclusive_session(|state| apply_termio(&mut state.termios, termio));
            Ok(0)
        }
        TIOCGWINSZ => {
            let winsize = CONSOLE_TTY_STATE.exclusive_session(|state| state.winsize);
            write_user_value(token, argp as *mut LinuxWinsize, &winsize)?;
            Ok(0)
        }
        TIOCSWINSZ => {
            let winsize = read_user_value(token, argp as *const LinuxWinsize)?;
            CONSOLE_TTY_STATE.exclusive_session(|state| state.winsize = winsize);
            Ok(0)
        }
        TCSBRK | TCSBRKP | TIOCSCTTY | TIOCNOTTY | TIOCVHANGUP => Ok(0),
        TCXONC => match argp {
            TCOOFF | TCOON | TCIOFF | TCION => Ok(0),
            _ => Err(SysError::EINVAL),
        },
        TCFLSH => match argp {
            TCIFLUSH | TCOFLUSH | TCIOFLUSH => Ok(0),
            _ => Err(SysError::EINVAL),
        },
        TIOCGETD => {
            let discipline = N_TTY;
            write_user_value(token, argp as *mut i32, &discipline)?;
            Ok(0)
        }
        TIOCSETD => {
            let discipline = read_user_value(token, argp as *const i32)?;
            match discipline {
                N_TTY => Ok(0),
                // CONTEXT: SLIP/SLCAN/PPP line disciplines create network
                // devices on Linux, and HDLC has its own protocol buffering.
                // This kernel has no tty line-discipline
                // subsystem yet, so report EINVAL instead of pretending those
                // protocol drivers exist.
                _ => Err(SysError::EINVAL),
            }
        }
        VT_GETSTATE => {
            let stat = LinuxVtStat {
                v_active: 1,
                v_signal: 0,
                v_state: 1 << 1,
            };
            write_user_value(token, argp as *mut LinuxVtStat, &stat)?;
            Ok(0)
        }
        VT_ACTIVATE | VT_DISALLOCATE | VT_RESIZE | VT_RESIZEX => {
            // CONTEXT: /dev/tty8 and /dev/tty9 are lightweight virtual-console
            // compatibility nodes for LTP race tests. There is no framebuffer
            // console allocation state to switch or free yet.
            Ok(0)
        }
        _ => Err(SysError::ENOTTY),
    }
}

fn fs_flag_ioctl_error(error: crate::fs::FsError) -> SysError {
    match error {
        crate::fs::FsError::Unsupported => SysError::ENOTTY,
        _ => error.into(),
    }
}

fn handle_loop_ioctl(loop_id: usize, request: usize, argp: usize) -> SysResult {
    match request {
        LOOP_SET_FD => {
            let backend = get_file_by_fd(argp)?;
            crate::fs::attach_loop_device(loop_id, backend)?;
            Ok(0)
        }
        LOOP_CLR_FD => {
            crate::fs::detach_loop_device(loop_id)?;
            Ok(0)
        }
        LOOP_SET_STATUS => {
            let token = current_user_token();
            let _ = read_user_value(token, argp as *const LinuxLoopInfo)?;
            Ok(0)
        }
        LOOP_GET_STATUS => {
            if !crate::fs::loop_device_is_attached(loop_id) {
                return Err(SysError::ENXIO);
            }
            let token = current_user_token();
            let mut info = LinuxLoopInfo::default();
            info.lo_number = loop_id as u32;
            write_user_value(token, argp as *mut LinuxLoopInfo, &info)?;
            Ok(0)
        }
        BLKGETSIZE64 => {
            let size = crate::fs::loop_device_size(loop_id)?;
            let token = current_user_token();
            write_user_value(token, argp as *mut u64, &size)?;
            Ok(0)
        }
        BLKSSZGET => {
            // CONTEXT: The lightweight loop device is backed by a regular
            // scratch file for LTP filesystem tests. Report a conventional
            // 512-byte logical sector size so O_DIRECT alignment tests can
            // allocate buffers and offsets before exercising pwritev.
            let sector_size = 512i32;
            let token = current_user_token();
            write_user_value(token, argp as *mut i32, &sector_size)?;
            Ok(0)
        }
        _ => Err(SysError::EINVAL),
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
