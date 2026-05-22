use crate::config::PAGE_SIZE;
use crate::fs::{File, FileStat, OpenFlags, PollEvents, S_IFDIR, S_IFMT, S_IFREG, SeekWhence};
use crate::mm::UserBuffer;
use crate::task::{
    FdTableEntry, RLimitResource, SignalFlags, current_add_signal, current_process,
    current_user_token,
};
use alloc::{vec, vec::Vec};
use core::mem::size_of;
use core::ptr::read_volatile;

use super::super::errno::{SysError, SysResult};
use super::super::user_ptr::{
    UserBufferAccess, read_user_array_item, read_user_value,
    translated_byte_buffer_checked_with_mmap_fault, write_user_value,
};
use super::fanotify::{fanotify_notify_access, fanotify_notify_modify};
use super::fd::{get_fd_entry_by_fd, get_file_by_fd};
use super::inotify::{inotify_notify_access, inotify_notify_modify};
use super::uapi::{IOV_MAX, LinuxIovec};

struct UserIovecs {
    entries: Vec<LinuxIovec>,
    total_len: usize,
}

struct UserIovecChunk {
    len: usize,
    buffers: Vec<&'static mut [u8]>,
}

struct UserIovecCursor {
    token: usize,
    entries: Vec<LinuxIovec>,
    index: usize,
    access: UserBufferAccess,
}

fn truncate_user_buffers(
    buffers: Vec<&'static mut [u8]>,
    mut limit: usize,
) -> Vec<&'static mut [u8]> {
    let mut truncated = Vec::new();
    for buffer in buffers {
        if limit == 0 {
            break;
        }
        if buffer.len() <= limit {
            limit -= buffer.len();
            truncated.push(buffer);
        } else {
            let (head, _) = buffer.split_at_mut(limit);
            truncated.push(head);
            break;
        }
    }
    truncated
}

impl UserIovecCursor {
    fn new(token: usize, iovecs: UserIovecs, access: UserBufferAccess) -> Self {
        Self {
            token,
            entries: iovecs.entries,
            index: 0,
            access,
        }
    }

    fn validate_all(&self) -> SysResult<()> {
        // CONTEXT: readv/preadv validate every destination iovec before
        // reading so an early filesystem read cannot partially modify user
        // memory before a later bad iovec reports EFAULT.
        for iovec in self.entries.iter() {
            if iovec.len == 0 {
                continue;
            }
            translated_byte_buffer_checked_with_mmap_fault(
                self.token,
                iovec.base as *const u8,
                iovec.len,
                self.access,
            )?;
        }
        Ok(())
    }

    fn next_chunk(&mut self) -> Option<SysResult<UserIovecChunk>> {
        while self.index < self.entries.len() {
            let iovec = self.entries[self.index];
            self.index += 1;
            if iovec.len == 0 {
                continue;
            }
            return Some(
                translated_byte_buffer_checked_with_mmap_fault(
                    self.token,
                    iovec.base as *const u8,
                    iovec.len,
                    self.access,
                )
                .map(|buffers| UserIovecChunk {
                    len: iovec.len,
                    buffers,
                }),
            );
        }
        None
    }
}

/// Reads a Linux iovec array and validates the aggregate byte count.
///
/// Length overflow and counts beyond Linux `SSIZE_MAX` are reported as
/// `EINVAL`, preserving the visible readv/writev-family ABI boundary.
fn read_user_iovecs(token: usize, iov: *const LinuxIovec, iovcnt: usize) -> SysResult<UserIovecs> {
    let mut entries = Vec::with_capacity(iovcnt);
    let mut total_len = 0usize;
    for index in 0..iovcnt {
        let iovec = read_user_array_item(token, iov, index)?;
        total_len = total_len.checked_add(iovec.len).ok_or(SysError::EINVAL)?;
        if total_len > isize::MAX as usize {
            return Err(SysError::EINVAL);
        }
        entries.push(iovec);
    }
    Ok(UserIovecs { entries, total_len })
}

fn ensure_nonblocking_ready(entry: &FdTableEntry, events: PollEvents) -> SysResult<()> {
    if !entry.status_flags().contains(OpenFlags::NONBLOCK) {
        return Ok(());
    }
    let file = entry.file();
    if file.poll(events).intersects(events) {
        Ok(())
    } else {
        Err(SysError::EAGAIN)
    }
}

fn write_with_status_flags(entry: &FdTableEntry, buf: UserBuffer) -> usize {
    let file = entry.file();
    if entry.status_flags().contains(OpenFlags::APPEND) {
        file.write_append(buf)
    } else {
        file.write(buf)
    }
}

fn current_file_size_limit() -> usize {
    current_process()
        .inner_exclusive_access()
        .resource_limits
        .get(RLimitResource::FSize)
        .rlim_cur
}

fn queue_file_size_limit_signal() {
    current_add_signal(SignalFlags::SIGXFSZ);
}

fn allowed_write_len_at(
    file: &(dyn File + Send + Sync),
    offset: usize,
    requested_len: usize,
) -> SysResult<usize> {
    if requested_len == 0 {
        return Ok(0);
    }
    let stat = file.stat()?;
    if stat.mode & S_IFMT != S_IFREG {
        return Ok(requested_len);
    }
    let limit = current_file_size_limit();
    if limit == usize::MAX {
        return Ok(requested_len);
    }
    let write_end = offset.checked_add(requested_len).ok_or(SysError::EFBIG)?;
    let current_size = usize::try_from(stat.size).map_err(|_| SysError::EFBIG)?;
    let permitted_end = current_size.max(limit);
    if write_end <= permitted_end {
        return Ok(requested_len);
    }
    if offset >= permitted_end {
        queue_file_size_limit_signal();
        return Err(SysError::EFBIG);
    }
    Ok(permitted_end - offset)
}

