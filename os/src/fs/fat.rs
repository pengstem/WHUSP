use super::dirent::{DT_DIR, DT_REG, RawDirEntry, write_dir_entries};
use super::mount::BlockPartition;
use super::vfs::{FileSystemBackend, FileSystemStat, FsError, FsNodeKind, FsResult};
use super::{FileStat, FileTimestamp, S_IFDIR, S_IFREG};
use crate::drivers::block::VirtIOBlock;
use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::cmp::min;
use fatfs::{
    DefaultTimeProvider, Error as FatError, FileSystem, FsOptions, IoBase, LossyOemCpConverter,
    Read, Seek, SeekFrom, Write,
};

const FAT_SECTOR_SIZE: usize = 512;
const ROOT_INO: u32 = 2;
const MSDOS_SUPER_MAGIC: i64 = 0x4d44;

type KernelFatFs = FileSystem<FatBlockDevice, DefaultTimeProvider, LossyOemCpConverter>;
type KernelFatDir<'a> = fatfs::Dir<'a, FatBlockDevice, DefaultTimeProvider, LossyOemCpConverter>;
type KernelFatFile<'a> = fatfs::File<'a, FatBlockDevice, DefaultTimeProvider, LossyOemCpConverter>;

#[derive(Clone)]
struct FatBlockDevice {
    dev: Arc<VirtIOBlock>,
    start_block: u64,
    block_count: u64,
    position: u64,
}

pub(super) struct FatMount {
    fs: KernelFatFs,
    next_ino: u32,
    path_to_ino: BTreeMap<String, u32>,
    ino_to_path: BTreeMap<u32, String>,
    ino_to_kind: BTreeMap<u32, FsNodeKind>,
}

impl FatBlockDevice {
    fn new(dev: Arc<VirtIOBlock>, partition: BlockPartition) -> Self {
        Self {
            dev,
            start_block: partition.start_block,
            block_count: partition.block_count,
            position: 0,
        }
    }

    fn size(&self) -> u64 {
        self.block_count * FAT_SECTOR_SIZE as u64
    }

    fn bounded_len(&self, len: usize) -> usize {
        if self.position >= self.size() {
            return 0;
        }
        min(len as u64, self.size() - self.position) as usize
    }
}

impl IoBase for FatBlockDevice {
    type Error = ();
}

impl Read for FatBlockDevice {
    fn read(&mut self, buf: &mut [u8]) -> Result<usize, Self::Error> {
        let len = self.bounded_len(buf.len());
        let mut done = 0usize;
        while done < len {
            let sector = self.position / FAT_SECTOR_SIZE as u64;
            let sector_offset = (self.position % FAT_SECTOR_SIZE as u64) as usize;
            let chunk_len = min(FAT_SECTOR_SIZE - sector_offset, len - done);
            if sector_offset == 0 && chunk_len == FAT_SECTOR_SIZE {
                self.dev.read_block(
                    (self.start_block + sector) as usize,
                    &mut buf[done..done + FAT_SECTOR_SIZE],
                );
            } else {
                let mut bounce = [0u8; FAT_SECTOR_SIZE];
                self.dev
                    .read_block((self.start_block + sector) as usize, &mut bounce);
                buf[done..done + chunk_len]
                    .copy_from_slice(&bounce[sector_offset..sector_offset + chunk_len]);
            }
            self.position += chunk_len as u64;
            done += chunk_len;
        }
        Ok(done)
    }
}

impl Write for FatBlockDevice {
    fn write(&mut self, buf: &[u8]) -> Result<usize, Self::Error> {
        let len = self.bounded_len(buf.len());
        let mut done = 0usize;
        while done < len {
            let sector = self.position / FAT_SECTOR_SIZE as u64;
            let sector_offset = (self.position % FAT_SECTOR_SIZE as u64) as usize;
            let chunk_len = min(FAT_SECTOR_SIZE - sector_offset, len - done);
            if sector_offset == 0 && chunk_len == FAT_SECTOR_SIZE {
                self.dev.write_block(
                    (self.start_block + sector) as usize,
                    &buf[done..done + FAT_SECTOR_SIZE],
                );
            } else {
                let mut bounce = [0u8; FAT_SECTOR_SIZE];
                self.dev
                    .read_block((self.start_block + sector) as usize, &mut bounce);
                bounce[sector_offset..sector_offset + chunk_len]
                    .copy_from_slice(&buf[done..done + chunk_len]);
                self.dev
                    .write_block((self.start_block + sector) as usize, &bounce);
            }
            self.position += chunk_len as u64;
            done += chunk_len;
        }
        Ok(done)
    }

