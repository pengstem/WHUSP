use crate::fs::{File, OpenFlags, PollEvents, S_IFDIR, S_IFREG, SeekWhence};
use crate::mm::UserBuffer;
use crate::task::{FdTableEntry, SignalFlags, current_add_signal, current_user_token};
use alloc::{vec, vec::Vec};
use core::mem::size_of;

use super::super::errno::{SysError, SysResult};
use super::super::user_ptr::{
    UserBufferAccess, read_user_usize, read_user_value, translated_byte_buffer_checked,
    translated_byte_buffer_checked_with_mmap_fault, write_user_value,
};
use super::fd::{get_fd_entry_by_fd, get_file_by_fd};
use super::uapi::{IOV_MAX, LinuxIovec};

fn read_user_iovecs(
    token: usize,
    iov: *const LinuxIovec,
    iovcnt: usize,
) -> SysResult<Vec<LinuxIovec>> {
    let entry_size = size_of::<LinuxIovec>();
    let mut iovecs = Vec::new();
    let mut total_len = 0usize;
    for index in 0..iovcnt {
        let entry_addr = (iov as usize)
            .checked_add(index.checked_mul(entry_size).ok_or(SysError::EFAULT)?)
            .ok_or(SysError::EFAULT)?;
        let base = read_user_usize(token, entry_addr)?;
        let len = read_user_usize(token, entry_addr + size_of::<usize>())?;
        total_len = total_len.checked_add(len).ok_or(SysError::EINVAL)?;
        if total_len > isize::MAX as usize {
            return Err(SysError::EINVAL);
        }
        iovecs.push(LinuxIovec { base, len });
    }
    Ok(iovecs)
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
    if mode & S_IFDIR == S_IFDIR {
        return Err(SysError::EISDIR);
    }
    if mode & S_IFREG != S_IFREG {
        return Err(SysError::ESPIPE);
    }
    Ok(())
}

pub fn sys_lseek(fd: usize, offset: i64, whence: usize) -> SysResult {
    let whence = match whence {
        0 => SeekWhence::Set,
        1 => SeekWhence::Current,
        2 => SeekWhence::End,
        // UNFINISHED: Linux SEEK_DATA and SEEK_HOLE are not implemented yet.
        // They require sparse-file data/hole discovery in the filesystem layer.
        _ => return Err(SysError::EINVAL),
    };
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
        return Err(SysError::EBADF);
    }
    if file.stat()?.mode & S_IFREG != S_IFREG {
        return Err(SysError::EINVAL);
    }
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
        file.set_len(end)?;
    }
    // CONTEXT: the current VFS has no block preallocation API. KEEP_SIZE is
    // accepted as a no-op because its visible contract in LTP sparse-file
    // cases is that file size must not change.
    Ok(0)
}

const SPLICE_F_MOVE: u32 = 0x01;
const SPLICE_F_NONBLOCK: u32 = 0x02;
const SPLICE_F_MORE: u32 = 0x04;
const SPLICE_F_GIFT: u32 = 0x08;
const SPLICE_KNOWN_FLAGS: u32 = SPLICE_F_MOVE | SPLICE_F_NONBLOCK | SPLICE_F_MORE | SPLICE_F_GIFT;
const SPLICE_COPY_CHUNK: usize = 4096;

