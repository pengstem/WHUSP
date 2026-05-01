use super::align_up;
use super::vfs::{FsError, FsResult};
use alloc::string::String;

pub(super) const LINUX_DIRENT64_HEADER_SIZE: usize = 19;
pub(super) const LINUX_DIRENT64_ALIGN: usize = 8;

pub(super) const DT_UNKNOWN: u8 = 0;
pub(super) const DT_CHR: u8 = 2;
pub(super) const DT_DIR: u8 = 4;
pub(super) const DT_REG: u8 = 8;
pub(super) const DT_LNK: u8 = 10;

pub(super) struct RawDirEntry {
    pub(super) ino: u32,
    pub(super) name: String,
    pub(super) dtype: u8,
}

pub(super) fn write_dir_entries(
    entries: &[RawDirEntry],
    offset: u64,
    buf: &mut [u8],
) -> FsResult<(usize, u64)> {
    let mut written = 0usize;
    let mut entry_index = offset as usize;
    while entry_index < entries.len() {
        let entry = &entries[entry_index];
        let name = entry.name.as_bytes();
        let d_reclen = align_up(
            LINUX_DIRENT64_HEADER_SIZE + name.len() + 1,
            LINUX_DIRENT64_ALIGN,
        );
        if d_reclen > buf.len().saturating_sub(written) {
            if written == 0 {
                return Err(FsError::InvalidInput);
            }
            break;
        }

        let next_offset = entry_index + 1;
        let entry_buf = &mut buf[written..written + d_reclen];
        entry_buf.fill(0);
        entry_buf[0..8].copy_from_slice(&(entry.ino as u64).to_ne_bytes());
        entry_buf[8..16].copy_from_slice(&(next_offset as i64).to_ne_bytes());
        entry_buf[16..18].copy_from_slice(&(d_reclen as u16).to_ne_bytes());
        entry_buf[18] = entry.dtype;
        entry_buf[LINUX_DIRENT64_HEADER_SIZE..LINUX_DIRENT64_HEADER_SIZE + name.len()]
            .copy_from_slice(name);

        written += d_reclen;
        entry_index = next_offset;
    }
    Ok((written, entry_index as u64))
}