    fn flush(&mut self) -> Result<(), Self::Error> {
        Ok(())
    }
}

impl Seek for FatBlockDevice {
    fn seek(&mut self, pos: SeekFrom) -> Result<u64, Self::Error> {
        let size = self.size();
        let new_position = match pos {
            SeekFrom::Start(pos) => Some(pos),
            SeekFrom::Current(off) => self.position.checked_add_signed(off),
            SeekFrom::End(off) => size.checked_add_signed(off),
        }
        .ok_or(())?;
        if new_position > size {
            return Err(());
        }
        self.position = new_position;
        Ok(new_position)
    }
}

fn map_fat_error(err: FatError<()>) -> FsError {
    match err {
        FatError::NotFound => FsError::NotFound,
        FatError::AlreadyExists => FsError::AlreadyExists,
        FatError::DirectoryIsNotEmpty => FsError::NotEmpty,
        FatError::InvalidInput | FatError::UnsupportedFileNameCharacter => FsError::InvalidInput,
        FatError::InvalidFileNameLength => FsError::NameTooLong,
        FatError::NotEnoughSpace => FsError::NoSpace,
        FatError::UnexpectedEof
        | FatError::WriteZero
        | FatError::CorruptedFileSystem
        | FatError::Io(()) => FsError::Io,
        _ => FsError::Io,
    }
}

fn rel_path(path: &str) -> &str {
    path.strip_prefix('/').unwrap_or(path)
}

fn child_path(parent_path: &str, name: &str) -> String {
    if parent_path == "/" {
        alloc::format!("/{name}")
    } else {
        alloc::format!("{parent_path}/{name}")
    }
}

fn parent_path(path: &str) -> String {
    if path == "/" {
        return "/".into();
    }
    match path.rsplit_once('/') {
        Some(("", _)) => "/".into(),
        Some((parent, _)) => parent.into(),
        None => "/".into(),
    }
}

fn node_kind(
    entry: &fatfs::DirEntry<'_, FatBlockDevice, DefaultTimeProvider, LossyOemCpConverter>,
) -> FsNodeKind {
    if entry.is_dir() {
        FsNodeKind::Directory
    } else if entry.is_file() {
        FsNodeKind::RegularFile
    } else {
        FsNodeKind::Other
    }
}

impl FatMount {
    pub(super) fn open(
        dev: Arc<VirtIOBlock>,
        partition: BlockPartition,
    ) -> Result<Self, FatError<()>> {
        let fs = KernelFatFs::new(FatBlockDevice::new(dev, partition), FsOptions::new())?;
        let mut path_to_ino = BTreeMap::new();
        let mut ino_to_path = BTreeMap::new();
        let mut ino_to_kind = BTreeMap::new();
        path_to_ino.insert("/".into(), ROOT_INO);
        ino_to_path.insert(ROOT_INO, "/".into());
        ino_to_kind.insert(ROOT_INO, FsNodeKind::Directory);
        Ok(Self {
            fs,
            next_ino: ROOT_INO + 1,
            path_to_ino,
            ino_to_path,
            ino_to_kind,
        })
    }

