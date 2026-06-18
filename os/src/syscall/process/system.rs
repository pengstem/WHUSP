use crate::sbi::shutdown;
use crate::syscall::SyscallContext;
use crate::syscall::errno::{SysError, SysResult};
#[cfg(target_arch = "riscv64")]
use crate::syscall::user_ptr::read_user_value_ctx;
use crate::syscall::user_ptr::{copy_to_user, copy_to_user_ctx, write_user_value_ctx};
use crate::task::{current_process, current_user_token, processes_snapshot};
use crate::timer::{get_time_clock_ticks, get_time_us};
use alloc::format;
use alloc::string::String;
use core::sync::atomic::{AtomicUsize, Ordering};
use log::warn;

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
const SYSLOG_ACTION_CLOSE: usize = 0;
const SYSLOG_ACTION_OPEN: usize = 1;
const SYSLOG_ACTION_READ: usize = 2;
const SYSLOG_ACTION_READ_ALL: usize = 3;
const SYSLOG_ACTION_READ_CLEAR: usize = 4;
const SYSLOG_ACTION_CLEAR: usize = 5;
const SYSLOG_ACTION_CONSOLE_OFF: usize = 6;
const SYSLOG_ACTION_CONSOLE_ON: usize = 7;
const SYSLOG_ACTION_CONSOLE_LEVEL: usize = 8;
const SYSLOG_ACTION_SIZE_UNREAD: usize = 9;
const SYSLOG_ACTION_SIZE_BUFFER: usize = 10;
const SYSLOG_BUF_SIZE: usize = 4096;
const SYSLOG_DEFAULT_MESSAGE_LEVEL: usize = 4;
const SYSLOG_MIN_CONSOLE_LEVEL: usize = 1;
const SYSLOG_DEFAULT_CONSOLE_LEVEL: usize = 7;
const SYSLOG_MAX_CONSOLE_LEVEL: usize = 8;
const PERSONALITY_QUERY: usize = 0xffff_ffff;
const PER_LINUX: u32 = 0;
const PER_MASK: u32 = 0xff;
const UNAME26: u32 = 0x0002_0000;
const UNAME26_RELEASE: &str = "2.6.60";
#[cfg(target_arch = "riscv64")]
const RISCV_HWPROBE_KEY_BASE_BEHAVIOR: i64 = 3;
#[cfg(target_arch = "riscv64")]
const RISCV_HWPROBE_BASE_BEHAVIOR_IMA: u64 = 1 << 0;
#[cfg(target_arch = "riscv64")]
const RISCV_HWPROBE_KEY_IMA_EXT_0: i64 = 4;

static SYSLOG_FAKE_MSG: &[u8] = b"<5>[    0.000000] Linux version 5.10.0 (whusp@oscomp)\n";
static SYSLOG_CONSOLE_LEVEL: AtomicUsize = AtomicUsize::new(SYSLOG_DEFAULT_CONSOLE_LEVEL);
static SYSLOG_SAVED_CONSOLE_LEVEL: AtomicUsize = AtomicUsize::new(SYSLOG_DEFAULT_CONSOLE_LEVEL);

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

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct LinuxSysInfo {
    uptime: isize,
    loads: [usize; 3],
    totalram: usize,
    freeram: usize,
    sharedram: usize,
    bufferram: usize,
    totalswap: usize,
    freeswap: usize,
    procs: u16,
    pad: u16,
    totalhigh: usize,
    freehigh: usize,
    mem_unit: u32,
}

#[cfg(target_arch = "riscv64")]
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
struct RiscvHwprobe {
    key: i64,
    value: u64,
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
            if let Err(err) = crate::fs::shutdown_all_mounts() {
                warn!("shutdown_all_mounts failed: {err:?}");
            }
            shutdown(false)
        }
        // UNFINISHED: RESTART2, KEXEC, and SW_SUSPEND require reboot strings,
        // kernel-image handoff, or suspend support that this kernel lacks.
        _ => Err(SysError::EINVAL),
    }
}

pub fn sys_uname_ctx(ctx: &SyscallContext, name: *mut LinuxUtsName) -> SysResult {
    // UNFINISHED: UTS namespaces and sethostname/setdomainname are not
    // implemented. The personality support below is limited to the UNAME26
    // release override needed by Linux compatibility tests.
    let uts = LinuxUtsName::current_for_personality(ctx.process().personality());
    write_user_value_ctx(ctx, name, &uts)?;
    Ok(0)
}

