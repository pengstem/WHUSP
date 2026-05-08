use super::inode::OpenFlags;
use super::status_flags::StatusFlagsCell;
use super::{File, FileStat, FsError, FsResult, S_IFREG, SeekWhence};
use crate::mm::UserBuffer;
use crate::sync::SleepMutex;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};

const F_SEAL_SEAL: u32 = 0x0001;
const F_SEAL_SHRINK: u32 = 0x0002;
const F_SEAL_GROW: u32 = 0x0004;
const F_SEAL_WRITE: u32 = 0x0008;
const F_SEAL_KNOWN: u32 = F_SEAL_SEAL | F_SEAL_SHRINK | F_SEAL_GROW | F_SEAL_WRITE;

static MEMFD_INO: AtomicU64 = AtomicU64::new(1);

struct MemfdInner {
    ino: u64,
    data: Vec<u8>,
    seals: u32,
    writable_shared_mmaps: usize,
}

pub(crate) struct MemfdFile {
    inner: Arc<SleepMutex<MemfdInner>>,
    offset: SleepMutex<usize>,
    readable: bool,
    writable: bool,
    status_flags: StatusFlagsCell,
}

impl MemfdFile {
    fn new(inner: Arc<SleepMutex<MemfdInner>>, flags: OpenFlags) -> Self {
        let (readable, writable) = flags.read_write();
        Self {
            inner,
            offset: SleepMutex::new(0),
            readable,
            writable,
            status_flags: StatusFlagsCell::new(OpenFlags::file_status_flags(flags)),
        }
    }

    fn has_seal(inner: &MemfdInner, seal: u32) -> bool {
        inner.seals & seal != 0
    }

    fn check_write_at_inner(inner: &MemfdInner, offset: usize, len: usize) -> FsResult {
        if len == 0 {
            return Ok(());
        }
        if Self::has_seal(inner, F_SEAL_WRITE) {
            return Err(FsError::PermissionDenied);
        }
        let end = offset.checked_add(len).ok_or(FsError::InvalidInput)?;
        if end > inner.data.len() && Self::has_seal(inner, F_SEAL_GROW) {
            return Err(FsError::PermissionDenied);
        }
        Ok(())
    }

    fn check_set_len_inner(inner: &MemfdInner, len: usize) -> FsResult {
        if len < inner.data.len() && Self::has_seal(inner, F_SEAL_SHRINK) {
            return Err(FsError::PermissionDenied);
        }
        if len > inner.data.len() && Self::has_seal(inner, F_SEAL_GROW) {
            return Err(FsError::PermissionDenied);
        }
        Ok(())
    }

    fn write_at_inner(inner: &mut MemfdInner, offset: usize, buf: &[u8]) -> usize {
        if Self::check_write_at_inner(inner, offset, buf.len()).is_err() {
            return 0;
        }
        let Some(end) = offset.checked_add(buf.len()) else {
            return 0;
        };
        if offset > inner.data.len() {
            inner.data.resize(offset, 0);
        }
        if end > inner.data.len() {
            inner.data.resize(end, 0);
        }
        inner.data[offset..end].copy_from_slice(buf);
        buf.len()
    }
}

pub(crate) fn make_memfd(allow_sealing: bool) -> Arc<dyn File + Send + Sync> {
    let seals = if allow_sealing { 0 } else { F_SEAL_SEAL };
    let inner = Arc::new(SleepMutex::new(MemfdInner {
        ino: MEMFD_INO.fetch_add(1, Ordering::Relaxed),
        data: Vec::new(),
        seals,
        writable_shared_mmaps: 0,
    }));
    Arc::new(MemfdFile::new(
        inner,
        OpenFlags::RDWR | OpenFlags::LARGEFILE,
    ))
}

impl File for MemfdFile {
    fn as_any(&self) -> &dyn core::any::Any {
        self
    }

    fn readable(&self) -> bool {
        self.readable
    }

    fn writable(&self) -> bool {
        self.writable
    }

    fn read(&self, mut buf: UserBuffer) -> usize {
        let inner = self.inner.lock();
        let mut offset = self.offset.lock();
        let available = inner.data.len().saturating_sub(*offset);
        let mut copied = 0usize;
        for slice in buf.buffers.iter_mut() {
            if copied == available {
                break;
            }
            let len = slice.len().min(available - copied);
            let start = *offset + copied;
            slice[..len].copy_from_slice(&inner.data[start..start + len]);
            copied += len;
        }
        *offset += copied;
        copied
    }

    fn write(&self, buf: UserBuffer) -> usize {
        let data = buf.to_vec();
        let mut inner = self.inner.lock();
        let mut offset = self.offset.lock();
        let written = Self::write_at_inner(&mut inner, *offset, &data);
        *offset += written;
        written
    }

