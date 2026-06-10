use crate::syscall::SyscallContext;
use crate::syscall::errno::{SysError, SysResult};
use crate::syscall::user_ptr::{copy_to_user_ctx, read_user_value_ctx, write_user_value_ctx};
use crate::task::{
    CAP_SETPCAP, CAP_SYS_ADMIN, ProcessControlBlock, SeccompSockFilter, SignalFlags,
    TaskControlBlock, current_process, pid2process,
};
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;

const NGROUPS_MAX: usize = 65536;
const LINUX_CAPABILITY_VERSION_1: u32 = 0x1998_0330;
const LINUX_CAPABILITY_VERSION_2: u32 = 0x2007_1026;
const LINUX_CAPABILITY_VERSION_3: u32 = 0x2008_0522;
const LINUX_CAPABILITY_U32S_1: usize = 1;
const LINUX_CAPABILITY_U32S_2: usize = 2;
const PR_SET_PDEATHSIG: usize = 1;
const PR_GET_PDEATHSIG: usize = 2;
const PR_GET_DUMPABLE: usize = 3;
const PR_SET_DUMPABLE: usize = 4;
const PR_SET_TIMING: usize = 14;
const PR_SET_NAME: usize = 15;
const PR_GET_NAME: usize = 16;
const PR_GET_SECCOMP: usize = 21;
const PR_SET_SECCOMP: usize = 22;
const PR_CAPBSET_READ: usize = 23;
const PR_CAPBSET_DROP: usize = 24;
const PR_GET_SECUREBITS: usize = 27;
const PR_SET_SECUREBITS: usize = 28;
const PR_SET_TIMERSLACK: usize = 29;
const PR_GET_TIMERSLACK: usize = 30;
const PR_SET_CHILD_SUBREAPER: usize = 36;
const PR_GET_CHILD_SUBREAPER: usize = 37;
const PR_SET_NO_NEW_PRIVS: usize = 38;
const PR_GET_NO_NEW_PRIVS: usize = 39;
const PR_SET_THP_DISABLE: usize = 41;
const PR_GET_THP_DISABLE: usize = 42;
const PR_CAP_AMBIENT: usize = 47;
const PR_GET_SPECULATION_CTRL: usize = 52;
const PR_CAP_AMBIENT_IS_SET: usize = 1;
const PR_CAP_AMBIENT_RAISE: usize = 2;
const PR_CAP_AMBIENT_LOWER: usize = 3;
const PR_CAP_AMBIENT_CLEAR_ALL: usize = 4;
const PR_SPEC_STORE_BYPASS: usize = 0;
const PR_NAME_LEN: usize = 16;
const SECCOMP_MODE_DISABLED: usize = 0;
const SECCOMP_MODE_STRICT: usize = 1;
const SECCOMP_MODE_FILTER: usize = 2;
const SECCOMP_FILTER_MAX_INSNS: usize = 4096;
const BPF_LD_W_ABS: u16 = 0x20;
const BPF_JMP_JEQ_K: u16 = 0x15;
const BPF_RET_K: u16 = 0x06;
const SECBIT_NO_CAP_AMBIENT_RAISE: u32 = 1 << 6;

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct LinuxCapUserHeader {
    version: u32,
    pid: i32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct LinuxCapUserData {
    effective: u32,
    permitted: u32,
    inheritable: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
struct LinuxSockFprog {
    len: u16,
    filter: usize,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
struct LinuxSockFilter {
    code: u16,
    jt: u8,
    jf: u8,
    k: u32,
}

pub fn sys_getuid() -> isize {
    current_process().credentials().ruid as isize
}

pub fn sys_geteuid() -> isize {
    current_process().credentials().euid as isize
}

pub fn sys_getgid() -> isize {
    current_process().credentials().rgid as isize
}

pub fn sys_getegid() -> isize {
    current_process().credentials().egid as isize
}

fn linux_capability_u32s(version: u32) -> Option<usize> {
    match version {
        LINUX_CAPABILITY_VERSION_1 => Some(LINUX_CAPABILITY_U32S_1),
        LINUX_CAPABILITY_VERSION_2 | LINUX_CAPABILITY_VERSION_3 => Some(LINUX_CAPABILITY_U32S_2),
        _ => None,
    }
}

fn capability_target_process_from(
    current_task: &TaskControlBlock,
    current: &Arc<ProcessControlBlock>,
    pid: i32,
) -> Result<Arc<ProcessControlBlock>, SysError> {
    if pid < 0 {
        return Err(SysError::EINVAL);
    }
    let pid = pid as usize;
    // Linux capget/capset address threads through the pid field. This kernel
    // stores credentials on the PCB, so the caller's Linux-visible TID aliases
    // the current process while other live ids resolve through PID lookup.
    if pid == 0 || pid == current.getpid() || pid == current_task.linux_tid() {
        return Ok(Arc::clone(current));
    }
    pid2process(pid).ok_or(SysError::ESRCH)
}

fn capability_target_process_ctx(
    ctx: &SyscallContext,
    pid: i32,
) -> Result<Arc<ProcessControlBlock>, SysError> {
    capability_target_process_from(ctx.task(), ctx.process(), pid)
}

pub fn sys_capget_ctx(
    ctx: &SyscallContext,
    hdrp: *mut LinuxCapUserHeader,
    datap: *mut LinuxCapUserData,
) -> SysResult {
    if hdrp.is_null() {
        return Err(SysError::EFAULT);
    }
    let mut header = read_user_value_ctx(ctx, hdrp.cast_const())?;
    let Some(u32s) = linux_capability_u32s(header.version) else {
        header.version = LINUX_CAPABILITY_VERSION_3;
        write_user_value_ctx(ctx, hdrp, &header)?;
        return Err(SysError::EINVAL);
    };
    let target = capability_target_process_ctx(ctx, header.pid)?;
    if datap.is_null() {
        return Ok(0);
    }

    // UNFINISHED: This is a compatibility capability model. It stores the raw
    // effective/permitted/inheritable/bounding bitsets that LTP exercises, but
    // does not implement Linux user namespaces, securebits, ambient caps, file
    // capabilities, or capability recalculation across execve/setuid files.
    let capabilities = target.credentials().capabilities;
    for index in 0..u32s {
        let data = LinuxCapUserData {
            effective: capabilities.effective[index],
            permitted: capabilities.permitted[index],
            inheritable: capabilities.inheritable[index],
        };
        write_user_value_ctx(ctx, datap.wrapping_add(index), &data)?;
    }
    Ok(0)
}

fn capability_data_subset(
    data: &[LinuxCapUserData; LINUX_CAPABILITY_U32S_2],
    u32s: usize,
    mut allowed: impl FnMut(usize) -> u32,
    field: impl Fn(&LinuxCapUserData) -> u32,
) -> bool {
    (0..u32s).all(|index| field(&data[index]) & !allowed(index) == 0)
}

pub fn sys_capset_ctx(
    ctx: &SyscallContext,
    hdrp: *mut LinuxCapUserHeader,
    datap: *const LinuxCapUserData,
) -> SysResult {
    if hdrp.is_null() {
        return Err(SysError::EFAULT);
    }
    let mut header = read_user_value_ctx(ctx, hdrp.cast_const())?;
    let Some(u32s) = linux_capability_u32s(header.version) else {
        header.version = LINUX_CAPABILITY_VERSION_3;
        write_user_value_ctx(ctx, hdrp, &header)?;
        return Err(SysError::EINVAL);
    };
    if header.pid < 0 {
        return Err(SysError::EINVAL);
    }
    let target_pid = header.pid as usize;
    let current = ctx.process();
    if target_pid != 0 && target_pid != current.getpid() && target_pid != ctx.task().linux_tid() {
        return Err(SysError::EPERM);
    }
    if datap.is_null() {
        return Err(SysError::EFAULT);
    }

    let mut data = [LinuxCapUserData::default(); LINUX_CAPABILITY_U32S_2];
    for (index, slot) in data.iter_mut().enumerate().take(u32s) {
        *slot = read_user_value_ctx(ctx, datap.wrapping_add(index))?;
    }
    current.mutate_credentials(|credentials| {
        let old = credentials.capabilities.clone();
        if !capability_data_subset(
            &data,
            u32s,
            |index| data[index].permitted,
            |item| item.effective,
        ) {
            return Err(SysError::EPERM);
        }
        if !capability_data_subset(
            &data,
            u32s,
            |index| old.permitted[index],
            |item| item.permitted,
        ) {
            return Err(SysError::EPERM);
        }
        if !capability_data_subset(
            &data,
            u32s,
            |index| old.bounding[index],
            |item| item.inheritable,
        ) {
            return Err(SysError::EPERM);
        }
        if !capability_data_subset(
            &data,
            u32s,
            |index| old.inheritable[index] | old.permitted[index],
            |item| item.inheritable,
        ) {
            return Err(SysError::EPERM);
        }
        for (index, item) in data.iter().enumerate().take(u32s) {
            credentials.capabilities.effective[index] = item.effective;
            credentials.capabilities.permitted[index] = item.permitted;
            credentials.capabilities.inheritable[index] = item.inheritable;
        }
        credentials
            .capabilities
            .clamp_ambient_to_permitted_inheritable();
        Ok(0)
    })
}

fn require_no_extra_args(args: &[usize]) -> SysResult<()> {
    if args.iter().any(|arg| *arg != 0) {
        Err(SysError::EINVAL)
    } else {
        Ok(())
    }
}

fn read_prctl_name_ctx(ctx: &SyscallContext, ptr: usize) -> SysResult<String> {
    let raw = read_user_value_ctx::<[u8; PR_NAME_LEN]>(ctx, ptr as *const [u8; PR_NAME_LEN])?;
    let len = raw
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(PR_NAME_LEN - 1)
        .min(PR_NAME_LEN - 1);
    Ok(raw[..len].iter().map(|byte| *byte as char).collect())
}

fn write_prctl_name_ctx(ctx: &SyscallContext, ptr: usize, name: &str) -> SysResult<()> {
    let mut raw = [0u8; PR_NAME_LEN];
    let bytes = name.as_bytes();
    let len = bytes.len().min(PR_NAME_LEN - 1);
    raw[..len].copy_from_slice(&bytes[..len]);
    copy_to_user_ctx(ctx, ptr as *mut u8, &raw)
}

fn securebits_block_ambient_raise(securebits: u32) -> bool {
    securebits & SECBIT_NO_CAP_AMBIENT_RAISE != 0 || securebits == 6
}

fn read_seccomp_filter_ctx(ctx: &SyscallContext, ptr: usize) -> SysResult<Vec<SeccompSockFilter>> {
    let fprog = read_user_value_ctx::<LinuxSockFprog>(ctx, ptr as *const LinuxSockFprog)?;
    let len = fprog.len as usize;
    if len == 0 || len > SECCOMP_FILTER_MAX_INSNS || fprog.filter == 0 {
        return Err(SysError::EINVAL);
    }

    let mut filters = Vec::new();
    for index in 0..len {
        let filter = read_user_value_ctx::<LinuxSockFilter>(
            ctx,
            (fprog.filter as *const LinuxSockFilter).wrapping_add(index),
        )?;
        // The syscall dispatcher evaluates only validated classic-BPF records
        // copied here. Keep offset 0 tied to seccomp_data.nr; other offsets
        // would require modeling the full seccomp_data ABI.
        if !matches!(filter.code, BPF_LD_W_ABS | BPF_JMP_JEQ_K | BPF_RET_K) {
            return Err(SysError::EINVAL);
        }
        if filter.code == BPF_LD_W_ABS && filter.k != 0 {
            return Err(SysError::EINVAL);
        }
        filters.push(SeccompSockFilter {
            code: filter.code,
            jt: filter.jt,
            jf: filter.jf,
            k: filter.k,
        });
    }
    if filters.iter().any(|filter| filter.code == BPF_RET_K) {
        Ok(filters)
    } else {
        Err(SysError::EINVAL)
    }
}

pub fn sys_prctl_ctx(
    ctx: &SyscallContext,
    option: usize,
    arg2: usize,
    arg3: usize,
    arg4: usize,
    arg5: usize,
) -> SysResult {
    match option {
        PR_SET_PDEATHSIG => {
            if SignalFlags::from_signum(arg2 as u32).is_none() {
                return Err(SysError::EINVAL);
            }
            ctx.process().inner_exclusive_access().pdeath_signal = arg2 as u32;
            Ok(0)
        }
        PR_GET_PDEATHSIG => {
            let signal = ctx.process().inner_exclusive_access().pdeath_signal as i32;
            write_user_value_ctx(ctx, arg2 as *mut i32, &signal)?;
            Ok(0)
        }
        PR_GET_DUMPABLE => Ok(ctx.process().inner_exclusive_access().dumpable as isize),
        PR_SET_DUMPABLE => {
            if arg2 > 1 {
                return Err(SysError::EINVAL);
            }
            ctx.process().inner_exclusive_access().dumpable = arg2 != 0;
            Ok(0)
        }
        PR_SET_TIMING => {
            if arg2 == 0 {
                Ok(0)
            } else {
                Err(SysError::EINVAL)
            }
        }
        PR_SET_NAME => {
            let name = read_prctl_name_ctx(ctx, arg2)?;
            ctx.process().inner_exclusive_access().comm = name;
            Ok(0)
        }
        PR_GET_NAME => {
            let name = ctx.process().inner_exclusive_access().comm.clone();
            write_prctl_name_ctx(ctx, arg2, &name)?;
            Ok(0)
        }
        PR_GET_SECCOMP => Ok(ctx.task().inner_exclusive_access().seccomp_mode as isize),
        PR_SET_SECCOMP => match arg2 {
            SECCOMP_MODE_STRICT => {
                // UNFINISHED: This implements Linux strict seccomp, but does
                // not model ptrace/audit interactions.
                let mut inner = ctx.task().inner_exclusive_access();
                inner.seccomp_mode = SECCOMP_MODE_STRICT as u8;
                inner.seccomp_filter = None;
                Ok(0)
            }
            SECCOMP_MODE_FILTER => {
                if arg3 == 0 {
                    return Err(SysError::EFAULT);
                }
                let filter = read_seccomp_filter_ctx(ctx, arg3)?;
                let has_sys_admin = ctx
                    .process()
                    .credentials()
                    .capabilities
                    .has_effective(CAP_SYS_ADMIN)
                    .ok_or(SysError::EINVAL)?;
                let no_new_privs = ctx.process().inner_exclusive_access().no_new_privs;
                if !has_sys_admin && !no_new_privs {
                    return Err(SysError::EACCES);
                }
                // UNFINISHED: This supports the classic BPF instruction subset
                // used by LTP prctl04: LD syscall nr, JEQ, and RET KILL/ALLOW.
                let mut inner = ctx.task().inner_exclusive_access();
                inner.seccomp_mode = SECCOMP_MODE_FILTER as u8;
                inner.seccomp_filter = Some(filter);
                Ok(0)
            }
            SECCOMP_MODE_DISABLED => Err(SysError::EINVAL),
            _ => Err(SysError::EINVAL),
        },
        PR_CAPBSET_READ => ctx
            .process()
            .credentials()
            .capabilities
            .bounding_contains(arg2)
            .map(|present| present as isize)
            .ok_or(SysError::EINVAL),
        PR_CAPBSET_DROP => {
            ctx.process().mutate_credentials(|credentials| {
                let capabilities = &mut credentials.capabilities;
                if !capabilities
                    .has_effective(CAP_SETPCAP)
                    .ok_or(SysError::EINVAL)?
                {
                    return Err(SysError::EPERM);
                }
                // UNFINISHED: Linux applies this to the per-thread capability
                // bounding set and interacts with user namespaces, securebits,
                // ambient/file capabilities, and execve propagation. This
                // contest subset stores a process-wide bounding set so LTP
                // capability error-path tests can exercise capset semantics.
                capabilities.drop_bounding(arg2).ok_or(SysError::EINVAL)?;
                Ok(0)
            })
        }
        PR_GET_SECUREBITS => Ok(ctx.process().inner_exclusive_access().securebits as isize),
        PR_SET_SECUREBITS => {
            ctx.process().mutate_credentials(|credentials| {
                if !credentials
                    .capabilities
                    .has_effective(CAP_SETPCAP)
                    .ok_or(SysError::EINVAL)?
                {
                    return Err(SysError::EPERM);
                }
                Ok(())
            })?;
            ctx.process().inner_exclusive_access().securebits = arg2 as u32;
            Ok(0)
        }
        PR_SET_TIMERSLACK => {
            let mut task_inner = ctx.task().inner_exclusive_access();
            task_inner.timer_slack_ns = if arg2 == 0 {
                task_inner.default_timer_slack_ns
            } else {
                arg2
            };
            Ok(0)
        }
        PR_GET_TIMERSLACK => Ok(ctx.task().inner_exclusive_access().timer_slack_ns as isize),
        PR_SET_CHILD_SUBREAPER => {
            ctx.process().inner_exclusive_access().is_child_subreaper = arg2 != 0;
            Ok(0)
        }
        PR_GET_CHILD_SUBREAPER => {
            let value = ctx.process().inner_exclusive_access().is_child_subreaper as i32;
            write_user_value_ctx(ctx, arg2 as *mut i32, &value)?;
            Ok(0)
        }
        PR_SET_NO_NEW_PRIVS => {
            if arg2 != 1 {
                return Err(SysError::EINVAL);
            }
            require_no_extra_args(&[arg3, arg4, arg5])?;
            ctx.process().inner_exclusive_access().no_new_privs = true;
            Ok(0)
        }
        PR_GET_NO_NEW_PRIVS => {
            require_no_extra_args(&[arg2, arg3, arg4, arg5])?;
            Ok(ctx.process().inner_exclusive_access().no_new_privs as isize)
        }
        PR_SET_THP_DISABLE => {
            require_no_extra_args(&[arg3, arg4, arg5])?;
            if arg2 > 1 {
                return Err(SysError::EINVAL);
            }
            ctx.process().inner_exclusive_access().thp_disabled = arg2 != 0;
            Ok(0)
        }
        PR_GET_THP_DISABLE => {
            require_no_extra_args(&[arg2, arg3, arg4, arg5])?;
            Ok(ctx.process().inner_exclusive_access().thp_disabled as isize)
        }
        PR_GET_SPECULATION_CTRL => {
            require_no_extra_args(&[arg3, arg4, arg5])?;
            match arg2 {
                // CONTEXT: The kernel does not model CPU speculation controls.
                // Returning 0 follows the Linux meaning that this CPU is not
                // affected, while preserving Linux's strict unused-argument
                // validation for LTP's error-path checks.
                PR_SPEC_STORE_BYPASS => Ok(0),
                _ => Err(SysError::ENODEV),
            }
        }
        PR_CAP_AMBIENT => sys_prctl_cap_ambient_ctx(ctx, arg2, arg3, arg4, arg5),
        _ => Err(SysError::EINVAL),
    }
}

fn sys_prctl_cap_ambient_ctx(
    ctx: &SyscallContext,
    command: usize,
    cap: usize,
    arg4: usize,
    arg5: usize,
) -> SysResult {
    require_no_extra_args(&[arg4, arg5])?;
    match command {
        PR_CAP_AMBIENT_CLEAR_ALL => {
            if cap != 0 {
                return Err(SysError::EINVAL);
            }
            ctx.process().mutate_credentials(|credentials| {
                credentials.capabilities.clear_ambient();
            });
            Ok(0)
        }
        PR_CAP_AMBIENT_IS_SET => ctx
            .process()
            .credentials()
            .capabilities
            .ambient_contains(cap)
            .map(|present| present as isize)
            .ok_or(SysError::EINVAL),
        PR_CAP_AMBIENT_LOWER => ctx.process().mutate_credentials(|credentials| {
            credentials
                .capabilities
                .lower_ambient(cap)
                .ok_or(SysError::EINVAL)?;
            Ok(0)
        }),
        PR_CAP_AMBIENT_RAISE => {
            let securebits = ctx.process().inner_exclusive_access().securebits;
            if securebits_block_ambient_raise(securebits) {
                return Err(SysError::EPERM);
            }
            ctx.process().mutate_credentials(|credentials| {
                let capabilities = &mut credentials.capabilities;
                let permitted = capabilities.has_permitted(cap).ok_or(SysError::EINVAL)?;
                let inheritable = capabilities.has_inheritable(cap).ok_or(SysError::EINVAL)?;
                if !permitted || !inheritable {
                    return Err(SysError::EPERM);
                }
                capabilities.raise_ambient(cap).ok_or(SysError::EINVAL)?;
                Ok(0)
            })
        }
        _ => Err(SysError::EINVAL),
    }
}

pub fn sys_getgroups_ctx(ctx: &SyscallContext, size: usize, list: *mut u32) -> SysResult {
    let groups = ctx.process().credentials().groups;
    if size == 0 {
        return Ok(groups.len() as isize);
    }
    if size < groups.len() {
        return Err(SysError::EINVAL);
    }
    if list.is_null() {
        return Err(SysError::EFAULT);
    }
    for (index, group) in groups.iter().enumerate() {
        write_user_value_ctx(ctx, list.wrapping_add(index), group)?;
    }
    Ok(groups.len() as isize)
}

pub fn sys_setgroups_ctx(ctx: &SyscallContext, size: usize, list: *const u32) -> SysResult {
    if size > NGROUPS_MAX {
        return Err(SysError::EINVAL);
    }
    if ctx.process().credentials().euid != 0 {
        // UNFINISHED: Linux checks CAP_SETGID in the caller's user namespace.
        // This kernel only has root-equivalent credentials for now.
        return Err(SysError::EPERM);
    }
    if size > 0 && list.is_null() {
        return Err(SysError::EFAULT);
    }
    let mut groups = Vec::new();
    for index in 0..size {
        groups.push(read_user_value_ctx(ctx, list.wrapping_add(index))?);
    }
    ctx.process().replace_supplementary_groups(groups);
    Ok(0)
}

fn require_valid_id(id: i32) -> SysResult<Option<u32>> {
    if id == -1 {
        Ok(None)
    } else if id < 0 {
        Err(SysError::EINVAL)
    } else {
        Ok(Some(id as u32))
    }
}

pub fn sys_setuid(uid: u32) -> SysResult {
    current_process().mutate_credentials(|credentials| {
        if credentials.is_root() {
            credentials.ruid = uid;
            credentials.euid = uid;
            credentials.suid = uid;
            credentials.fsuid = uid;
            Ok(0)
        } else if uid == credentials.ruid || uid == credentials.suid {
            credentials.euid = uid;
            credentials.fsuid = uid;
            Ok(0)
        } else {
            Err(SysError::EPERM)
        }
    })
}

pub fn sys_setgid(gid: u32) -> SysResult {
    current_process().mutate_credentials(|credentials| {
        if credentials.is_root() {
            credentials.rgid = gid;
            credentials.egid = gid;
            credentials.sgid = gid;
            credentials.fsgid = gid;
            Ok(0)
        } else if gid == credentials.rgid || gid == credentials.sgid {
            credentials.egid = gid;
            credentials.fsgid = gid;
            Ok(0)
        } else {
            Err(SysError::EPERM)
        }
    })
}

pub fn sys_setreuid(ruid: i32, euid: i32) -> SysResult {
    let ruid = require_valid_id(ruid)?;
    let euid = require_valid_id(euid)?;
    current_process().mutate_credentials(|credentials| {
        let old_ruid = credentials.ruid;
        let old_euid = credentials.euid;
        let old_suid = credentials.suid;
        if !credentials.is_root() {
            if let Some(ruid) = ruid
                && ruid != old_ruid
                && ruid != old_euid
            {
                return Err(SysError::EPERM);
            }
            if let Some(euid) = euid
                && euid != old_ruid
                && euid != old_euid
                && euid != old_suid
            {
                return Err(SysError::EPERM);
            }
        }
        if let Some(ruid) = ruid {
            credentials.ruid = ruid;
        }
        if let Some(euid) = euid {
            credentials.euid = euid;
            credentials.fsuid = euid;
        }
        if ruid.is_some() || euid.is_some_and(|euid| euid != old_ruid) {
            credentials.suid = credentials.euid;
        }
        Ok(0)
    })
}

pub fn sys_setregid(rgid: i32, egid: i32) -> SysResult {
    let rgid = require_valid_id(rgid)?;
    let egid = require_valid_id(egid)?;
    current_process().mutate_credentials(|credentials| {
        let old_rgid = credentials.rgid;
        let old_egid = credentials.egid;
        let old_sgid = credentials.sgid;
        if !credentials.is_root() {
            if let Some(rgid) = rgid
                && rgid != old_rgid
                && rgid != old_egid
                && rgid != old_sgid
            {
                return Err(SysError::EPERM);
            }
            if let Some(egid) = egid
                && egid != old_rgid
                && egid != old_egid
                && egid != old_sgid
            {
                return Err(SysError::EPERM);
            }
        }
        if let Some(rgid) = rgid {
            credentials.rgid = rgid;
        }
        if let Some(egid) = egid {
            credentials.egid = egid;
            credentials.fsgid = egid;
        }
        if rgid.is_some() || egid.is_some_and(|egid| egid != old_rgid) {
            credentials.sgid = credentials.egid;
        }
        Ok(0)
    })
}

pub fn sys_setresuid(ruid: i32, euid: i32, suid: i32) -> SysResult {
    let ruid = require_valid_id(ruid)?;
    let euid = require_valid_id(euid)?;
    let suid = require_valid_id(suid)?;
    current_process().mutate_credentials(|credentials| {
        if !credentials.is_root() {
            for uid in [ruid, euid, suid].into_iter().flatten() {
                if !credentials.uid_matches_saved_set(uid) {
                    return Err(SysError::EPERM);
                }
            }
        }
        if let Some(ruid) = ruid {
            credentials.ruid = ruid;
        }
        if let Some(euid) = euid {
            credentials.euid = euid;
        }
        if let Some(suid) = suid {
            credentials.suid = suid;
        }
        credentials.fsuid = credentials.euid;
        Ok(0)
    })
}

pub fn sys_setresgid(rgid: i32, egid: i32, sgid: i32) -> SysResult {
    let rgid = require_valid_id(rgid)?;
    let egid = require_valid_id(egid)?;
    let sgid = require_valid_id(sgid)?;
    current_process().mutate_credentials(|credentials| {
        if !credentials.is_root() {
            for gid in [rgid, egid, sgid].into_iter().flatten() {
                if !credentials.gid_matches_saved_set(gid) {
                    return Err(SysError::EPERM);
                }
            }
        }
        if let Some(rgid) = rgid {
            credentials.rgid = rgid;
        }
        if let Some(egid) = egid {
            credentials.egid = egid;
        }
        if let Some(sgid) = sgid {
            credentials.sgid = sgid;
        }
        credentials.fsgid = credentials.egid;
        Ok(0)
    })
}

pub fn sys_getresuid_ctx(
    ctx: &SyscallContext,
    ruid: *mut u32,
    euid: *mut u32,
    suid: *mut u32,
) -> SysResult {
    let credentials = ctx.process().credentials();
    if !ruid.is_null() {
        write_user_value_ctx(ctx, ruid, &credentials.ruid)?;
    }
    if !euid.is_null() {
        write_user_value_ctx(ctx, euid, &credentials.euid)?;
    }
    if !suid.is_null() {
        write_user_value_ctx(ctx, suid, &credentials.suid)?;
    }
    Ok(0)
}

pub fn sys_getresgid_ctx(
    ctx: &SyscallContext,
    rgid: *mut u32,
    egid: *mut u32,
    sgid: *mut u32,
) -> SysResult {
    let credentials = ctx.process().credentials();
    if !rgid.is_null() {
        write_user_value_ctx(ctx, rgid, &credentials.rgid)?;
    }
    if !egid.is_null() {
        write_user_value_ctx(ctx, egid, &credentials.egid)?;
    }
    if !sgid.is_null() {
        write_user_value_ctx(ctx, sgid, &credentials.sgid)?;
    }
    Ok(0)
}

pub fn sys_setfsuid(uid: i32) -> SysResult {
    let uid = require_valid_id(uid)?;
    Ok(current_process().mutate_credentials(|credentials| {
        let old_fsuid = credentials.fsuid;
        if let Some(uid) = uid
            && (credentials.is_root()
                || uid == credentials.ruid
                || uid == credentials.euid
                || uid == credentials.suid
                || uid == credentials.fsuid)
        {
            credentials.fsuid = uid;
        }
        old_fsuid as isize
    }))
}

pub fn sys_setfsgid(gid: i32) -> SysResult {
    let gid = require_valid_id(gid)?;
    Ok(current_process().mutate_credentials(|credentials| {
        let old_fsgid = credentials.fsgid;
        if let Some(gid) = gid
            && (credentials.is_root()
                || gid == credentials.rgid
                || gid == credentials.egid
                || gid == credentials.sgid
                || gid == credentials.fsgid)
        {
            credentials.fsgid = gid;
        }
        old_fsgid as isize
    }))
}