    fn dir_for_path(&self, path: &str) -> Result<KernelFatDir<'_>, FatError<()>> {
        let root = self.fs.root_dir();
        let rel = rel_path(path);
        if rel.is_empty() {
            Ok(root)
        } else {
            root.open_dir(rel)
        }
    }

    fn file_for_path(&self, path: &str) -> Result<KernelFatFile<'_>, FatError<()>> {
        self.fs.root_dir().open_file(rel_path(path))
    }

    fn path_for_ino(&self, ino: u32) -> FsResult<String> {
        self.ino_to_path.get(&ino).cloned().ok_or(FsError::NotFound)
    }

    fn intern_path(&mut self, path: String, kind: FsNodeKind) -> u32 {
        if let Some(ino) = self.path_to_ino.get(&path).copied() {
            self.ino_to_kind.insert(ino, kind);
            return ino;
        }
        let ino = self.next_ino;
        self.next_ino += 1;
        self.path_to_ino.insert(path.clone(), ino);
        self.ino_to_path.insert(ino, path);
        self.ino_to_kind.insert(ino, kind);
        ino
    }

    fn forget_path(&mut self, path: &str) {
        if let Some(ino) = self.path_to_ino.remove(path) {
            // UNFINISHED: Linux keeps unlinked-but-open files alive. This FAT
            // adapter forgets the path immediately, so old file handles cannot
            // continue accessing removed entries by inode.
            self.ino_to_path.remove(&ino);
            self.ino_to_kind.remove(&ino);
        }
    }

    fn lookup_child(&mut self, parent_path: &str, component: &str) -> FsResult<(u32, FsNodeKind)> {
        let found = {
            let dir = self.dir_for_path(parent_path).map_err(map_fat_error)?;
            let mut found = None;
            for entry in dir.iter() {
                let entry = entry.map_err(map_fat_error)?;
                if entry.eq_name(component) {
                    found = Some((entry.file_name(), node_kind(&entry)));
                    break;
                }
            }
            found
        }
        .ok_or(FsError::NotFound)?;
        let path = child_path(parent_path, &found.0);
        let ino = self.intern_path(path, found.1);
        Ok((ino, found.1))
    }

    fn parent_ino_for(&mut self, path: &str) -> u32 {
        let parent = parent_path(path);
        self.intern_path(parent, FsNodeKind::Directory)
    }

    fn dir_entries(&mut self, ino: u32) -> FsResult<Vec<RawDirEntry>> {
        let path = self.path_for_ino(ino)?;
        let parent_ino = self.parent_ino_for(&path);
        let children = {
            let dir = self.dir_for_path(&path).map_err(map_fat_error)?;
            let mut children = Vec::new();
            for entry in dir.iter() {
                let entry = entry.map_err(map_fat_error)?;
                let name = entry.file_name();
                if name == "." || name == ".." {
                    continue;
                }
                children.push((name, node_kind(&entry)));
            }
            children
        };
        let mut entries = Vec::new();
        entries.push(RawDirEntry {
            ino,
            name: ".".into(),
            dtype: DT_DIR,
        });
        entries.push(RawDirEntry {
            ino: parent_ino,
            name: "..".into(),
            dtype: DT_DIR,
        });
        for (name, kind) in children {
            let child_ino = self.intern_path(child_path(&path, &name), kind);
            entries.push(RawDirEntry {
                ino: child_ino,
                name,
                dtype: match kind {
                    FsNodeKind::Directory => DT_DIR,
                    FsNodeKind::RegularFile => DT_REG,
                    _ => 0,
                },
            });
        }
        Ok(entries)
    }

    fn grow_file(file: &mut KernelFatFile<'_>, len: u64) -> FsResult {
        let mut pos = file.seek(SeekFrom::End(0)).map_err(map_fat_error)?;
        let zeroes = [0u8; FAT_SECTOR_SIZE];
        while pos < len {
            let chunk = min((len - pos) as usize, zeroes.len());
            let written = file.write(&zeroes[..chunk]).map_err(map_fat_error)?;
            if written == 0 {
                return Err(FsError::NoSpace);
            }
            pos += written as u64;
        }
        Ok(())
    }
}

impl FileSystemBackend for FatMount {
    fn root_ino(&self) -> u32 {
        ROOT_INO
    }

    fn statfs(&mut self) -> FileSystemStat {
        match self.fs.stats() {
            Ok(stats) => FileSystemStat {
                magic: MSDOS_SUPER_MAGIC,
                block_size: stats.cluster_size() as u64,
                blocks: stats.total_clusters() as u64,
                free_blocks: stats.free_clusters() as u64,
                available_blocks: stats.free_clusters() as u64,
                files: 0,
                free_files: 0,
                max_name_len: 255,
                flags: 0,
            },
            Err(_) => FileSystemStat {
                magic: MSDOS_SUPER_MAGIC,
                block_size: FAT_SECTOR_SIZE as u64,
                blocks: 0,
                free_blocks: 0,
                available_blocks: 0,
                files: 0,
                free_files: 0,
                max_name_len: 255,
                flags: 0,
            },
        }
    }