pub fn sys_sysinfo_ctx(ctx: &SyscallContext, info: *mut LinuxSysInfo) -> SysResult {
    let value = LinuxSysInfo {
        uptime: (get_time_us() / 1_000_000) as isize,
        totalram: 1024 * 1024 * 1024,
        freeram: 900 * 1024 * 1024,
        totalswap: 2 * 1024 * 1024 * 1024,
        freeswap: 2 * 1024 * 1024 * 1024,
        procs: processes_snapshot().len().min(u16::MAX as usize) as u16,
        mem_unit: 1,
        ..LinuxSysInfo::default()
    };
    write_user_value_ctx(ctx, info, &value)?;
    Ok(0)
}

#[cfg(target_arch = "riscv64")]
fn riscv_hwprobe_pair_ptr(pairs: *mut RiscvHwprobe, index: usize) -> SysResult<*mut RiscvHwprobe> {
    let offset = index
        .checked_mul(core::mem::size_of::<RiscvHwprobe>())
        .ok_or(SysError::EFAULT)?;
    let addr = (pairs as usize)
        .checked_add(offset)
        .ok_or(SysError::EFAULT)?;
    Ok(addr as *mut RiscvHwprobe)
}

#[cfg(target_arch = "riscv64")]
fn fill_riscv_hwprobe_pair(pair: &mut RiscvHwprobe) {
    match pair.key {
        RISCV_HWPROBE_KEY_BASE_BEHAVIOR => {
            pair.value = RISCV_HWPROBE_BASE_BEHAVIOR_IMA;
        }
        RISCV_HWPROBE_KEY_IMA_EXT_0 => {
            pair.value = 0;
        }
        _ => {
            pair.key = -1;
            pair.value = 0;
        }
    }
}

#[cfg(target_arch = "riscv64")]
pub fn sys_riscv_hwprobe_ctx(
    ctx: &SyscallContext,
    pairs: *mut u8,
    pair_count: usize,
    cpuset_size: usize,
    cpus: usize,
    flags: u32,
) -> SysResult {
    if flags != 0 {
        return Err(SysError::EINVAL);
    }
    if cpuset_size != 0 || cpus != 0 {
        return Err(SysError::EINVAL);
    }
    if pair_count == 0 {
        return Ok(0);
    }
    if pairs.is_null() {
        return Err(SysError::EFAULT);
    }

    let pairs = pairs.cast::<RiscvHwprobe>();
    for index in 0..pair_count {
        let pair_ptr = riscv_hwprobe_pair_ptr(pairs, index)?;
        let mut pair = read_user_value_ctx(ctx, pair_ptr as *const RiscvHwprobe)?;
        // CONTEXT: This conservative RISC-V hwprobe subset supports only the
        // all-online-CPU shortcut on the contest single-hart kernel. It reports
        // base IMA behavior and deliberately under-reports optional extensions
        // until the corresponding arch state is modeled.
        fill_riscv_hwprobe_pair(&mut pair);
        write_user_value_ctx(ctx, pair_ptr, &pair)?;
    }
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

pub fn sys_getrandom_ctx(ctx: &SyscallContext, buf: *mut u8, len: usize, flags: u32) -> SysResult {
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
    let mut state = (get_time_clock_ticks() as u64)
        ^ ((ctx.process().getpid() as u64) << 32)
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
        copy_to_user_ctx(ctx, buf.wrapping_add(offset), &chunk[..chunk_len])?;
        offset += chunk_len;
    }
    Ok(len as isize)
}

pub(crate) fn proc_sys_kernel_printk_content() -> String {
    format!(
        "{}\t{}\t{}\t{}\n",
        SYSLOG_CONSOLE_LEVEL.load(Ordering::Relaxed),
        SYSLOG_DEFAULT_MESSAGE_LEVEL,
        SYSLOG_MIN_CONSOLE_LEVEL,
        SYSLOG_DEFAULT_CONSOLE_LEVEL
    )
}

pub(crate) fn write_proc_sys_kernel_printk(buf: &[u8], offset: u64) -> usize {
    if offset != 0 {
        return 0;
    }
    let Ok(text) = core::str::from_utf8(buf) else {
        return 0;
    };
    let Some(level) = text
        .split_whitespace()
        .next()
        .and_then(|value| value.parse::<usize>().ok())
    else {
        return 0;
    };
    if !(SYSLOG_MIN_CONSOLE_LEVEL..=SYSLOG_MAX_CONSOLE_LEVEL).contains(&level) {
        return 0;
    }
    SYSLOG_CONSOLE_LEVEL.store(level, Ordering::Relaxed);
    buf.len()
}

