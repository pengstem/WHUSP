use super::super::FileStat;
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
    ) -> Option<(u32, FsNodeKind)>;
    fn create_file(&mut self, parent_ino: u32, leaf_name: &str) -> Option<u32>;
    fn create_dir(&mut self, parent_ino: u32, leaf_name: &str, mode: u32) -> Option<u32>;
    fn unlink(&mut self, parent_ino: u32, leaf_name: &str) -> Option<()>;
    fn set_len(&mut self, ino: u32, len: u64) -> Option<()>;
    fn stat(&mut self, ino: u32) -> Option<FileStat>;
    fn read_at(&mut self, ino: u32, buf: &mut [u8], offset: u64) -> usize;
    fn write_at(&mut self, ino: u32, buf: &[u8], offset: u64) -> usize;
    fn read_dirent64(&mut self, ino: u32, offset: u64, buf: &mut [u8]) -> Option<(usize, u64)>;
    fn list_root_names(&mut self) -> Vec<String>;
}
