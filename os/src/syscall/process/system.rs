use crate::sbi::shutdown;
use crate::syscall::errno::{SysError, SysResult};
use crate::syscall::user_ptr::{copy_to_user, write_user_value};
use crate::task::{current_process, current_user_token};
use crate::timer::get_time_clock_ticks;

const UTS_FIELD_LEN: usize = 65;
const LINUX_REBOOT_MAGIC1: u32 = 0xfee1_dead;
const LINUX_REBOOT_MAGIC2: u32 = 0x2812_1969;
const LINUX_REBOOT_MAGIC2A: u32 = 0x0512_1996;
const LINUX_REBOOT_MAGIC2B: u32 = 0x1604_1998;
const LINUX_REBOOT_MAGIC2C: u32 = 0x2011_2000;
const LINUX_REBOOT_CMD_RESTART: u32 = 0x0123_4567;
const LINUX_REBOOT_CMD_HALT: u32 = 0xcdef_0123;
const LINUX_REBOOT_CMD_CAD_ON: u32 = 0x89ab_cdef;
const LINUX_REBOOT_CMD_CAD_OFF: u32 = 0x0000_0000;
const LINUX_REBOOT_CMD_POWER_OFF: u32 = 0x4321_fedc;
const GRND_NONBLOCK: u32 = 0x0001;
const GRND_RANDOM: u32 = 0x0002;
const GRND_INSECURE: u32 = 0x0004;
const GRND_SUPPORTED: u32 = GRND_NONBLOCK | GRND_RANDOM | GRND_INSECURE;
const GETRANDOM_CHUNK: usize = 64;
const SYSLOG_ACTION_READ_ALL: usize = 3;
const SYSLOG_ACTION_SIZE_BUFFER: usize = 10;
const SYSLOG_BUF_SIZE: usize = 4096;
const PERSONALITY_QUERY: usize = 0xffff_ffff;
const PER_LINUX: u32 = 0;
const PER_MASK: u32 = 0xff;
const UNAME26: u32 = 0x0002_0000;
const UNAME26_RELEASE: &str = "2.6.60";

static SYSLOG_FAKE_MSG: &[u8] = b"<5>[    0.000000] Linux version 5.10.0 (whusp@oscomp)\n";

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct LinuxUtsName {
    sysname: [u8; UTS_FIELD_LEN],
    nodename: [u8; UTS_FIELD_LEN],
    release: [u8; UTS_FIELD_LEN],
    version: [u8; UTS_FIELD_LEN],
    machine: [u8; UTS_FIELD_LEN],
    domainname: [u8; UTS_FIELD_LEN],
}

impl LinuxUtsName {
    fn field(value: &str) -> [u8; UTS_FIELD_LEN] {
        let mut field = [0u8; UTS_FIELD_LEN];
        let bytes = value.as_bytes();
        let len = bytes.len().min(UTS_FIELD_LEN - 1);
        field[..len].copy_from_slice(&bytes[..len]);
        field
    }

    fn current() -> Self {
        Self {
            sysname: Self::field("Linux"),
            nodename: Self::field("WHUSP"),
            release: Self::field("6.8.0-whusp"),
            version: Self::field("#1 SMP OSKernel2026"),
            machine: Self::field(machine_name()),
            domainname: Self::field("(none)"),
        }
    }

    fn current_for_personality(personality: u32) -> Self {
        let mut uts = Self::current();
        if personality & UNAME26 != 0 {
            uts.release = Self::field(UNAME26_RELEASE);
        }
        uts
    }
}

#[cfg(target_arch = "loongarch64")]
fn machine_name() -> &'static str {
    "loongarch64"
}

#[cfg(not(target_arch = "loongarch64"))]
fn machine_name() -> &'static str {
    "riscv64"
}

fn has_linux_reboot_magic(magic: u32, magic2: u32) -> bool {
    magic == LINUX_REBOOT_MAGIC1
        && matches!(
            magic2,
            LINUX_REBOOT_MAGIC2
                | LINUX_REBOOT_MAGIC2A
                | LINUX_REBOOT_MAGIC2B
                | LINUX_REBOOT_MAGIC2C
        )
}

