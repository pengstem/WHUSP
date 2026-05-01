use super::super::{FileStat, FileTimestamp};
use super::FsError;
use super::FsResult;
use alloc::string::String;
use alloc::vec::Vec;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum FsNodeKind {
    Directory,
    RegularFile,
    Symlink,
    Other,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct FileSystemStat {
    pub(crate) magic: i64,
    pub(crate) block_size: u64,
    pub(crate) blocks: u64,
    pub(crate) free_blocks: u64,
    pub(crate) available_blocks: u64,
    pub(crate) files: u64,
    pub(crate) free_files: u64,
    pub(crate) max_name_len: u64,
    pub(crate) flags: u64,
}

pub(crate) trait FileSystemBackend: Send {
    fn root_ino(&self) -> u32 {
        2
    }

    fn statfs(&mut self) -> FileSystemStat {
        FileSystemStat {
            magic: 0,
            block_size: 4096,
            blocks: 0,
            free_blocks: 0,
            available_blocks: 0,
            files: 1024,
            free_files: 1024,
            max_name_len: 255,
            flags: 0,
        }
    }

    fn lookup_component_from(
        &mut self,
        parent_ino: u32,
        component: &str,
    ) -> FsResult<(u32, FsNodeKind)>;
    fn create_file(&mut self, parent_ino: u32, leaf_name: &str) -> FsResult<u32>;
    fn create_dir(&mut self, parent_ino: u32, leaf_name: &str, mode: u32) -> FsResult<u32>;
    fn link(&mut self, parent_ino: u32, leaf_name: &str, child_ino: u32) -> FsResult;
    fn symlink(&mut self, parent_ino: u32, leaf_name: &str, target: &[u8]) -> FsResult;
    fn unlink(&mut self, parent_ino: u32, leaf_name: &str) -> FsResult;
    fn rename(&mut self, src_dir: u32, src_name: &str, dst_dir: u32, dst_name: &str) -> FsResult;
    fn set_len(&mut self, ino: u32, len: u64) -> FsResult;
    fn set_times(
        &mut self,
        _ino: u32,
        _atime: Option<FileTimestamp>,
        _mtime: Option<FileTimestamp>,
        _ctime: FileTimestamp,
    ) -> FsResult {
        Err(FsError::Unsupported)
    }
    fn stat(&mut self, ino: u32) -> FsResult<FileStat>;
    fn readlink(&mut self, ino: u32, buf: &mut [u8]) -> FsResult<usize>;
    fn read_at(&mut self, ino: u32, buf: &mut [u8], offset: u64) -> usize;
    fn write_at(&mut self, ino: u32, buf: &[u8], offset: u64) -> usize;
    fn read_dirent64(&mut self, ino: u32, offset: u64, buf: &mut [u8]) -> FsResult<(usize, u64)>;
    fn list_root_names(&mut self) -> Vec<String>;
}