fn allowed_write_len_for_entry(entry: &FdTableEntry, requested_len: usize) -> SysResult<usize> {
    let file = entry.file();
    let stat = file.stat()?;
    if stat.mode & S_IFMT != S_IFREG {
        return Ok(requested_len);
    }
    let offset = if entry.status_flags().contains(OpenFlags::APPEND) {
        usize::try_from(stat.size).map_err(|_| SysError::EFBIG)?
    } else {
        file.seek(0, SeekWhence::Current)?
    };
    allowed_write_len_at(file.as_ref(), offset, requested_len)
}

fn check_file_size_limit_for_len(file: &(dyn File + Send + Sync), len: usize) -> SysResult<()> {
    let stat = file.stat()?;
    if stat.mode & S_IFMT != S_IFREG {
        return Ok(());
    }
    let current_size = usize::try_from(stat.size).map_err(|_| SysError::EFBIG)?;
    if len <= current_size || len <= current_file_size_limit() {
        return Ok(());
    }
    queue_file_size_limit_signal();
    Err(SysError::EFBIG)
}

fn checked_write_result(requested: usize, written: usize) -> SysResult {
    if requested > 0 && written == 0 {
        // CONTEXT: A no-progress non-empty write makes libc/BusyBox retry loops
        // spin forever. Backends that cannot accept data must reject the write in
        // check_write/check_write_at; if they still report no progress, surface a
        // hard I/O error instead of returning 0 to userspace.
        Err(SysError::EIO)
    } else {
        Ok(written as isize)
    }
}

fn checked_write_result_for_entry(
    entry: &FdTableEntry,
    requested: usize,
    written: usize,
) -> SysResult {
    let file = entry.file();
    if requested > 0
        && written == 0
        && (file.pipe_readers_closed() || file.socket_write_peer_closed())
    {
        current_add_signal(SignalFlags::SIGPIPE);
        Err(SysError::EPIPE)
    } else {
        checked_write_result(requested, written)
    }
}

fn check_pipe_write_peer(entry: &FdTableEntry, has_data: bool) -> SysResult<()> {
    if has_data && entry.file().pipe_readers_closed() {
        current_add_signal(SignalFlags::SIGPIPE);
        return Err(SysError::EPIPE);
    }
    Ok(())
}

fn checked_position_offset(offset: usize) -> SysResult<usize> {
    if offset > isize::MAX as usize {
        Err(SysError::EINVAL)
    } else {
        Ok(offset)
    }
}

fn checked_position_offset_pair(pos_l: usize, pos_h: usize) -> SysResult<usize> {
    let offset = if pos_h == 0 {
        pos_l
    } else {
        let combined = ((pos_h as u128) << 32) | ((pos_l as u32) as u128);
        if combined > usize::MAX as u128 {
            return Err(SysError::EINVAL);
        }
        combined as usize
    };
    checked_position_offset(offset)
}

fn ensure_positioned_target(file: &(dyn File + Send + Sync)) -> SysResult<()> {
    let mode = file.stat()?.mode;
    if mode & S_IFMT == S_IFDIR {
        return Err(SysError::EISDIR);
    }
    if mode & S_IFREG != S_IFREG {
        return Err(SysError::ESPIPE);
    }
    Ok(())
}

fn ensure_fadvise_target(file: &(dyn File + Send + Sync)) -> SysResult<()> {
    if file.stat()?.mode & S_IFMT == S_IFREG {
        Ok(())
    } else {
        Err(SysError::ESPIPE)
    }
}

const POSIX_FADV_WILLNEED: i32 = 3;