    fn lookup_component_from(
        &mut self,
        parent_ino: u32,
        component: &str,
    ) -> FsResult<(u32, FsNodeKind)> {
        let parent = self.path_for_ino(parent_ino)?;
        match component {
            "." => Ok((parent_ino, FsNodeKind::Directory)),
            ".." => {
                let parent_ino = self.parent_ino_for(&parent);
                Ok((parent_ino, FsNodeKind::Directory))
            }
            _ => self.lookup_child(&parent, component),
        }
    }

    fn create_file(&mut self, parent_ino: u32, leaf_name: &str) -> FsResult<u32> {
        let parent = self.path_for_ino(parent_ino)?;
        self.dir_for_path(&parent)
            .map_err(map_fat_error)?
            .create_file(leaf_name)
            .map_err(map_fat_error)?;
        Ok(self.intern_path(child_path(&parent, leaf_name), FsNodeKind::RegularFile))
    }

    fn create_dir(&mut self, parent_ino: u32, leaf_name: &str, _mode: u32) -> FsResult<u32> {
        let parent = self.path_for_ino(parent_ino)?;
        self.dir_for_path(&parent)
            .map_err(map_fat_error)?
            .create_dir(leaf_name)
            .map_err(map_fat_error)?;
        Ok(self.intern_path(child_path(&parent, leaf_name), FsNodeKind::Directory))
    }

    fn link(&mut self, _parent_ino: u32, _leaf_name: &str, _child_ino: u32) -> FsResult {
        // UNFINISHED: FAT has no Unix hard-link model, and this adapter does
        // not emulate hard links above the FAT directory-entry layer.
        Err(FsError::PermissionDenied)
    }

    fn symlink(&mut self, _parent_ino: u32, _leaf_name: &str, _target: &[u8]) -> FsResult {
        // UNFINISHED: FAT/VFAT does not provide POSIX symlinks in the format
        // expected by Linux filesystems, so symlink creation is unsupported.
        Err(FsError::Unsupported)
    }

    fn unlink(&mut self, parent_ino: u32, leaf_name: &str) -> FsResult {
        let parent = self.path_for_ino(parent_ino)?;
        self.dir_for_path(&parent)
            .map_err(map_fat_error)?
            .remove(leaf_name)
            .map_err(map_fat_error)?;
        self.forget_path(&child_path(&parent, leaf_name));
        Ok(())
    }

    fn rename(&mut self, src_dir: u32, src_name: &str, dst_dir: u32, dst_name: &str) -> FsResult {
        let src_parent = self.path_for_ino(src_dir)?;
        let dst_parent = self.path_for_ino(dst_dir)?;
        let src_path = child_path(&src_parent, src_name);
        let dst_path = child_path(&dst_parent, dst_name);
        self.dir_for_path(&src_parent)
            .map_err(map_fat_error)?
            .rename(
                src_name,
                &self.dir_for_path(&dst_parent).map_err(map_fat_error)?,
                dst_name,
            )
            .map_err(map_fat_error)?;
        if let Some(ino) = self.path_to_ino.remove(&src_path) {
            self.path_to_ino.insert(dst_path.clone(), ino);
            self.ino_to_path.insert(ino, dst_path);
        }
        Ok(())
    }

    fn set_len(&mut self, ino: u32, len: u64) -> FsResult {
        let path = self.path_for_ino(ino)?;
        let mut file = self.file_for_path(&path).map_err(map_fat_error)?;
        let size = file.size().unwrap_or(0) as u64;
        if len <= size {
            file.seek(SeekFrom::Start(len)).map_err(map_fat_error)?;
            file.truncate().map_err(map_fat_error)
        } else {
            Self::grow_file(&mut file, len)
        }
    }

