use super::errno::{SysError, SysResult};
use super::fs::get_file_by_fd;
use super::user_ptr::{read_user_array_item, read_user_value, write_user_value};
use crate::sync::UPIntrFreeCell;
use crate::task::current_user_token;
use alloc::collections::{BTreeMap, VecDeque};
use lazy_static::lazy_static;

const AIO_MAX_NR: usize = 65_536;
const IOCB_CMD_PREAD: u16 = 0;
const IOCB_CMD_PWRITE: u16 = 1;

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct LinuxIocb {
    aio_data: u64,
    aio_key: u32,
    aio_rw_flags: u32,
    aio_lio_opcode: u16,
    aio_reqprio: i16,
    aio_fildes: u32,
    aio_buf: u64,
    aio_nbytes: u64,
    aio_offset: i64,
    aio_reserved2: u64,
    aio_flags: u32,
    aio_resfd: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct LinuxIoEvent {
    data: u64,
    obj: u64,
    res: i64,
    res2: i64,
}

#[derive(Default)]
struct AioContext {
    max_events: usize,
    pending: VecDeque<LinuxIoEvent>,
}

struct AioManager {
    next_id: usize,
    contexts: BTreeMap<usize, AioContext>,
}

impl AioManager {
    fn new() -> Self {
        Self {
            next_id: 1,
            contexts: BTreeMap::new(),
        }
    }

    fn create(&mut self, max_events: usize) -> usize {
        while self.next_id == 0 || self.contexts.contains_key(&self.next_id) {
            self.next_id = self.next_id.wrapping_add(1);
        }
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1);
        self.contexts.insert(
            id,
            AioContext {
                max_events,
                pending: VecDeque::new(),
            },
        );
        id
    }

    fn remove(&mut self, ctx: usize) -> SysResult<()> {
        self.contexts
            .remove(&ctx)
            .map(|_| ())
            .ok_or(SysError::EINVAL)
    }

    fn contains(&self, ctx: usize) -> bool {
        self.contexts.contains_key(&ctx)
    }

    fn push_event(&mut self, ctx: usize, event: LinuxIoEvent) -> SysResult<()> {
        let context = self.contexts.get_mut(&ctx).ok_or(SysError::EINVAL)?;
        if context.pending.len() >= context.max_events {
            return Err(SysError::EAGAIN);
        }
        context.pending.push_back(event);
        Ok(())
    }

    fn pop_events(&mut self, ctx: usize, max: usize) -> SysResult<alloc::vec::Vec<LinuxIoEvent>> {
        let context = self.contexts.get_mut(&ctx).ok_or(SysError::EINVAL)?;
        let count = max.min(context.pending.len());
        let mut events = alloc::vec::Vec::with_capacity(count);
        for _ in 0..count {
            if let Some(event) = context.pending.pop_front() {
                events.push(event);
            }
        }
        Ok(events)
    }
}

lazy_static! {
    static ref AIO_MANAGER: UPIntrFreeCell<AioManager> =
        unsafe { UPIntrFreeCell::new(AioManager::new()) };
}

pub(crate) fn aio_max_nr_content() -> &'static str {
    "65536\n"
}

pub fn sys_io_setup(nr_events: usize, ctxp: *mut usize) -> SysResult {
    if ctxp.is_null() {
        return Err(SysError::EFAULT);
    }
    if nr_events == 0 || nr_events == usize::MAX {
        return Err(SysError::EINVAL);
    }
    if nr_events > AIO_MAX_NR {
        return Err(SysError::EAGAIN);
    }

    let token = current_user_token();
    if read_user_value::<usize>(token, ctxp.cast_const())? != 0 {
        return Err(SysError::EINVAL);
    }

    let ctx = AIO_MANAGER.exclusive_access().create(nr_events);
    write_user_value(token, ctxp, &ctx)?;
    Ok(0)
}

pub fn sys_io_destroy(ctx: usize) -> SysResult {
    AIO_MANAGER.exclusive_access().remove(ctx)?;
    Ok(0)
}