    fn write_append(&self, buf: UserBuffer) -> usize {
        let data = buf.to_vec();
        let mut inner = self.inner.lock();
        let offset = inner.data.len();
        let written = Self::write_at_inner(&mut inner, offset, &data);
        *self.offset.lock() = offset + written;
        written
    }

    fn stat(&self) -> FsResult<FileStat> {
        let inner = self.inner.lock();
        let mut stat = FileStat::with_mode(S_IFREG | 0o777);
        stat.dev = 0;
        stat.ino = inner.ino;
        stat.nlink = 0;
        stat.size = inner.data.len() as u64;
        Ok(stat)
    }

    fn read_at(&self, offset: usize, buf: &mut [u8]) -> usize {
        let inner = self.inner.lock();
        if offset >= inner.data.len() {
            return 0;
        }
        let len = buf.len().min(inner.data.len() - offset);
        buf[..len].copy_from_slice(&inner.data[offset..offset + len]);
        len
    }

    fn write_at(&self, offset: usize, buf: &[u8]) -> usize {
        let mut inner = self.inner.lock();
        Self::write_at_inner(&mut inner, offset, buf)
    }

    fn set_len(&self, len: usize) -> FsResult {
        if !self.writable {
            return Err(FsError::PermissionDenied);
        }
        let mut inner = self.inner.lock();
        Self::check_set_len_inner(&inner, len)?;
        inner.data.resize(len, 0);
        Ok(())
    }

    fn check_write(&self, len: usize, append: bool) -> FsResult {
        let offset = if append {
            self.inner.lock().data.len()
        } else {
            *self.offset.lock()
        };
        self.check_write_at(offset, len)
    }

    fn check_write_at(&self, offset: usize, len: usize) -> FsResult {
        let inner = self.inner.lock();
        Self::check_write_at_inner(&inner, offset, len)
    }

    fn check_set_len(&self, len: usize) -> FsResult {
        let inner = self.inner.lock();
        Self::check_set_len_inner(&inner, len)
    }

    fn seals(&self) -> FsResult<u32> {
        Ok(self.inner.lock().seals)
    }

    fn add_seals(&self, seals: u32) -> FsResult {
        if !self.writable {
            return Err(FsError::PermissionDenied);
        }
        if seals & !F_SEAL_KNOWN != 0 {
            return Err(FsError::InvalidInput);
        }
        let mut inner = self.inner.lock();
        if Self::has_seal(&inner, F_SEAL_SEAL) {
            return Err(FsError::PermissionDenied);
        }
        if seals & F_SEAL_WRITE != 0 && inner.writable_shared_mmaps > 0 {
            return Err(FsError::Busy);
        }
        inner.seals |= seals;
        Ok(())
    }

    fn reopen_from_proc_fd(&self, flags: OpenFlags) -> FsResult<Arc<dyn File + Send + Sync>> {
        if flags.contains(OpenFlags::DIRECTORY | OpenFlags::TMPFILE | OpenFlags::PATH) {
            return Err(FsError::InvalidInput);
        }
        let file = Arc::new(Self::new(Arc::clone(&self.inner), flags));
        if flags.contains(OpenFlags::TRUNC) {
            file.set_len(0)?;
        }
        Ok(file)
    }

    fn inc_writable_shared_mmap(&self) {
        self.inner.lock().writable_shared_mmaps += 1;
    }

    fn dec_writable_shared_mmap(&self) {
        let mut inner = self.inner.lock();
        inner.writable_shared_mmaps = inner.writable_shared_mmaps.saturating_sub(1);
    }

    fn blocks_shared_writable_mmap(&self) -> bool {
        Self::has_seal(&self.inner.lock(), F_SEAL_WRITE)
    }

    fn blocks_file_write(&self) -> bool {
        Self::has_seal(&self.inner.lock(), F_SEAL_WRITE)
    }

    fn sync(&self, _data_only: bool) -> FsResult {
        Ok(())
    }

    fn seek(&self, offset: i64, whence: SeekWhence) -> FsResult<usize> {
        let mut current = self.offset.lock();
        let size = self.inner.lock().data.len();
        let base = match whence {
            SeekWhence::Set => 0i128,
            SeekWhence::Current => *current as i128,
            SeekWhence::End => size as i128,
        };
        let next = base + offset as i128;
        if next < 0 || next > usize::MAX as i128 {
            return Err(FsError::InvalidInput);
        }
        *current = next as usize;
        Ok(*current)
    }

    fn status_flags(&self) -> OpenFlags {
        self.status_flags.get()
    }

    fn set_status_flags(&self, flags: OpenFlags) {
        self.status_flags.set(flags);
    }

    fn is_memfd(&self) -> bool {
        true
    }
}