    fn set_times(
        &mut self,
        _ino: u32,
        _atime: Option<FileTimestamp>,
        _mtime: Option<FileTimestamp>,
        _ctime: FileTimestamp,
    ) -> FsResult {
        // UNFINISHED: FAT/VFAT stores timestamps in DOS date/time fields with
        // different precision and timezone rules; this first adapter does not
        // translate Linux utimensat timestamps back into directory entries yet.
        Err(FsError::Unsupported)
    }

    fn stat(&mut self, ino: u32) -> FsResult<FileStat> {
        let path = self.path_for_ino(ino)?;
        if path == "/" {
            return Ok(FileStat {
                ino: ROOT_INO as u64,
                mode: S_IFDIR | 0o777,
                nlink: 2,
                blksize: self.fs.bytes_per_sector() as u32,
                ..FileStat::default()
            });
        }
        let parent = parent_path(&path);
        let name = path.rsplit('/').next().ok_or(FsError::NotFound)?;
        let entry = {
            let dir = self.dir_for_path(&parent).map_err(map_fat_error)?;
            let mut found = None;
            for entry in dir.iter() {
                let entry = entry.map_err(map_fat_error)?;
                if entry.eq_name(name) {
                    found = Some((entry.len(), node_kind(&entry)));
                    break;
                }
            }
            found
        }
        .ok_or(FsError::NotFound)?;
        let mode = match entry.1 {
            FsNodeKind::Directory => S_IFDIR | 0o777,
            FsNodeKind::RegularFile => S_IFREG | 0o666,
            _ => S_IFREG | 0o666,
        };
        // UNFINISHED: FAT timestamps are DOS date/time values. This first
        // mount wrapper does not yet translate them into Linux stat timestamps.
        Ok(FileStat {
            ino: ino as u64,
            mode,
            nlink: if entry.1 == FsNodeKind::Directory {
                2
            } else {
                1
            },
            size: if entry.1 == FsNodeKind::Directory {
                0
            } else {
                entry.0
            },
            blocks: entry.0.div_ceil(FAT_SECTOR_SIZE as u64),
            blksize: self.fs.bytes_per_sector() as u32,
            ..FileStat::default()
        })
    }

    fn readlink(&mut self, _ino: u32, _buf: &mut [u8]) -> FsResult<usize> {
        // UNFINISHED: FAT symlink-like Windows reparse points are not exposed
        // as Linux symlinks by this adapter.
        Err(FsError::InvalidInput)
    }

    fn read_at(&mut self, ino: u32, mut buf: &mut [u8], offset: u64) -> usize {
        let Ok(path) = self.path_for_ino(ino) else {
            return 0;
        };
        let Ok(mut file) = self.file_for_path(&path) else {
            return 0;
        };
        if file.seek(SeekFrom::Start(offset)).is_err() {
            return 0;
        }
        let mut read = 0usize;
        while !buf.is_empty() {
            let Ok(size) = file.read(buf) else {
                break;
            };
            if size == 0 {
                break;
            }
            read += size;
            buf = &mut buf[size..];
        }
        read
    }

    fn write_at(&mut self, ino: u32, mut buf: &[u8], offset: u64) -> usize {
        let Ok(path) = self.path_for_ino(ino) else {
            return 0;
        };
        let Ok(mut file) = self.file_for_path(&path) else {
            return 0;
        };
        let size = file.size().unwrap_or(0) as u64;
        if offset > size && Self::grow_file(&mut file, offset).is_err() {
            return 0;
        }
        if file.seek(SeekFrom::Start(offset)).is_err() {
            return 0;
        }
        let mut written = 0usize;
        while !buf.is_empty() {
            let Ok(size) = file.write(buf) else {
                break;
            };
            if size == 0 {
                break;
            }
            written += size;
            buf = &buf[size..];
        }
        written
    }

    fn read_dirent64(&mut self, ino: u32, offset: u64, buf: &mut [u8]) -> FsResult<(usize, u64)> {
        write_dir_entries(&self.dir_entries(ino)?, offset, buf)
    }

    fn list_root_names(&mut self) -> Vec<String> {
        let mut names = Vec::new();
        let Ok(root) = self.dir_for_path("/") else {
            return names;
        };
        for entry in root.iter() {
            let Ok(entry) = entry else {
                break;
            };
            let name = entry.file_name();
            if name != "." && name != ".." {
                names.push(name);
            }
        }
        names
    }
}
