use crate::config::PAGE_SIZE;
use crate::fs::{
    FS_VERITY_FL, LinuxTermio, LinuxTermios, LinuxTermios2, LinuxWinsize, ProcNamespaceInfo,
    ProcNamespaceKind, apply_console_tty_termio, console_tty_available_bytes,
    console_tty_foreground_pgid, console_tty_termio, console_tty_termios, console_tty_termios2,
    console_tty_winsize, proc_namespace_info_from_path, proc_namespace_kind_name,
    proc_namespace_stat_ino, set_console_tty_foreground_pgid, set_console_tty_termios,
    set_console_tty_termios2, set_console_tty_winsize,
};
use crate::mm::UserBuffer;
use crate::task::current_user_token;
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;

use super::super::errno::{SysError, SysResult};
use super::super::user_ptr::{copy_to_user, read_user_value, write_user_value};
use super::fd::{get_fd_entry_by_fd, get_file_by_fd};

const LOOP_SET_FD: usize = 0x4c00;
const LOOP_CLR_FD: usize = 0x4c01;
const LOOP_SET_STATUS: usize = 0x4c02;
const LOOP_GET_STATUS: usize = 0x4c03;
const LOOP_SET_STATUS64: usize = 0x4c04;
const LOOP_GET_STATUS64: usize = 0x4c05;
const LOOP_CHANGE_FD: usize = 0x4c06;
const LOOP_SET_CAPACITY: usize = 0x4c07;
const LOOP_SET_DIRECT_IO: usize = 0x4c08;
const LOOP_SET_BLOCK_SIZE: usize = 0x4c09;
const LOOP_CONFIGURE: usize = 0x4c0a;
const LOOP_CTL_GET_FREE: usize = 0x4c82;
const BLKROSET: usize = 0x125d;
const BLKROGET: usize = 0x125e;
const BLKGETSIZE: usize = 0x1260;
const BLKRASET: usize = 0x1262;
const BLKRAGET: usize = 0x1263;
const BLKSSZGET: usize = 0x1268;
const BLKGETSIZE64: usize = 0x8008_1272;
const RNDGETENTCNT: usize = 0x8004_5200;
const TUNGETFEATURES: usize = 0x8004_54cf;
const SIOCGIFNAME: usize = 0x8910;
const SIOCGIFCONF: usize = 0x8912;
const SIOCGIFFLAGS: usize = 0x8913;
const SIOCSIFFLAGS: usize = 0x8914;
const SIOCSIFMTU: usize = 0x8922;
const SIOCGIFINDEX: usize = 0x8933;
const EVIOCGVERSION: usize = 0x8004_4501;
const EVIOCGID: usize = 0x8008_4502;
const EVIOCGREP: usize = 0x8008_4503;
const EVIOCGRAB: usize = 0x4004_4590;
const UI_DEV_CREATE: usize = 0x5501;
const UI_DEV_DESTROY: usize = 0x5502;
const UI_DEV_SETUP: usize = 0x405c_5503;
const UI_SET_EVBIT: usize = 0x4004_5564;
const UI_SET_KEYBIT: usize = 0x4004_5565;
const UI_SET_RELBIT: usize = 0x4004_5566;
const UI_SET_ABSBIT: usize = 0x4004_5567;
const UI_SET_MSCBIT: usize = 0x4004_5568;
const UI_SET_LEDBIT: usize = 0x4004_5569;
const UI_SET_SNDBIT: usize = 0x4004_556a;
const UI_SET_FFBIT: usize = 0x4004_556b;
const UI_SET_SWBIT: usize = 0x4004_556d;
const UI_SET_PROPBIT: usize = 0x4004_556e;
const UI_GET_VERSION: usize = 0x8004_552d;
const FS_IOC_GETFLAGS: usize = 0x8008_6601;
const FS_IOC_SETFLAGS: usize = 0x4008_6602;
const FS_IOC32_GETFLAGS: usize = 0x8004_6601;
const FS_IOC32_SETFLAGS: usize = 0x4004_6602;
const FS_IOC_ENABLE_VERITY: usize = 0x4080_6685;
const NS_GET_USERNS: usize = 0xb701;
const NS_GET_PARENT: usize = 0xb702;
const NS_GET_NSTYPE: usize = 0xb703;
const NS_GET_OWNER_UID: usize = 0xb704;
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
const TIOCGPGRP: usize = 0x540f;
const TIOCSPGRP: usize = 0x5410;
const TIOCGWINSZ: usize = 0x5413;
const TIOCSWINSZ: usize = 0x5414;
const FIONREAD: usize = 0x541b;
const TIOCNOTTY: usize = 0x5422;
const TIOCSETD: usize = 0x5423;
const TIOCGETD: usize = 0x5424;
const TCSBRKP: usize = 0x5425;
const TCGETS2: usize = 0x802c_542a;
const TCSETS2: usize = 0x402c_542b;
const TCSETSW2: usize = 0x402c_542c;
const TCSETSF2: usize = 0x402c_542d;
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
const IOC_READ: usize = 2;
const EV_VERSION: i32 = 0x010001;
const UINPUT_VERSION: u32 = 5;
const INPUT_REP_DELAY_MS: u32 = 250;
const INPUT_REP_PERIOD_MS: u32 = 33;
const RANDOM_ENTROPY_AVAIL: i32 = 256;
const IFNAMSIZ: usize = 16;
const IFREQ_DATA_LEN: usize = 24;
const IFREQ_SIZE: usize = IFNAMSIZ + IFREQ_DATA_LEN;
const LOOPBACK_IF_INDEX: i32 = 1;
const LOOPBACK_IF_FLAGS: i16 = 0x1 | 0x8 | 0x40;
const LO_FLAGS_READ_ONLY: u32 = 1;
const LO_FLAGS_AUTOCLEAR: u32 = 4;
const LO_FLAGS_PARTSCAN: u32 = 8;
const LO_FLAGS_DIRECT_IO: u32 = 16;
const CLONE_NEWNS_VALUE: isize = 0x0002_0000;
const CLONE_NEWUTS_VALUE: isize = 0x0400_0000;
const CLONE_NEWUSER_VALUE: isize = 0x1000_0000;
const CLONE_NEWPID_VALUE: isize = 0x2000_0000;
const TUN_SUPPORTED_FEATURES: u32 =
    0x0001 | 0x0002 | 0x0010 | 0x0020 | 0x0040 | 0x0100 | 0x1000 | 0x2000 | 0x4000;

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
struct LinuxInputId {
    bustype: u16,
    vendor: u16,
    product: u16,
    version: u16,
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
    lo_number: i32,
    lo_device: u32,
    lo_inode: u64,
    lo_rdevice: u32,
    lo_offset: i32,
    lo_encrypt_type: i32,
    lo_encrypt_key_size: i32,
    lo_flags: i32,
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

#[repr(C)]
#[derive(Clone, Copy, Debug)]
struct LinuxLoopInfo64 {
    lo_device: u64,
    lo_inode: u64,
    lo_rdevice: u64,
    lo_offset: u64,
    lo_sizelimit: u64,
    lo_number: u32,
    lo_encrypt_type: u32,
    lo_encrypt_key_size: u32,
    lo_flags: u32,
    lo_file_name: [u8; 64],
    lo_crypt_name: [u8; 64],
    lo_encrypt_key: [u8; 32],
    lo_init: [u64; 2],
}

impl Default for LinuxLoopInfo64 {
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
            lo_file_name: [0; 64],
            lo_crypt_name: [0; 64],
            lo_encrypt_key: [0; 32],
            lo_init: [0; 2],
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
struct LinuxLoopConfig {
    fd: u32,
    block_size: u32,
    info: LinuxLoopInfo64,
    reserved: [u64; 8],
}

#[derive(Debug)]
struct NamespaceFile {
    info: ProcNamespaceInfo,
}

impl NamespaceFile {
    fn new(info: ProcNamespaceInfo) -> Self {
        Self { info }
    }
}

impl crate::fs::File for NamespaceFile {
    fn as_any(&self) -> &dyn core::any::Any {
        self
    }