pub fn sys_io_cancel(ctx: usize, iocb: *const LinuxIocb, result: *mut LinuxIoEvent) -> SysResult {
    if iocb.is_null() || result.is_null() {
        return Err(SysError::EFAULT);
    }
    let token = current_user_token();
    let _ = read_user_value::<LinuxIocb>(token, iocb)?;
    let _ = read_user_value::<LinuxIoEvent>(token, result.cast_const())?;
    if !AIO_MANAGER.exclusive_access().contains(ctx) {
        return Err(SysError::EINVAL);
    }
    Err(SysError::EINVAL)
}

pub fn sys_io_getevents(
    ctx: usize,
    min_nr: isize,
    nr: isize,
    events: *mut LinuxIoEvent,
    timeout: *const u8,
) -> SysResult {
    if !AIO_MANAGER.exclusive_access().contains(ctx) {
        return Err(SysError::EINVAL);
    }
    if min_nr < 0 || nr < 0 || min_nr > nr {
        return Err(SysError::EINVAL);
    }
    if !timeout.is_null() {
        let _ = read_user_value::<u8>(current_user_token(), timeout)?;
    }
    if nr == 0 {
        return Ok(0);
    }
    if events.is_null() {
        return Err(SysError::EFAULT);
    }

    let token = current_user_token();
    let ready = AIO_MANAGER
        .exclusive_access()
        .pop_events(ctx, nr as usize)?;
    for (index, event) in ready.iter().enumerate() {
        write_user_value(token, events.wrapping_add(index), event)?;
    }
    Ok(ready.len() as isize)
}

pub fn sys_io_pgetevents(
    ctx: usize,
    min_nr: isize,
    nr: isize,
    events: *mut LinuxIoEvent,
    timeout: *const u8,
    sigmask: *const u8,
) -> SysResult {
    if !sigmask.is_null() {
        let _ = read_user_value::<u8>(current_user_token(), sigmask)?;
    }
    sys_io_getevents(ctx, min_nr, nr, events, timeout)
}

pub fn sys_io_submit(ctx: usize, nr: isize, iocbpp: *const *const LinuxIocb) -> SysResult {
    if !AIO_MANAGER.exclusive_access().contains(ctx) {
        return Err(SysError::EINVAL);
    }
    if nr < 0 {
        return Err(SysError::EINVAL);
    }
    if nr == 0 {
        return Ok(0);
    }

    let token = current_user_token();
    for index in 0..nr as usize {
        let iocb_ptr = read_user_array_item(token, iocbpp, index)?;
        if iocb_ptr.is_null() {
            return Err(SysError::EFAULT);
        }
        let iocb = read_user_value::<LinuxIocb>(token, iocb_ptr)?;
        validate_iocb(&iocb)?;
    }

    for index in 0..nr as usize {
        let iocb_ptr = read_user_array_item(token, iocbpp, index)?;
        let iocb = read_user_value::<LinuxIocb>(token, iocb_ptr)?;
        let event = LinuxIoEvent {
            data: iocb.aio_data,
            obj: iocb_ptr as u64,
            res: iocb.aio_nbytes as i64,
            res2: 0,
        };
        AIO_MANAGER.exclusive_access().push_event(ctx, event)?;
    }

    Ok(nr)
}

fn validate_iocb(iocb: &LinuxIocb) -> SysResult<()> {
    let fd = iocb.aio_fildes as i32;
    if fd < 0 {
        return Err(SysError::EBADF);
    }
    let file = get_file_by_fd(fd as usize).map_err(|_| SysError::EBADF)?;
    match iocb.aio_lio_opcode {
        IOCB_CMD_PREAD => {
            if !file.readable() {
                return Err(SysError::EBADF);
            }
        }
        IOCB_CMD_PWRITE => {
            if !file.writable() {
                return Err(SysError::EBADF);
            }
        }
        _ => return Err(SysError::EINVAL),
    }
    Ok(())
}