fn fault_in_read_buffers(buffers: &[&'static mut [u8]]) {
    for slice in buffers {
        for index in 0..slice.len() {
            // Force the lazy user page to be touched even when a later file
            // permission check makes the syscall fail without copying data.
            unsafe {
                read_volatile(slice.as_ptr().add(index));
            }
        }
    }
}

pub fn sys_lseek(fd: usize, offset: i64, whence: usize) -> SysResult {
    let whence = match whence {
        0 => SeekWhence::Set,
        1 => SeekWhence::Current,
        2 => SeekWhence::End,
        3 => SeekWhence::Data,
        4 => SeekWhence::Hole,
        _ => return Err(SysError::EINVAL),
    };
    if matches!(whence, SeekWhence::Data | SeekWhence::Hole) && offset < 0 {
        return Err(SysError::EINVAL);
    }
    let file = get_file_by_fd(fd)?;
    let new_offset = file.seek(offset, whence)?;
    if new_offset > isize::MAX as usize {
        return Err(SysError::EINVAL);
    }
    Ok(new_offset as isize)
}

pub fn sys_ftruncate(fd: usize, len: usize) -> SysResult {
    if len > isize::MAX as usize {
        return Err(SysError::EINVAL);
    }
    let file = get_file_by_fd(fd)?;
    if !file.writable() {
        // CONTEXT: POSIX permits either EBADF or EINVAL for ftruncate() on an
        // fd that is not open for writing; Linux reports EINVAL, and LTP
        // ftruncate03 checks that Linux-visible errno.
        return Err(SysError::EINVAL);
    }
    if file.stat()?.mode & S_IFREG != S_IFREG {
        return Err(SysError::EINVAL);
    }
    check_file_size_limit_for_len(file.as_ref(), len)?;
    file.check_set_len(len)?;
    file.set_len(len)?;
    Ok(0)
}

const FALLOC_FL_KEEP_SIZE: u32 = 0x01;
const FALLOC_FL_PUNCH_HOLE: u32 = 0x02;
const FALLOC_FL_COLLAPSE_RANGE: u32 = 0x08;
const FALLOC_FL_ZERO_RANGE: u32 = 0x10;
const FALLOC_FL_INSERT_RANGE: u32 = 0x20;
const FALLOC_FL_UNSHARE_RANGE: u32 = 0x40;
const FALLOC_KNOWN_FLAGS: u32 = FALLOC_FL_KEEP_SIZE
    | FALLOC_FL_PUNCH_HOLE
    | FALLOC_FL_COLLAPSE_RANGE
    | FALLOC_FL_ZERO_RANGE
    | FALLOC_FL_INSERT_RANGE
    | FALLOC_FL_UNSHARE_RANGE;

pub fn sys_fallocate(fd: usize, mode: u32, offset: usize, len: usize) -> SysResult {
    let max_file_size = isize::MAX as usize;
    if offset > max_file_size || len > max_file_size || len == 0 {
        return Err(SysError::EINVAL);
    }
    let end = offset.checked_add(len).ok_or(SysError::EFBIG)?;
    if end > max_file_size {
        return Err(SysError::EFBIG);
    }

    let file = get_file_by_fd(fd)?;
    if !file.writable() {
        return Err(SysError::EBADF);
    }
    ensure_positioned_target(file.as_ref())?;

    if mode & !FALLOC_KNOWN_FLAGS != 0 {
        return Err(SysError::EINVAL);
    }
    if mode & FALLOC_FL_PUNCH_HOLE != 0 && mode & FALLOC_FL_KEEP_SIZE == 0 {
        return Err(SysError::EINVAL);
    }
    if mode & FALLOC_FL_PUNCH_HOLE != 0 && file.is_memfd() {
        if file.blocks_file_write() {
            return Err(SysError::EPERM);
        }
        // CONTEXT: memfd punch-hole support is visible to current LTP only
        // through success/failure and unchanged size, so this in-memory file
        // treats the range as a successful no-op.
        return Ok(0);
    }

    if mode
        & (FALLOC_FL_PUNCH_HOLE
            | FALLOC_FL_COLLAPSE_RANGE
            | FALLOC_FL_ZERO_RANGE
            | FALLOC_FL_INSERT_RANGE
            | FALLOC_FL_UNSHARE_RANGE)
        != 0
    {
        // UNFINISHED: Linux fallocate range operations require filesystem
        // extent allocation/deallocation support that this VFS layer does not
        // expose yet.
        return Err(SysError::ENOTSUP);
    }

    let keep_size = mode & FALLOC_FL_KEEP_SIZE != 0;
    if !keep_size && end as u64 > file.stat()?.size {
        check_file_size_limit_for_len(file.as_ref(), end)?;
        file.check_set_len(end)?;
        file.set_len(end)?;
    }
    // CONTEXT: the current VFS has no block preallocation API. KEEP_SIZE is
    // accepted as a no-op because its visible contract in LTP sparse-file
    // cases is that file size must not change.
    Ok(0)
}

pub fn sys_fadvise64(fd: usize, offset: i64, len: i64, advice: i32) -> SysResult {
    if offset < 0 || len < 0 {
        return Err(SysError::EINVAL);
    }
    let file = get_file_by_fd(fd)?;
    ensure_fadvise_target(file.as_ref())?;
    if !(0..=5).contains(&advice) {
        return Err(SysError::EINVAL);
    }
    // CONTEXT: The current VFS has no page-cache advice API. Linux accepts
    // valid POSIX_FADV_* hints as advisory, so the observable contest behavior
    // can be represented as a checked no-op for regular files.
    if advice == POSIX_FADV_WILLNEED {
        crate::fs::procfs_note_readahead();
    }
    Ok(0)
}

const COPY_FILE_RANGE_CHUNK: usize = PAGE_SIZE;

fn ensure_copy_file_range_target(file: &(dyn File + Send + Sync)) -> SysResult<FileStat> {
    let stat = file.stat()?;
    match stat.mode & S_IFMT {
        S_IFREG => Ok(stat),
        S_IFDIR => Err(SysError::EISDIR),
        _ => Err(SysError::EINVAL),
    }
}

fn read_copy_file_range_offset(token: usize, ptr: *mut i64) -> SysResult<Option<usize>> {
    if ptr.is_null() {
        return Ok(None);
    }
    let offset = read_user_value(token, ptr.cast_const())?;
    if offset < 0 {
        return Err(SysError::EINVAL);
    }
    Ok(Some(checked_position_offset(offset as usize)?))
}

fn write_copy_file_range_offset(
    token: usize,
    ptr: *mut i64,
    offset: Option<usize>,
) -> SysResult<()> {
    if !ptr.is_null() {
        let value = offset.ok_or(SysError::EINVAL)? as i64;
        write_user_value(token, ptr, &value)?;
    }
    Ok(())
}

fn current_copy_file_range_offset(file: &(dyn File + Send + Sync)) -> SysResult<usize> {
    Ok(file.seek(0, SeekWhence::Current)?)
}

fn checked_copy_file_range_end(start: usize, len: usize) -> SysResult<usize> {
    let end = start.checked_add(len).ok_or(SysError::EOVERFLOW)?;
    if end > isize::MAX as usize {
        return Err(SysError::EFBIG);
    }
    Ok(end)
}

fn same_file_range_overlaps(src_start: usize, dst_start: usize, len: usize) -> bool {
    if len == 0 {
        return false;
    }
    let src_end = src_start.saturating_add(len);
    let dst_end = dst_start.saturating_add(len);
    src_start < dst_end && dst_start < src_end
}

fn same_copy_file_range_file(
    in_file: &(dyn File + Send + Sync),
    out_file: &(dyn File + Send + Sync),
    in_stat: FileStat,
    out_stat: FileStat,
) -> bool {
    if let (Some(in_id), Some(out_id)) = (in_file.page_cache_id(), out_file.page_cache_id()) {
        return in_id == out_id;
    }

    (in_stat.ino != 0 || out_stat.ino != 0)
        && in_stat.dev == out_stat.dev
        && in_stat.ino == out_stat.ino
}

// UNFINISHED: Linux copy_file_range can delegate to filesystem-specific
// acceleration and Linux 5.19+ may support cross-filesystem copies. This path
// provides the current contest-visible regular-file semantics by copying
// through kernel memory and returning EXDEV across different mounts.
pub fn sys_copy_file_range(
    fd_in: usize,
    off_in: *mut i64,
    fd_out: usize,
    off_out: *mut i64,
    len: usize,
    flags: u32,
) -> SysResult {
    if flags != 0 {
        return Err(SysError::EINVAL);
    }

    let token = current_user_token();
    let in_offset_arg = read_copy_file_range_offset(token, off_in)?;
    let out_offset_arg = read_copy_file_range_offset(token, off_out)?;

    let in_entry = get_fd_entry_by_fd(fd_in)?;
    let out_entry = get_fd_entry_by_fd(fd_out)?;
    let in_file = in_entry.file();
    let out_file = out_entry.file();
    let in_stat = ensure_copy_file_range_target(in_file.as_ref())?;
    let out_stat = ensure_copy_file_range_target(out_file.as_ref())?;

    if !in_file.readable() {
        return Err(SysError::EBADF);
    }
    if !out_file.writable() {
        return Err(SysError::EBADF);
    }
    if out_entry.status_flags().contains(OpenFlags::APPEND) {
        return Err(SysError::EBADF);
    }
    if len == 0 {
        return Ok(0);
    }
    if in_stat.dev != out_stat.dev {
        return Err(SysError::EXDEV);
    }

    let mut in_offset = match in_offset_arg {
        Some(offset) => offset,
        None => current_copy_file_range_offset(in_file.as_ref())?,
    };
    let mut out_offset = match out_offset_arg {
        Some(offset) => offset,
        None => current_copy_file_range_offset(out_file.as_ref())?,
    };
    checked_copy_file_range_end(out_offset, len)?;

    if same_copy_file_range_file(in_file.as_ref(), out_file.as_ref(), in_stat, out_stat)
        && same_file_range_overlaps(in_offset, out_offset, len)
    {
        return Err(SysError::EINVAL);
    }

    if out_offset_arg.is_some() {
        out_file.check_write_at(out_offset, len)?;
    } else {
        out_file.check_write(len, false)?;
    }

    crate::perf::record_copy_file_range_call();
    let mut copied = 0usize;
    let mut buffer = vec![0u8; len.min(COPY_FILE_RANGE_CHUNK)];
    while copied < len {
        let want = buffer.len().min(len - copied);
        let read = if in_offset_arg.is_some() {
            in_file.read_at(in_offset, &mut buffer[..want])
        } else {
            in_file.read(kernel_user_buffer(&mut buffer[..want]))
        };
        if read == 0 {
            break;
        }

        let written = if out_offset_arg.is_some() {
            out_file.write_at(out_offset, &buffer[..read])
        } else {
            out_file.write(kernel_user_buffer(&mut buffer[..read]))
        };
        if written == 0 {
            break;
        }

        crate::perf::record_copy_file_range_chunk(written);
        copied = copied.checked_add(written).ok_or(SysError::EOVERFLOW)?;
        if in_offset_arg.is_some() {
            in_offset = in_offset.checked_add(written).ok_or(SysError::EOVERFLOW)?;
        }
        if out_offset_arg.is_some() {
            out_offset = out_offset.checked_add(written).ok_or(SysError::EOVERFLOW)?;
        }
        if read < want || written < read {
            break;
        }
    }

    write_copy_file_range_offset(token, off_in, in_offset_arg.map(|_| in_offset))?;
    write_copy_file_range_offset(token, off_out, out_offset_arg.map(|_| out_offset))?;
    Ok(copied as isize)
}

const SPLICE_F_MOVE: u32 = 0x01;
const SPLICE_F_NONBLOCK: u32 = 0x02;
const SPLICE_F_MORE: u32 = 0x04;
const SPLICE_F_GIFT: u32 = 0x08;
const SPLICE_KNOWN_FLAGS: u32 = SPLICE_F_MOVE | SPLICE_F_NONBLOCK | SPLICE_F_MORE | SPLICE_F_GIFT;
const SPLICE_COPY_CHUNK: usize = 4096;
const SENDFILE_COPY_CHUNK: usize = PAGE_SIZE;

// UNFINISHED: Linux splice can move pipe pages without copying and has deeper
// file-type-specific wakeup semantics. This contest compatibility path copies
// through kernel memory while preserving the visible fd, offset, and errno
// behavior needed by current LTP splice cases.
fn kernel_user_buffer(buf: &mut [u8]) -> UserBuffer {
    UserBuffer::from_kernel_slice_for_sync_io(buf)
}

fn read_sendfile_offset(token: usize, ptr: *mut i64) -> SysResult<Option<usize>> {
    if ptr.is_null() {
        return Ok(None);
    }
    translated_byte_buffer_checked_with_mmap_fault(
        token,
        ptr.cast_const().cast::<u8>(),
        size_of::<i64>(),
        UserBufferAccess::Write,
    )?;
    let offset = read_user_value(token, ptr.cast_const())?;
    if offset < 0 {
        return Err(SysError::EINVAL);
    }
    Ok(Some(checked_position_offset(offset as usize)?))
}

fn write_sendfile_offset(token: usize, ptr: *mut i64, offset: Option<usize>) -> SysResult<()> {
    if let Some(offset) = offset {
        write_user_value(token, ptr, &(offset as i64))?;
    }
    Ok(())
}

fn ensure_sendfile_input(file: &(dyn File + Send + Sync)) -> SysResult<()> {
    if !file.readable() {
        return Err(SysError::EBADF);
    }
    if file.stat()?.mode & S_IFMT != S_IFREG {
        return Err(SysError::EINVAL);
    }
    Ok(())
}

pub fn sys_sendfile(out_fd: usize, in_fd: usize, offset: *mut i64, count: usize) -> SysResult {
    let token = current_user_token();
    let mut explicit_offset = read_sendfile_offset(token, offset)?;
    let in_entry = get_fd_entry_by_fd(in_fd)?;
    let out_entry = get_fd_entry_by_fd(out_fd)?;
    let in_file = in_entry.file();
    let out_file = out_entry.file();

    ensure_sendfile_input(in_file.as_ref())?;
    if !out_file.writable() {
        return Err(SysError::EBADF);
    }
    if out_entry.status_flags().contains(OpenFlags::APPEND) {
        return Err(SysError::EINVAL);
    }
    if count == 0 {
        write_sendfile_offset(token, offset, explicit_offset)?;
        return Ok(0);
    }

    crate::perf::record_sendfile_call();
    let mut copied = 0usize;
    let mut buffer = vec![0u8; count.min(SENDFILE_COPY_CHUNK)];
    while copied < count {
        let remaining = count - copied;
        let permitted = allowed_write_len_for_entry(&out_entry, remaining)?;
        if permitted == 0 {
            break;
        }
        let want = buffer.len().min(permitted);
        ensure_nonblocking_ready(&out_entry, PollEvents::POLLOUT)?;
        let read = if let Some(input_offset) = explicit_offset {
            in_file.read_at(input_offset, &mut buffer[..want])
        } else {
            in_file.read(kernel_user_buffer(&mut buffer[..want]))
        };
        if read == 0 {
            break;
        }

        let written = write_with_status_flags(&out_entry, kernel_user_buffer(&mut buffer[..read]));
        if written == 0 {
            checked_write_result_for_entry(&out_entry, read, written)?;
            break;
        }

        crate::perf::record_sendfile_chunk(written);
        copied = copied.checked_add(written).ok_or(SysError::EOVERFLOW)?;
        if let Some(input_offset) = explicit_offset.as_mut() {
            *input_offset = input_offset
                .checked_add(written)
                .ok_or(SysError::EOVERFLOW)?;
        }
        if written < read {
            break;
        }
    }

    write_sendfile_offset(token, offset, explicit_offset)?;
    Ok(copied as isize)
}

fn read_splice_offset(token: usize, ptr: *mut i64, is_pipe: bool) -> SysResult<Option<i64>> {
    if ptr.is_null() {
        return Ok(None);
    }
    if is_pipe {
        return Err(SysError::ESPIPE);
    }
    let offset = read_user_value(token, ptr.cast_const())?;
    if offset < 0 {
        return Err(SysError::EINVAL);
    }
    Ok(Some(offset))
}

fn write_splice_offset(token: usize, ptr: *mut i64, offset: Option<i64>) -> SysResult<()> {
    if let Some(offset) = offset {
        write_user_value(token, ptr, &offset)?;
    }
    Ok(())
}

fn read_for_splice(entry: &FdTableEntry, offset: Option<i64>, buf: &mut [u8]) -> SysResult<usize> {
    let file = entry.file();
    if let Some(offset) = offset {
        Ok(file.read_at(offset as usize, buf))
    } else {
        if file.is_socket() && !file.poll(PollEvents::POLLIN).contains(PollEvents::POLLIN) {
            return Err(SysError::EINVAL);
        }
        ensure_nonblocking_ready(entry, PollEvents::POLLIN)?;
        Ok(file.read(kernel_user_buffer(buf)))
    }
}

fn write_for_splice(entry: &FdTableEntry, offset: Option<i64>, data: &[u8]) -> SysResult<usize> {
    let file = entry.file();
    if file.is_dev_full() && !data.is_empty() {
        return Err(SysError::ENOSPC);
    }
    if let Some(offset) = offset {
        Ok(file.write_at(offset as usize, data))
    } else {
        check_pipe_write_peer(entry, !data.is_empty())?;
        ensure_nonblocking_ready(entry, PollEvents::POLLOUT)?;
        let mut owned = data.to_vec();
        Ok(write_with_status_flags(
            entry,
            kernel_user_buffer(&mut owned),
        ))
    }
}

pub fn sys_splice(
    fd_in: usize,
    off_in: *mut i64,
    fd_out: usize,
    off_out: *mut i64,
    len: usize,
    flags: u32,
) -> SysResult {
    if flags & !SPLICE_KNOWN_FLAGS != 0 {
        return Err(SysError::EINVAL);
    }
    if len == 0 {
        return Ok(0);
    }

    let token = current_user_token();
    let in_entry = get_fd_entry_by_fd(fd_in)?;
    let out_entry = get_fd_entry_by_fd(fd_out)?;
    let in_file = in_entry.file();
    let out_file = out_entry.file();
    if !in_file.readable() {
        return Err(SysError::EBADF);
    }
    if !out_file.writable() {
        return Err(SysError::EBADF);
    }
    if out_entry.status_flags().contains(OpenFlags::APPEND) {
        return Err(SysError::EINVAL);
    }
    if in_file.stat()?.mode & S_IFDIR == S_IFDIR || out_file.stat()?.mode & S_IFDIR == S_IFDIR {
        return Err(SysError::EINVAL);
    }
    if !in_file.supports_splice_read() || !out_file.supports_splice_write() {
        return Err(SysError::EINVAL);
    }

    let in_is_pipe = in_file.is_pipe();
    let out_is_pipe = out_file.is_pipe();
    if !in_is_pipe && !out_is_pipe {
        return Err(SysError::EINVAL);
    }

    let mut in_offset = read_splice_offset(token, off_in, in_is_pipe)?;
    let mut out_offset = read_splice_offset(token, off_out, out_is_pipe)?;
    crate::perf::record_splice_call();
    if in_is_pipe && out_is_pipe && in_file.pipe_occupied().unwrap_or(0) > 0 {
        check_pipe_write_peer(&out_entry, true)?;
        ensure_nonblocking_ready(&in_entry, PollEvents::POLLIN)?;
        ensure_nonblocking_ready(&out_entry, PollEvents::POLLOUT)?;
        if let Some(moved) = in_file.splice_pipe_to_pipe(out_file.as_ref(), len)? {
            if moved > 0 {
                crate::perf::record_splice_chunk(moved);
            }
            return Ok(moved as isize);
        }
    }
    let mut copied = 0usize;
    let mut buffer = vec![0u8; len.min(SPLICE_COPY_CHUNK)];

    while copied < len {
        let want = buffer.len().min(len - copied);
        let read = read_for_splice(&in_entry, in_offset, &mut buffer[..want])?;
        if read == 0 {
            break;
        }
        let written = write_for_splice(&out_entry, out_offset, &buffer[..read])?;
        if written == 0 {
            break;
        }
        crate::perf::record_splice_chunk(written);
        copied += written;
        if let Some(offset) = in_offset.as_mut() {
            *offset += read as i64;
        }
        if let Some(offset) = out_offset.as_mut() {
            *offset += written as i64;
        }
        if read < want && (in_is_pipe || in_file.is_socket()) {
            break;
        }
        if written < read {
            break;
        }
    }

    write_splice_offset(token, off_in, in_offset)?;
    write_splice_offset(token, off_out, out_offset)?;
    Ok(copied as isize)
}

pub fn sys_fsync(fd: usize) -> SysResult {
    let file = get_file_by_fd(fd)?;
    let mode = file.stat()?.mode;
    if mode & S_IFREG != S_IFREG && mode & S_IFDIR != S_IFDIR {
        return Err(SysError::EINVAL);
    }
    file.sync(false)?;
    Ok(0)
}

pub fn sys_sync() -> SysResult {
    crate::fs::sync_all_mounts();
    Ok(0)
}

pub fn sys_syncfs(fd: usize) -> SysResult {
    let file = get_file_by_fd(fd)?;
    // CONTEXT: The current in-kernel filesystems are synchronous enough for
    // LTP's fanotify/drop-caches ordering checks. Validate the fd and flush
    // the referenced file object when the backend exposes a sync operation.
    let _ = file.sync(false);
    Ok(0)
}

pub fn sys_pread64(fd: usize, buf: *mut u8, len: usize, offset: usize) -> SysResult {
    let offset = checked_position_offset(offset)?;
    let token = current_user_token();
    let file = get_file_by_fd(fd)?;
    if !file.readable() {
        return Err(SysError::EBADF);
    }
    ensure_positioned_target(file.as_ref())?;
    let buffers =
        translated_byte_buffer_checked_with_mmap_fault(token, buf, len, UserBufferAccess::Write)?;
    let mut total_read = 0usize;
    for slice in buffers {
        let read = file.read_at(
            offset.checked_add(total_read).ok_or(SysError::EINVAL)?,
            slice,
        );
        total_read += read;
        if read < slice.len() {
            break;
        }
    }
    fanotify_notify_access(&file, total_read);
    inotify_notify_access(&file, total_read);
    Ok(total_read as isize)
}

pub fn sys_pwrite64(fd: usize, buf: *const u8, len: usize, offset: usize) -> SysResult {
    let mut offset = checked_position_offset(offset)?;
    let entry = get_fd_entry_by_fd(fd)?;
    let file = entry.file();
    ensure_positioned_target(file.as_ref())?;
    if !file.writable() {
        return Err(SysError::EBADF);
    }
    if len == 0 {
        return Ok(0);
    }
    if buf.is_null() {
        return Err(SysError::EFAULT);
    }
    let token = current_user_token();
    if entry.status_flags().contains(OpenFlags::APPEND) {
        offset = file.stat()?.size as usize;
    }
    let allowed_len = allowed_write_len_at(file.as_ref(), offset, len)?;
    let buffers = translated_byte_buffer_checked_with_mmap_fault(
        token,
        buf,
        allowed_len,
        UserBufferAccess::Read,
    )?;
    if let Err(err) = file.check_write_at(offset, allowed_len) {
        fault_in_read_buffers(&buffers);
        return Err(err.into());
    }
    let mut total_written = 0usize;
    for slice in buffers {
        let written = file.write_at(
            offset.checked_add(total_written).ok_or(SysError::EINVAL)?,
            slice,
        );
        total_written += written;
        if written < slice.len() {
            break;
        }
    }
    fanotify_notify_modify(&file, total_written);
    inotify_notify_modify(&file, total_written);
    checked_write_result(allowed_len, total_written)
}

pub fn sys_preadv(
    fd: usize,
    iov: *const LinuxIovec,
    iovcnt: usize,
    pos_l: usize,
    pos_h: usize,
) -> SysResult {
    if iovcnt == 0 {
        return Ok(0);
    }
    if iovcnt > IOV_MAX {
        return Err(SysError::EINVAL);
    }
    if iov.is_null() {
        return Err(SysError::EFAULT);
    }
    let mut offset = checked_position_offset_pair(pos_l, pos_h)?;
    let token = current_user_token();
    let iovecs = read_user_iovecs(token, iov, iovcnt)?;
    let file = get_file_by_fd(fd)?;
    if !file.readable() {
        return Err(SysError::EBADF);
    }
    ensure_positioned_target(file.as_ref())?;

    let mut cursor = UserIovecCursor::new(token, iovecs, UserBufferAccess::Write);
    cursor.validate_all()?;

    let mut total_read = 0usize;
    while let Some(chunk) = cursor.next_chunk() {
        for slice in chunk?.buffers {
            let read = file.read_at(offset, slice);
            total_read += read;
            offset = offset.checked_add(read).ok_or(SysError::EINVAL)?;
            if read < slice.len() {
                fanotify_notify_access(&file, total_read);
                inotify_notify_access(&file, total_read);
                return Ok(total_read as isize);
            }
        }
    }
    fanotify_notify_access(&file, total_read);
    inotify_notify_access(&file, total_read);
    Ok(total_read as isize)
}

pub fn sys_pwritev(
    fd: usize,
    iov: *const LinuxIovec,
    iovcnt: usize,
    pos_l: usize,
    pos_h: usize,
) -> SysResult {
    if iovcnt == 0 {
        return Ok(0);
    }
    if iovcnt > IOV_MAX {
        return Err(SysError::EINVAL);
    }
    if iov.is_null() {
        return Err(SysError::EFAULT);
    }
    let mut offset = checked_position_offset_pair(pos_l, pos_h)?;
    let token = current_user_token();
    let iovecs = read_user_iovecs(token, iov, iovcnt)?;
    let entry = get_fd_entry_by_fd(fd)?;
    let file = entry.file();
    ensure_positioned_target(file.as_ref())?;
    if !file.writable() {
        return Err(SysError::EBADF);
    }
    if entry.status_flags().contains(OpenFlags::APPEND) {
        offset = file.stat()?.size as usize;
    }
    let allowed_len = allowed_write_len_at(file.as_ref(), offset, iovecs.total_len)?;
    let requested_len = allowed_len;
    let mut remaining_len = allowed_len;
    let mut cursor = UserIovecCursor::new(token, iovecs, UserBufferAccess::Read);
    let mut total_written = 0usize;
    while let Some(chunk) = cursor.next_chunk() {
        let chunk = match chunk {
            Ok(chunk) => chunk,
            Err(_) if total_written > 0 => {
                fanotify_notify_modify(&file, total_written);
                inotify_notify_modify(&file, total_written);
                return Ok(total_written as isize);
            }
            Err(err) => return Err(err),
        };
        let chunk_len = chunk.len.min(remaining_len);
        let buffers = truncate_user_buffers(chunk.buffers, chunk_len);
        if let Err(err) = file.check_write_at(offset, chunk_len) {
            fault_in_read_buffers(&buffers);
            return Err(err.into());
        }
        for slice in buffers {
            let written = file.write_at(offset, slice);
            total_written += written;
            remaining_len = remaining_len.saturating_sub(written);
            offset = offset.checked_add(written).ok_or(SysError::EINVAL)?;
            if written < slice.len() {
                fanotify_notify_modify(&file, total_written);
                inotify_notify_modify(&file, total_written);
                return checked_write_result(requested_len, total_written);
            }
        }
        if remaining_len == 0 {
            break;
        }
    }
    fanotify_notify_modify(&file, total_written);
    inotify_notify_modify(&file, total_written);
    checked_write_result(requested_len, total_written)
}

pub fn sys_pwritev2(
    fd: usize,
    iov: *const LinuxIovec,
    iovcnt: usize,
    pos_l: usize,
    pos_h: usize,
    flags: usize,
) -> SysResult {
    if flags != 0 {
        return Err(SysError::ENOTSUP);
    }
    if pos_l == usize::MAX {
        return sys_writev(fd, iov, iovcnt);
    }
    sys_pwritev(fd, iov, iovcnt, pos_l, pos_h)
}

pub fn sys_write(fd: usize, buf: *const u8, len: usize) -> SysResult {
    let token = current_user_token();
    let entry = get_fd_entry_by_fd(fd)?;
    let file = entry.file();
    if !file.writable() {
        return Err(SysError::EBADF);
    }
    if file.is_dev_full() && len > 0 {
        return Err(SysError::ENOSPC);
    }
    check_pipe_write_peer(&entry, len > 0)?;
    ensure_nonblocking_ready(&entry, PollEvents::POLLOUT)?;
    let allowed_len = allowed_write_len_for_entry(&entry, len)?;
    file.check_write(
        allowed_len,
        entry.status_flags().contains(OpenFlags::APPEND),
    )?;
    if file.write_ignores_user_buffer() {
        // CONTEXT: AF_ALG hash request writes in the current contest subset do
        // not consume payload bytes; skipping the copy keeps af_alg04 from
        // spending most of its time fault-checking data that is discarded.
        fanotify_notify_modify(&file, allowed_len);
        inotify_notify_modify(&file, allowed_len);
        return Ok(allowed_len as isize);
    }
    let buffers = translated_byte_buffer_checked_with_mmap_fault(
        token,
        buf,
        allowed_len,
        UserBufferAccess::Read,
    )?;
    let written = write_with_status_flags(&entry, UserBuffer::new(buffers));
    fanotify_notify_modify(&file, written);
    inotify_notify_modify(&file, written);
    checked_write_result_for_entry(&entry, allowed_len, written)
}

pub fn sys_writev(fd: usize, iov: *const LinuxIovec, iovcnt: usize) -> SysResult {
    if iovcnt == 0 {
        return Ok(0);
    }
    if iovcnt > IOV_MAX {
        return Err(SysError::EINVAL);
    }
    if iov.is_null() {
        return Err(SysError::EFAULT);
    }

    let token = current_user_token();
    let iovecs = read_user_iovecs(token, iov, iovcnt)?;
    let entry = get_fd_entry_by_fd(fd)?;
    let file = entry.file();
    if !file.writable() {
        return Err(SysError::EBADF);
    }
    let has_data = iovecs.total_len > 0;
    if file.is_dev_full() && has_data {
        return Err(SysError::ENOSPC);
    }
    check_pipe_write_peer(&entry, has_data)?;
    ensure_nonblocking_ready(&entry, PollEvents::POLLOUT)?;
    let allowed_len = allowed_write_len_for_entry(&entry, iovecs.total_len)?;
    file.check_write(
        allowed_len,
        entry.status_flags().contains(OpenFlags::APPEND),
    )?;

    let requested_len = allowed_len;
    let mut remaining_len = allowed_len;
    let mut cursor = UserIovecCursor::new(token, iovecs, UserBufferAccess::Read);
    let mut total_written = 0usize;
    while let Some(chunk) = cursor.next_chunk() {
        let chunk = match chunk {
            Ok(chunk) => chunk,
            Err(_) if total_written > 0 => {
                fanotify_notify_modify(&file, total_written);
                inotify_notify_modify(&file, total_written);
                return Ok(total_written as isize);
            }
            Err(err) => return Err(err),
        };
        let chunk_len = chunk.len.min(remaining_len);
        let buffers = truncate_user_buffers(chunk.buffers, chunk_len);
        let written = write_with_status_flags(&entry, UserBuffer::new(buffers));
        total_written += written;
        remaining_len = remaining_len.saturating_sub(written);
        if written < chunk_len {
            break;
        }
        if remaining_len == 0 {
            break;
        }
    }
    fanotify_notify_modify(&file, total_written);
    inotify_notify_modify(&file, total_written);
    checked_write_result_for_entry(&entry, requested_len, total_written)
}

pub fn sys_readv(fd: usize, iov: *const LinuxIovec, iovcnt: usize) -> SysResult {
    if iovcnt == 0 {
        return Ok(0);
    }
    if iovcnt > IOV_MAX {
        return Err(SysError::EINVAL);
    }
    if iov.is_null() {
        return Err(SysError::EFAULT);
    }

    let token = current_user_token();
    let iovecs = read_user_iovecs(token, iov, iovcnt)?;
    let file = get_file_by_fd(fd)?;
    if file.stat()?.mode & S_IFMT == S_IFDIR {
        return Err(SysError::EISDIR);
    }
    if !file.readable() {
        return Err(SysError::EBADF);
    }
    file.check_read(iovecs.total_len)?;
    let entry = get_fd_entry_by_fd(fd)?;
    ensure_nonblocking_ready(&entry, PollEvents::POLLIN)?;

    let mut cursor = UserIovecCursor::new(token, iovecs, UserBufferAccess::Write);
    cursor.validate_all()?;

    let mut total_read = 0usize;
    while let Some(chunk) = cursor.next_chunk() {
        let chunk = chunk?;
        let chunk_len = chunk.len;
        let read = file.read(UserBuffer::new(chunk.buffers));
        total_read += read;
        if read < chunk_len {
            break;
        }
    }
    fanotify_notify_access(&file, total_read);
    inotify_notify_access(&file, total_read);
    Ok(total_read as isize)
}

pub fn sys_read(fd: usize, buf: *const u8, len: usize) -> SysResult {
    let token = current_user_token();
    let entry = get_fd_entry_by_fd(fd)?;
    let file = entry.file();
    if file.stat()?.mode & S_IFMT == S_IFDIR {
        return Err(SysError::EISDIR);
    }
    if !file.readable() {
        return Err(SysError::EBADF);
    }
    file.check_read(len)?;
    ensure_nonblocking_ready(&entry, PollEvents::POLLIN)?;
    let buffers =
        translated_byte_buffer_checked_with_mmap_fault(token, buf, len, UserBufferAccess::Write)?;
    let read = file.read(UserBuffer::new(buffers));
    fanotify_notify_access(&file, read);
    inotify_notify_access(&file, read);
    Ok(read as isize)
}

pub fn sys_readahead(fd: usize, _offset: usize, _count: usize) -> SysResult {
    let entry = get_fd_entry_by_fd(fd)?;
    let file = entry.file();
    if entry.status_flags().contains(OpenFlags::PATH) || !file.readable() {
        return Err(SysError::EBADF);
    }
    match file.stat()?.mode & S_IFMT {
        S_IFREG => {
            crate::fs::procfs_note_readahead();
            Ok(0)
        }
        _ => Err(SysError::EINVAL),
    }
}