    fn readable(&self) -> bool {
        true
    }

    fn writable(&self) -> bool {
        false
    }

    fn read(&self, _buf: UserBuffer) -> usize {
        0
    }

    fn write(&self, _buf: UserBuffer) -> usize {
        0
    }

    fn stat(&self) -> crate::fs::FsResult<crate::fs::FileStat> {
        let mut stat = crate::fs::FileStat::with_mode(crate::fs::S_IFREG | 0o444);
        stat.dev = 0x6e736673;
        stat.ino = proc_namespace_stat_ino(self.info.kind, self.info.id);
        Ok(stat)
    }

    fn proc_fd_target(&self) -> Option<String> {
        Some(format!(
            "{}:[{}]",
            proc_namespace_kind_name(self.info.kind),
            proc_namespace_stat_ino(self.info.kind, self.info.id)
        ))
    }
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

    if crate::fs::is_devfs_uinput(file.as_ref()) {
        return handle_uinput_ioctl(file.as_ref(), request, argp);
    }

    if crate::fs::is_devfs_input_event(file.as_ref()) {
        return handle_input_event_ioctl(file.as_ref(), request, argp);
    }

    if crate::fs::is_devfs_tun(file.as_ref()) {
        return handle_tun_ioctl(request, argp);
    }

    if file.is_dev_random() {
        return handle_random_ioctl(request, argp);
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
        let unread = if file.is_tty() {
            console_tty_available_bytes()
        } else {
            file.pipe_occupied().ok_or(SysError::ENOTTY)?
        } as i32;
        let token = current_user_token();
        write_user_value(token, argp as *mut i32, &unread)?;
        return Ok(0);
    }