fn validate_syslog_size(len: usize) -> SysResult<usize> {
    if len > i32::MAX as usize {
        return Err(SysError::EINVAL);
    }
    Ok(len)
}

fn validate_syslog_read_args(buf: *mut u8, len: usize) -> SysResult<usize> {
    let len = validate_syslog_size(len)?;
    if buf.is_null() {
        return Err(SysError::EINVAL);
    }
    Ok(len)
}

fn current_can_use_privileged_syslog() -> bool {
    // UNFINISHED: Linux checks CAP_SYSLOG or CAP_SYS_ADMIN in the caller's user
    // namespace. The current credential model uses effective uid 0 as the
    // visible privileged boundary for LTP set[e]uid transitions.
    current_process().credentials().euid == 0
}

fn syslog_action_requires_privilege(log_type: usize) -> bool {
    !matches!(log_type, SYSLOG_ACTION_READ_ALL | SYSLOG_ACTION_SIZE_BUFFER)
}

fn syslog_copy_fake_log(buf: *mut u8, len: usize) -> SysResult {
    if len == 0 {
        return Ok(0);
    }
    let copy_len = SYSLOG_FAKE_MSG.len().min(len);
    copy_to_user(current_user_token(), buf, &SYSLOG_FAKE_MSG[..copy_len])?;
    Ok(copy_len as isize)
}

pub fn sys_syslog(log_type: usize, buf: *mut u8, len: usize) -> SysResult {
    match log_type {
        SYSLOG_ACTION_CLOSE
        | SYSLOG_ACTION_OPEN
        | SYSLOG_ACTION_CLEAR
        | SYSLOG_ACTION_CONSOLE_OFF
        | SYSLOG_ACTION_CONSOLE_ON
        | SYSLOG_ACTION_SIZE_UNREAD
        | SYSLOG_ACTION_SIZE_BUFFER => {}
        SYSLOG_ACTION_READ | SYSLOG_ACTION_READ_ALL | SYSLOG_ACTION_READ_CLEAR => {
            validate_syslog_read_args(buf, len)?;
        }
        SYSLOG_ACTION_CONSOLE_LEVEL => {
            let level = validate_syslog_size(len)?;
            if !(SYSLOG_MIN_CONSOLE_LEVEL..=SYSLOG_MAX_CONSOLE_LEVEL).contains(&level) {
                return Err(SysError::EINVAL);
            }
        }
        _ => return Err(SysError::EINVAL),
    }

    if syslog_action_requires_privilege(log_type) && !current_can_use_privileged_syslog() {
        return Err(SysError::EPERM);
    }

    match log_type {
        SYSLOG_ACTION_CLOSE | SYSLOG_ACTION_OPEN | SYSLOG_ACTION_CLEAR => Ok(0),
        SYSLOG_ACTION_READ | SYSLOG_ACTION_READ_ALL | SYSLOG_ACTION_READ_CLEAR => {
            syslog_copy_fake_log(buf, validate_syslog_size(len)?)
        }
        SYSLOG_ACTION_CONSOLE_OFF => {
            let previous = SYSLOG_CONSOLE_LEVEL.swap(SYSLOG_MIN_CONSOLE_LEVEL, Ordering::Relaxed);
            SYSLOG_SAVED_CONSOLE_LEVEL.store(previous, Ordering::Relaxed);
            Ok(0)
        }
        SYSLOG_ACTION_CONSOLE_ON => {
            let saved = SYSLOG_SAVED_CONSOLE_LEVEL.load(Ordering::Relaxed);
            SYSLOG_CONSOLE_LEVEL.store(saved, Ordering::Relaxed);
            Ok(0)
        }
        SYSLOG_ACTION_CONSOLE_LEVEL => {
            let level = validate_syslog_size(len)?;
            SYSLOG_CONSOLE_LEVEL.store(level, Ordering::Relaxed);
            Ok(0)
        }
        SYSLOG_ACTION_SIZE_UNREAD => Ok(SYSLOG_FAKE_MSG.len() as isize),
        SYSLOG_ACTION_SIZE_BUFFER => Ok(SYSLOG_BUF_SIZE as isize),
        _ => Err(SysError::EINVAL),
    }
}
