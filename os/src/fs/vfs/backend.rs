use super::super::FileStat;
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

pub(crate) trait FileSystemBackend: Send {
    fn lookup_component_from(
        &mut self,
        parent_ino: u32,
        component: &str,
    ) -> FsResult<(u32, FsNodeKind)>;
    fn create_file(&mut self, parent_ino: u32, leaf_name: &str) -> FsResult<u32>;
    fn create_dir(&mut self, parent_ino: u32, leaf_name: &str, mode: u32) -> FsResult<u32>;
    fn unlink(&mut self, parent_ino: u32, leaf_name: &str) -> FsResult;
    fn rename(&mut self, src_dir: u32, src_name: &str, dst_dir: u32, dst_name: &str) -> FsResult;
    fn set_len(&mut self, ino: u32, len: u64) -> FsResult;
    fn stat(&mut self, ino: u32) -> FsResult<FileStat>;
    fn read_at(&mut self, ino: u32, buf: &mut [u8], offset: u64) -> usize;
    fn write_at(&mut self, ino: u32, buf: &[u8], offset: u64) -> usize;
    fn read_dirent64(&mut self, ino: u32, offset: u64, buf: &mut [u8]) -> FsResult<(usize, u64)>;
    fn list_root_names(&mut self) -> Vec<String>;
}