    if file.is_socket() {
        return handle_socket_if_ioctl(request, argp);
    }

    if let Some(namespace) = namespace_info_from_file(file.as_ref()) {
        return handle_namespace_ioctl(namespace, request, argp);
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
        FS_IOC_ENABLE_VERITY => {
            let flags = file.inode_flags().map_err(fs_flag_ioctl_error)? | FS_VERITY_FL;
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
            let termios = console_tty_termios();
            write_user_value(token, argp as *mut LinuxTermios, &termios)?;
            Ok(0)
        }
        TCGETS2 => {
            let termios = console_tty_termios2();
            write_user_value(token, argp as *mut LinuxTermios2, &termios)?;
            Ok(0)
        }
        TCSETS | TCSETSW | TCSETSF => {
            let termios = read_user_value(token, argp as *const LinuxTermios)?;
            // CONTEXT: Linux differentiates drain/flush behavior across TCSETS*, but for the
            // contest shell path we only need the termios state to round-trip and persist.
            set_console_tty_termios(termios);
            Ok(0)
        }
        TCSETS2 | TCSETSW2 | TCSETSF2 => {
            let termios = read_user_value(token, argp as *const LinuxTermios2)?;
            // CONTEXT: Linux differentiates drain/flush behavior across TCSETS2*, but for the
            // contest shell path we only need the termios state to round-trip and persist.
            set_console_tty_termios2(termios);
            Ok(0)
        }
        TCGETA => {
            let termio = console_tty_termio();
            write_user_value(token, argp as *mut LinuxTermio, &termio)?;
            Ok(0)
        }
        TCSETA | TCSETAW | TCSETAF => {
            let termio = read_user_value(token, argp as *const LinuxTermio)?;
            apply_console_tty_termio(termio);
            Ok(0)
        }
        TIOCGPGRP => {
            let pgid = console_tty_foreground_pgid() as i32;
            write_user_value(token, argp as *mut i32, &pgid)?;
            Ok(0)
        }
        TIOCSPGRP => {
            let pgid = read_user_value(token, argp as *const i32)?;
            if pgid <= 0 {
                return Err(SysError::EINVAL);
            }
            // CONTEXT: This console has a single shared foreground process
            // group; full Linux session/controlling-tty permission checks are
            // still outside the current tty model.
            set_console_tty_foreground_pgid(pgid as usize);
            Ok(0)
        }
        TIOCGWINSZ => {
            let winsize = console_tty_winsize();
            write_user_value(token, argp as *mut LinuxWinsize, &winsize)?;
            Ok(0)
        }
        TIOCSWINSZ => {
            let winsize = read_user_value(token, argp as *const LinuxWinsize)?;
            set_console_tty_winsize(winsize);
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

fn ioctl_nr(request: usize) -> usize {
    request & 0xff
}

fn ioctl_type(request: usize) -> usize {
    (request >> 8) & 0xff
}

fn ioctl_size(request: usize) -> usize {
    (request >> 16) & 0x3fff
}

fn ioctl_dir(request: usize) -> usize {
    (request >> 30) & 0x3
}

fn copy_capped_string_to_user(token: usize, argp: usize, bytes: &[u8], size: usize) -> SysResult {
    if size == 0 {
        return Ok(0);
    }
    let mut out = [0u8; 256];
    let copied = bytes.len().min(size.saturating_sub(1)).min(out.len() - 1);
    out[..copied].copy_from_slice(&bytes[..copied]);
    let total = (copied + 1).min(size).min(out.len());
    copy_to_user(token, argp as *mut u8, &out[..total])?;
    Ok(0)
}

fn namespace_info_from_file(
    file: &(dyn crate::fs::File + Send + Sync),
) -> Option<ProcNamespaceInfo> {
    if let Some(namespace) = file.as_any().downcast_ref::<NamespaceFile>() {
        return Some(namespace.info);
    }
    file.proc_fd_target()
        .and_then(|path| proc_namespace_info_from_path(path.as_str()))
}

fn namespace_type_value(kind: ProcNamespaceKind) -> isize {
    match kind {
        ProcNamespaceKind::Mnt => CLONE_NEWNS_VALUE,
        ProcNamespaceKind::Pid => CLONE_NEWPID_VALUE,
        ProcNamespaceKind::User => CLONE_NEWUSER_VALUE,
        ProcNamespaceKind::Uts => CLONE_NEWUTS_VALUE,
    }
}

fn install_namespace_fd(info: ProcNamespaceInfo) -> SysResult {
    let file = Arc::new(NamespaceFile::new(info));
    super::fd::install_file_fd(
        file,
        crate::fs::OpenFlags::RDONLY | crate::fs::OpenFlags::CLOEXEC,
        None,
    )
}

fn handle_namespace_parent_ioctl(info: ProcNamespaceInfo) -> SysResult {
    match info.kind {
        ProcNamespaceKind::Pid => {
            let current = crate::task::current_process().pid_namespace();
            let parent_id = info.parent_id.ok_or(SysError::EPERM)?;
            if info.id == current.id {
                return Err(SysError::EPERM);
            }
            install_namespace_fd(ProcNamespaceInfo {
                kind: ProcNamespaceKind::Pid,
                id: parent_id,
                parent_id: None,
            })
        }
        ProcNamespaceKind::User => {
            let current = crate::task::current_process().user_namespace();
            let parent_id = info.parent_id.ok_or(SysError::EPERM)?;
            if info.id == current.id {
                return Err(SysError::EPERM);
            }
            install_namespace_fd(ProcNamespaceInfo {
                kind: ProcNamespaceKind::User,
                id: parent_id,
                parent_id: None,
            })
        }
        ProcNamespaceKind::Mnt | ProcNamespaceKind::Uts => Err(SysError::EINVAL),
    }
}

fn handle_namespace_ioctl(info: ProcNamespaceInfo, request: usize, argp: usize) -> SysResult {
    match request {
        NS_GET_PARENT => handle_namespace_parent_ioctl(info),
        NS_GET_USERNS => match info.kind {
            ProcNamespaceKind::User => handle_namespace_parent_ioctl(info),
            // UNFINISHED: The kernel records only process-visible user
            // namespace ancestry. Owning-user-namespace discovery for other
            // namespace types is deferred until full user namespace support.
            _ => Err(SysError::EPERM),
        },
        NS_GET_NSTYPE => Ok(namespace_type_value(info.kind)),
        NS_GET_OWNER_UID => {
            if info.kind != ProcNamespaceKind::User {
                return Err(SysError::EINVAL);
            }
            let uid = 0u32;
            let token = current_user_token();
            write_user_value(token, argp as *mut u32, &uid)?;
            Ok(0)
        }
        _ => Err(SysError::ENOTTY),
    }
}

fn handle_uinput_ioctl(
    file: &(dyn crate::fs::File + Send + Sync),
    request: usize,
    argp: usize,
) -> SysResult {
    match request {
        UI_DEV_CREATE => {
            crate::fs::devfs_uinput_create(file)?;
            Ok(0)
        }
        UI_DEV_DESTROY => {
            crate::fs::devfs_uinput_destroy(file)?;
            Ok(0)
        }
        UI_DEV_SETUP => Ok(0),
        UI_SET_EVBIT => {
            crate::fs::devfs_uinput_set_evbit(file, argp)?;
            Ok(0)
        }
        UI_SET_KEYBIT => {
            crate::fs::devfs_uinput_set_keybit(file, argp)?;
            Ok(0)
        }
        UI_SET_RELBIT => {
            crate::fs::devfs_uinput_set_relbit(file, argp)?;
            Ok(0)
        }
        UI_SET_ABSBIT | UI_SET_MSCBIT | UI_SET_LEDBIT | UI_SET_SNDBIT | UI_SET_FFBIT
        | UI_SET_SWBIT | UI_SET_PROPBIT => Ok(0),
        UI_GET_VERSION => {
            let token = current_user_token();
            write_user_value(token, argp as *mut u32, &UINPUT_VERSION)?;
            Ok(0)
        }
        _ if ioctl_dir(request) == IOC_READ
            && ioctl_type(request) == b'U' as usize
            && ioctl_nr(request) == 44 =>
        {
            let token = current_user_token();
            copy_capped_string_to_user(token, argp, b"input0", ioctl_size(request))
        }
        _ => Err(SysError::EINVAL),
    }
}

fn handle_input_event_ioctl(
    file: &(dyn crate::fs::File + Send + Sync),
    request: usize,
    argp: usize,
) -> SysResult {
    match request {
        EVIOCGVERSION => {
            let token = current_user_token();
            write_user_value(token, argp as *mut i32, &EV_VERSION)?;
            Ok(0)
        }
        EVIOCGID => {
            let token = current_user_token();
            let id = LinuxInputId {
                bustype: 0x03,
                vendor: 0x01,
                product: 0x01,
                version: 0x01,
            };
            write_user_value(token, argp as *mut LinuxInputId, &id)?;
            Ok(0)
        }
        EVIOCGREP => {
            let token = current_user_token();
            let rep = [INPUT_REP_DELAY_MS, INPUT_REP_PERIOD_MS];
            write_user_value(token, argp as *mut [u32; 2], &rep)?;
            Ok(0)
        }
        EVIOCGRAB => {
            crate::fs::devfs_input_event_set_grabbed(file, argp != 0)?;
            Ok(0)
        }
        _ if ioctl_dir(request) == IOC_READ
            && ioctl_type(request) == b'E' as usize
            && ioctl_nr(request) == 0x06 =>
        {
            let name = crate::fs::devfs_input_event_name(file).ok_or(SysError::ENOTTY)?;
            let token = current_user_token();
            copy_capped_string_to_user(token, argp, name.as_slice(), ioctl_size(request))
        }
        _ => Err(SysError::EINVAL),
    }
}

fn handle_tun_ioctl(request: usize, argp: usize) -> SysResult {
    match request {
        TUNGETFEATURES => {
            let token = current_user_token();
            write_user_value(token, argp as *mut u32, &TUN_SUPPORTED_FEATURES)?;
            Ok(0)
        }
        _ => Err(SysError::EINVAL),
    }
}

fn handle_random_ioctl(request: usize, argp: usize) -> SysResult {
    match request {
        RNDGETENTCNT => {
            let token = current_user_token();
            write_user_value(token, argp as *mut i32, &RANDOM_ENTROPY_AVAIL)?;
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

fn loop_backend_from_fd(
    fd: usize,
) -> SysResult<(
    alloc::sync::Arc<dyn crate::fs::File + Send + Sync>,
    bool,
    Option<String>,
)> {
    let entry = get_fd_entry_by_fd(fd)?;
    let file = entry.file();
    let read_only = !file.writable();
    let path = entry
        .dir_path()
        .map(String::from)
        .or_else(|| file.proc_fd_target());
    Ok((file, read_only, path))
}

fn validate_loop_block_size(block_size: usize, allow_zero: bool) -> SysResult<Option<usize>> {
    if block_size == 0 && allow_zero {
        return Ok(None);
    }
    if !(512..=PAGE_SIZE).contains(&block_size) || !block_size.is_power_of_two() {
        return Err(SysError::EINVAL);
    }
    Ok(Some(block_size))
}

fn copy_loop_name(dst: &mut [u8], path: Option<String>) {
    let Some(path) = path else {
        return;
    };
    let bytes = path.as_bytes();
    let len = bytes.len().min(dst.len().saturating_sub(1));
    dst[..len].copy_from_slice(&bytes[..len]);
}

fn loop_backing_path_from_sysfs() -> Option<String> {
    let content = crate::fs::loop_device_sysfs_content("/sys/block/loop0/loop/backing_file")?;
    let text = core::str::from_utf8(&content).ok()?.trim_end_matches('\n');
    if text.is_empty() {
        None
    } else {
        Some(String::from(text))
    }
}

fn make_loop_info(loop_id: usize) -> SysResult<LinuxLoopInfo> {
    let mut info = LinuxLoopInfo {
        lo_number: loop_id as i32,
        lo_flags: crate::fs::loop_device_flags(loop_id)? as i32,
        ..LinuxLoopInfo::default()
    };
    copy_loop_name(&mut info.lo_name, loop_backing_path_from_sysfs());
    Ok(info)
}

fn make_loop_info64(loop_id: usize) -> SysResult<LinuxLoopInfo64> {
    let mut info = LinuxLoopInfo64 {
        lo_number: loop_id as u32,
        lo_flags: crate::fs::loop_device_flags(loop_id)?,
        lo_sizelimit: crate::fs::loop_device_size_limit(loop_id)?,
        ..LinuxLoopInfo64::default()
    };
    copy_loop_name(&mut info.lo_file_name, loop_backing_path_from_sysfs());
    Ok(info)
}

fn handle_loop_ioctl(loop_id: usize, request: usize, argp: usize) -> SysResult {
    match request {
        LOOP_SET_FD => {
            let (backend, read_only, path) = loop_backend_from_fd(argp)?;
            crate::fs::attach_loop_device(loop_id, backend, read_only, path)?;
            Ok(0)
        }
        LOOP_CLR_FD => {
            crate::fs::detach_loop_device(loop_id)?;
            Ok(0)
        }
        LOOP_SET_STATUS => {
            let token = current_user_token();
            let info = read_user_value(token, argp as *const LinuxLoopInfo)?;
            crate::fs::loop_device_set_status(loop_id, info.lo_flags as u32, None)?;
            Ok(0)
        }
        LOOP_GET_STATUS => {
            if !crate::fs::loop_device_is_attached(loop_id) {
                return Err(SysError::ENXIO);
            }
            let token = current_user_token();
            let info = make_loop_info(loop_id)?;
            write_user_value(token, argp as *mut LinuxLoopInfo, &info)?;
            Ok(0)
        }
        LOOP_SET_STATUS64 => {
            let token = current_user_token();
            let info = read_user_value(token, argp as *const LinuxLoopInfo64)?;
            crate::fs::loop_device_set_status(loop_id, info.lo_flags, Some(info.lo_sizelimit))?;
            Ok(0)
        }
        LOOP_GET_STATUS64 => {
            if !crate::fs::loop_device_is_attached(loop_id) {
                return Err(SysError::ENXIO);
            }
            let token = current_user_token();
            let info = make_loop_info64(loop_id)?;
            write_user_value(token, argp as *mut LinuxLoopInfo64, &info)?;
            Ok(0)
        }
        LOOP_CHANGE_FD => {
            let (backend, _, path) = loop_backend_from_fd(argp)?;
            crate::fs::loop_device_change_fd(loop_id, backend, path)?;
            Ok(0)
        }
        LOOP_SET_CAPACITY => {
            crate::fs::loop_device_refresh_size(loop_id)?;
            Ok(0)
        }
        LOOP_SET_DIRECT_IO => {
            crate::fs::loop_device_set_direct_io(loop_id, argp != 0)?;
            Ok(0)
        }
        LOOP_SET_BLOCK_SIZE => {
            if let Some(block_size) = validate_loop_block_size(argp, false)? {
                crate::fs::loop_device_set_block_size(loop_id, block_size)?;
            }
            Ok(0)
        }
        LOOP_CONFIGURE => {
            let token = current_user_token();
            let config = read_user_value(token, argp as *const LinuxLoopConfig)?;
            let block_size = validate_loop_block_size(config.block_size as usize, true)?;
            let (backend, fd_read_only, path) = loop_backend_from_fd(config.fd as usize)?;
            let read_only = fd_read_only || config.info.lo_flags & LO_FLAGS_READ_ONLY != 0;
            crate::fs::attach_loop_device(loop_id, backend, read_only, path)?;
            if let Some(block_size) = block_size {
                crate::fs::loop_device_set_block_size(loop_id, block_size)?;
            }
            crate::fs::loop_device_set_status(
                loop_id,
                config.info.lo_flags & (LO_FLAGS_AUTOCLEAR | LO_FLAGS_PARTSCAN),
                Some(config.info.lo_sizelimit),
            )?;
            if config.info.lo_flags & LO_FLAGS_DIRECT_IO != 0 {
                crate::fs::loop_device_set_direct_io(loop_id, true)?;
            }
            Ok(0)
        }
        BLKROSET => {
            let token = current_user_token();
            let read_only = read_user_value(token, argp as *const i32)? != 0;
            crate::fs::loop_device_set_read_only(loop_id, read_only)?;
            Ok(0)
        }
        BLKROGET => {
            let read_only = if crate::fs::loop_device_is_read_only(loop_id) {
                1i32
            } else {
                0i32
            };
            let token = current_user_token();
            write_user_value(token, argp as *mut i32, &read_only)?;
            Ok(0)
        }
        BLKGETSIZE => {
            let size = (crate::fs::loop_device_size(loop_id)? / 512) as usize;
            let token = current_user_token();
            write_user_value(token, argp as *mut usize, &size)?;
            Ok(0)
        }
        BLKGETSIZE64 => {
            let size = crate::fs::loop_device_size(loop_id)?;
            let token = current_user_token();
            write_user_value(token, argp as *mut u64, &size)?;
            Ok(0)
        }
        BLKRASET => {
            crate::fs::loop_device_set_read_ahead(loop_id, argp)?;
            Ok(0)
        }
        BLKRAGET => {
            let read_ahead = crate::fs::loop_device_read_ahead(loop_id)?;
            let token = current_user_token();
            write_user_value(token, argp as *mut usize, &read_ahead)?;
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

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct LinuxIfReq {
    ifr_name: [u8; IFNAMSIZ],
    ifr_data: [u8; IFREQ_DATA_LEN],
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct LinuxIfConf {
    ifc_len: i32,
    ifc_buf: usize,
}

fn loopback_ifreq() -> LinuxIfReq {
    let mut req = LinuxIfReq::default();
    req.ifr_name[..2].copy_from_slice(b"lo");
    set_ifreq_i32(&mut req, LOOPBACK_IF_INDEX);
    req
}

fn ifreq_name(req: &LinuxIfReq) -> &[u8] {
    let end = req
        .ifr_name
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(IFNAMSIZ);
    &req.ifr_name[..end]
}

fn ifreq_i32(req: &LinuxIfReq) -> i32 {
    i32::from_ne_bytes([
        req.ifr_data[0],
        req.ifr_data[1],
        req.ifr_data[2],
        req.ifr_data[3],
    ])
}

fn set_ifreq_i32(req: &mut LinuxIfReq, value: i32) {
    req.ifr_data[..4].copy_from_slice(&value.to_ne_bytes());
}

fn set_ifreq_i16(req: &mut LinuxIfReq, value: i16) {
    req.ifr_data[..2].copy_from_slice(&value.to_ne_bytes());
}

fn handle_socket_if_ioctl(request: usize, argp: usize) -> SysResult {
    let token = current_user_token();
    match request {
        SIOCGIFINDEX => {
            let mut req = read_user_value(token, argp as *const LinuxIfReq)?;
            if ifreq_name(&req) != b"lo" {
                return Err(SysError::ENODEV);
            }
            set_ifreq_i32(&mut req, LOOPBACK_IF_INDEX);
            write_user_value(token, argp as *mut LinuxIfReq, &req)?;
            Ok(0)
        }
        SIOCGIFNAME => {
            let mut req = read_user_value(token, argp as *const LinuxIfReq)?;
            if ifreq_i32(&req) != LOOPBACK_IF_INDEX {
                return Err(SysError::ENXIO);
            }
            req.ifr_name = [0; IFNAMSIZ];
            req.ifr_name[..2].copy_from_slice(b"lo");
            write_user_value(token, argp as *mut LinuxIfReq, &req)?;
            Ok(0)
        }
        SIOCGIFFLAGS => {
            let mut req = read_user_value(token, argp as *const LinuxIfReq)?;
            if ifreq_name(&req) != b"lo" {
                return Err(SysError::ENODEV);
            }
            set_ifreq_i16(&mut req, LOOPBACK_IF_FLAGS);
            write_user_value(token, argp as *mut LinuxIfReq, &req)?;
            Ok(0)
        }
        SIOCGIFCONF => {
            let mut conf = read_user_value(token, argp as *const LinuxIfConf)?;
            if conf.ifc_buf != 0 && conf.ifc_len as usize >= IFREQ_SIZE {
                let req = loopback_ifreq();
                let mut bytes = [0u8; IFREQ_SIZE];
                bytes[..IFNAMSIZ].copy_from_slice(&req.ifr_name);
                bytes[IFNAMSIZ..].copy_from_slice(&req.ifr_data);
                copy_to_user(token, conf.ifc_buf as *mut u8, &bytes)?;
            }
            conf.ifc_len = IFREQ_SIZE as i32;
            write_user_value(token, argp as *mut LinuxIfConf, &conf)?;
            Ok(0)
        }
        SIOCSIFFLAGS | SIOCSIFMTU => {
            let _ = read_user_value(token, argp as *const u8)?;
            Ok(0)
        }
        _ => Err(SysError::ENOTTY),
    }
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
    for days in month_days.iter().take(m as usize - 1) {
        tm_yday += days;
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