pub fn sys_reboot(magic: usize, magic2: usize, op: usize, _arg: usize) -> SysResult {
    let magic = magic as u32;
    let magic2 = magic2 as u32;
    let op = op as u32;
    if !has_linux_reboot_magic(magic, magic2) {
        return Err(SysError::EINVAL);
    }

    // UNFINISHED: Linux requires CAP_SYS_BOOT in the caller's user namespace
    // and returns EPERM for unprivileged callers. This kernel has no real
    // credential or capability model yet and runs contest user tasks as root.
    match op {
        LINUX_REBOOT_CMD_CAD_OFF | LINUX_REBOOT_CMD_CAD_ON => Ok(0),
        LINUX_REBOOT_CMD_HALT | LINUX_REBOOT_CMD_POWER_OFF | LINUX_REBOOT_CMD_RESTART => {
            // UNFINISHED: RESTART should reset and reboot the machine. The
            // current arch layer exposes only a shutdown/poweroff primitive,
            // which is the contest-critical behavior under QEMU -no-reboot.
            // CONTEXT: This path terminates the VM immediately. Finalize
            // mounted backends so lwext4 can mark superblocks clean after
            // user space has issued sync(2); normal sync(2) remains a plain
            // writeback operation while the filesystem is still mounted.
            crate::fs::shutdown_all_mounts();
            shutdown(false)
        }
        // UNFINISHED: RESTART2, KEXEC, and SW_SUSPEND require reboot strings,
        // kernel-image handoff, or suspend support that this kernel lacks.
        _ => Err(SysError::EINVAL),
    }
}

pub fn sys_uname(name: *mut LinuxUtsName) -> SysResult {
    // UNFINISHED: UTS namespaces and sethostname/setdomainname are not
    // implemented. The personality support below is limited to the UNAME26
    // release override needed by Linux compatibility tests.
    let uts = LinuxUtsName::current_for_personality(current_process().personality());
    write_user_value(current_user_token(), name, &uts)?;
    Ok(0)
}

pub fn sys_personality(persona: usize) -> SysResult {
    let old = current_process().personality();
    if persona == PERSONALITY_QUERY {
        return Ok(old as isize);
    }

    let persona = persona as u32;
    // CONTEXT: Most execution-domain flags have no effect in this kernel. For
    // now accept only PER_LINUX plus UNAME26, which is the ABI surface needed by
    // uname04 and avoids pretending to support broader personality emulation.
    if persona & !(PER_MASK | UNAME26) != 0 || persona & PER_MASK != PER_LINUX {
        return Err(SysError::EINVAL);
    }

    current_process().set_personality(persona);
    Ok(old as isize)
}

pub fn sys_getrandom(buf: *mut u8, len: usize, flags: u32) -> SysResult {
    if flags & !GRND_SUPPORTED != 0 {
        return Err(SysError::EINVAL);
    }
    if len == 0 {
        return Ok(0);
    }
    if len > isize::MAX as usize {
        return Err(SysError::EINVAL);
    }

    // CONTEXT: The contest kernel has no cryptographic entropy pool yet. Use a
    // deterministic per-call generator, matching the existing /dev/urandom
    // compatibility role well enough for libc seeding and getentropy-style
    // small reads.
    let token = current_user_token();
    let mut state = (get_time_clock_ticks() as u64)
        ^ ((current_process().getpid() as u64) << 32)
        ^ (buf as usize as u64)
        ^ (len as u64)
        ^ (flags as u64);
    let mut offset = 0usize;
    while offset < len {
        let chunk_len = (len - offset).min(GETRANDOM_CHUNK);
        let mut chunk = [0u8; GETRANDOM_CHUNK];
        for byte in &mut chunk[..chunk_len] {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(0x9e37_79b9_7f4a_7c15);
            *byte = (state >> 32) as u8;
        }
        copy_to_user(token, buf.wrapping_add(offset), &chunk[..chunk_len])?;
        offset += chunk_len;
    }
    Ok(len as isize)
}

pub fn sys_syslog(log_type: usize, buf: *mut u8, len: usize) -> SysResult {
    match log_type {
        SYSLOG_ACTION_SIZE_BUFFER => Ok(SYSLOG_BUF_SIZE as isize),
        SYSLOG_ACTION_READ_ALL => {
            if buf.is_null() || len == 0 {
                return Ok(0);
            }
            let token = current_user_token();
            let msg = SYSLOG_FAKE_MSG;
            let copy_len = msg.len().min(len);
            copy_to_user(token, buf, &msg[..copy_len])?;
            Ok(copy_len as isize)
        }
        _ => Ok(0),
    }
}