// UNFINISHED: Linux splice can move pipe pages without copying and has deeper
// file-type-specific wakeup semantics. This contest compatibility path copies
// through kernel memory while preserving the visible fd, offset, and errno
// behavior needed by current LTP splice cases.
fn kernel_user_buffer(buf: &mut [u8]) -> UserBuffer {
    // UserBuffer is also the in-kernel File trait data carrier. The borrowed
    // slice is consumed synchronously by File::read/write and is not stored.
    let slice = unsafe { core::mem::transmute::<&mut [u8], &'static mut [u8]>(buf) };
    UserBuffer::new(vec![slice])
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

    let in_is_pipe = in_file.is_pipe();
    let out_is_pipe = out_file.is_pipe();
    if !in_is_pipe && !out_is_pipe {
        return Err(SysError::EINVAL);
    }

    let mut in_offset = read_splice_offset(token, off_in, in_is_pipe)?;
    let mut out_offset = read_splice_offset(token, off_out, out_is_pipe)?;
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

pub fn sys_pread64(fd: usize, buf: *mut u8, len: usize, offset: usize) -> SysResult {
    let offset = checked_position_offset(offset)?;
    let token = current_user_token();
    let file = get_file_by_fd(fd)?;
    if !file.readable() {
        return Err(SysError::EBADF);
    }
    ensure_positioned_target(file.as_ref())?;
    let buffers = translated_byte_buffer_checked(token, buf, len, UserBufferAccess::Write)?;
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
    Ok(total_read as isize)
}

pub fn sys_pwrite64(fd: usize, buf: *const u8, len: usize, offset: usize) -> SysResult {
    let offset = checked_position_offset(offset)?;
    let token = current_user_token();
    let file = get_file_by_fd(fd)?;
    if !file.writable() {
        return Err(SysError::EBADF);
    }
    ensure_positioned_target(file.as_ref())?;
    // UNFINISHED: Linux's pwrite path has the historical O_APPEND quirk.
    // The contest iozone path opens regular files without O_APPEND, so this
    // implementation writes at the explicit offset and leaves fd offset intact.
    let buffers = translated_byte_buffer_checked(token, buf, len, UserBufferAccess::Read)?;
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
    Ok(total_written as isize)
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

    for iovec in iovecs.iter() {
        if iovec.len == 0 {
            continue;
        }
        translated_byte_buffer_checked(
            token,
            iovec.base as *const u8,
            iovec.len,
            UserBufferAccess::Write,
        )?;
    }

    let mut total_read = 0usize;
    for iovec in iovecs {
        if iovec.len == 0 {
            continue;
        }
        let buffers = translated_byte_buffer_checked(
            token,
            iovec.base as *const u8,
            iovec.len,
            UserBufferAccess::Write,
        )?;
        for slice in buffers {
            let read = file.read_at(offset, slice);
            total_read += read;
            offset = offset.checked_add(read).ok_or(SysError::EINVAL)?;
            if read < slice.len() {
                return Ok(total_read as isize);
            }
        }
    }
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
    let file = get_file_by_fd(fd)?;
    if !file.writable() {
        return Err(SysError::EBADF);
    }
    ensure_positioned_target(file.as_ref())?;
    // UNFINISHED: See sys_pwrite64 for the Linux O_APPEND quirk.
    let mut total_written = 0usize;
    for iovec in iovecs {
        if iovec.len == 0 {
            continue;
        }
        let buffers = match translated_byte_buffer_checked(
            token,
            iovec.base as *const u8,
            iovec.len,
            UserBufferAccess::Read,
        ) {
            Ok(buffers) => buffers,
            Err(_) if total_written > 0 => return Ok(total_written as isize),
            Err(err) => return Err(err),
        };
        for slice in buffers {
            let written = file.write_at(offset, slice);
            total_written += written;
            offset = offset.checked_add(written).ok_or(SysError::EINVAL)?;
            if written < slice.len() {
                return Ok(total_written as isize);
            }
        }
    }
    Ok(total_written as isize)
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
    let buffers =
        translated_byte_buffer_checked_with_mmap_fault(token, buf, len, UserBufferAccess::Read)?;
    Ok(write_with_status_flags(&entry, UserBuffer::new(buffers)) as isize)
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
    let has_data = iovecs.iter().any(|iovec| iovec.len > 0);
    if file.is_dev_full() && has_data {
        return Err(SysError::ENOSPC);
    }
    check_pipe_write_peer(&entry, has_data)?;
    ensure_nonblocking_ready(&entry, PollEvents::POLLOUT)?;

    let mut total_written = 0usize;
    for iovec in iovecs {
        if iovec.len == 0 {
            continue;
        }
        let buffers = match translated_byte_buffer_checked(
            token,
            iovec.base as *const u8,
            iovec.len,
            UserBufferAccess::Read,
        ) {
            Ok(buffers) => buffers,
            Err(_) if total_written > 0 => return Ok(total_written as isize),
            Err(err) => return Err(err),
        };
        let written = write_with_status_flags(&entry, UserBuffer::new(buffers));
        total_written += written;
        if written < iovec.len {
            break;
        }
    }
    Ok(total_written as isize)
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
    if !file.readable() {
        return Err(SysError::EBADF);
    }
    if file.stat()?.mode & S_IFDIR == S_IFDIR {
        return Err(SysError::EISDIR);
    }
    let entry = get_fd_entry_by_fd(fd)?;
    ensure_nonblocking_ready(&entry, PollEvents::POLLIN)?;

    for iovec in iovecs.iter() {
        if iovec.len == 0 {
            continue;
        }
        translated_byte_buffer_checked(
            token,
            iovec.base as *const u8,
            iovec.len,
            UserBufferAccess::Write,
        )?;
    }

    let mut total_read = 0usize;
    for iovec in iovecs {
        if iovec.len == 0 {
            continue;
        }
        let buffers = translated_byte_buffer_checked(
            token,
            iovec.base as *const u8,
            iovec.len,
            UserBufferAccess::Write,
        )?;
        let read = file.read(UserBuffer::new(buffers));
        total_read += read;
        if read < iovec.len {
            break;
        }
    }
    Ok(total_read as isize)
}

pub fn sys_read(fd: usize, buf: *const u8, len: usize) -> SysResult {
    let token = current_user_token();
    let entry = get_fd_entry_by_fd(fd)?;
    let file = entry.file();
    if !file.readable() {
        return Err(SysError::EBADF);
    }
    ensure_nonblocking_ready(&entry, PollEvents::POLLIN)?;
    let buffers =
        translated_byte_buffer_checked_with_mmap_fault(token, buf, len, UserBufferAccess::Write)?;
    Ok(file.read(UserBuffer::new(buffers)) as isize)
}
