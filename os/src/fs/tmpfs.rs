use super::dirent::{
    DT_BLK, DT_CHR, DT_DIR, DT_FIFO, DT_LNK, DT_REG, DT_SOCK, RawDirEntry, write_dir_entries,
};
use super::vfs::{FileSystemBackend, FsError, FsNodeKind, FsResult};
use super::{
    FS_ENCRYPT_FL, FS_STATX_ATTR_FLAGS, FS_STATX_COMMON_ATTR_FLAGS, FileStat, FileTimestamp,
    S_IFBLK, S_IFCHR, S_IFDIR, S_IFIFO, S_IFLNK, S_IFREG, S_IFSOCK,
};
use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec::Vec;

const ROOT_INO: u32 = 2;
const TMPFS_MAGIC: i64 = 0x0102_1994;
pub(super) const EXT234_SUPER_MAGIC: i64 = 0xEF53;
const E4CRYPT_ENCRYPTED_MARKER: &str = ".whusp_e4crypt_encrypted";
// CONTEXT: Larger sparse tmpfs files are represented by sparse extents so
// high-offset writes do not require one huge zero-filled heap allocation.
const TMPFS_DEFAULT_INLINE_FILE_LIMIT: usize = 1024 * 1024;
// CONTEXT: LTP ext scratch mounts can create hundreds of small repeated-data
// files. Make those mounts sparse from byte zero so they do not exhaust heap.
const EXT_SCRATCH_INLINE_FILE_LIMIT: usize = 0;
const TMPFS_SPARSE_EXTENT_LIMIT: usize = 64 * 1024;
const TMPFS_ALLOCATED_PAYLOAD_LIMIT: usize = 64 * 1024 * 1024;
const EXT_SCRATCH_SYNC_BYTES: u64 = 64 * 1024 * 1024;

enum TmpfsSparseExtent {
    Bytes(Vec<u8>),
    Repeated { pattern: Vec<u8>, len: usize },
}

impl TmpfsSparseExtent {
    fn len(&self) -> usize {
        match self {
            Self::Bytes(data) => data.len(),
            Self::Repeated { len, .. } => *len,
        }
    }

    fn allocated_len(&self) -> usize {
        match self {
            Self::Bytes(data) => data.len(),
            Self::Repeated { pattern, .. } => pattern.len(),
        }
    }

    fn repeated_byte(buf: &[u8]) -> Option<Self> {
        let (&first, rest) = buf.split_first()?;
        if rest.iter().all(|byte| *byte == first) {
            let mut pattern = Vec::new();
            pattern.push(first);
            Some(Self::Repeated {
                pattern,
                len: buf.len(),
            })
        } else {
            None
        }
    }

    fn matching_pattern_prefix(pattern: &[u8], pattern_offset: usize, buf: &[u8]) -> usize {
        if pattern.is_empty() {
            return 0;
        }
        let mut matched = 0usize;
        while matched < buf.len() {
            let pattern_index = (pattern_offset + matched) % pattern.len();
            let chunk_len = (pattern.len() - pattern_index).min(buf.len() - matched);
            let expected = &pattern[pattern_index..pattern_index + chunk_len];
            let actual = &buf[matched..matched + chunk_len];
            if expected == actual {
                matched += chunk_len;
                continue;
            }
            for index in 0..chunk_len {
                if actual[index] != expected[index] {
                    return matched + index;
                }
            }
        }
        matched
    }

    fn rotated_pattern(pattern: &[u8], offset: usize, len: usize) -> Option<Vec<u8>> {
        if pattern.is_empty() || len == 0 {
            return None;
        }
        let copy_len = pattern.len().min(len);
        let mut rotated = Vec::new();
        if rotated.try_reserve(copy_len).is_err() {
            return None;
        }
        for index in 0..copy_len {
            rotated.push(pattern[(offset + index) % pattern.len()]);
        }
        Some(rotated)
    }

    fn slice(&self, offset: usize, len: usize) -> Option<Self> {
        if len == 0 || offset >= self.len() {
            return None;
        }
        let len = len.min(self.len() - offset);
        match self {
            Self::Bytes(data) => Some(Self::Bytes(data[offset..offset + len].to_vec())),
            Self::Repeated { pattern, .. } => {
                let original_pattern_len = pattern.len();
                let pattern = Self::rotated_pattern(pattern, offset, len)?;
                if len < original_pattern_len {
                    Some(Self::Bytes(pattern))
                } else {
                    Some(Self::Repeated { pattern, len })
                }
            }
        }
    }

    fn copy_to(&self, extent_start: u64, offset: u64, buf: &mut [u8]) {
        let src_start = offset.saturating_sub(extent_start) as usize;
        let dst_start = extent_start.saturating_sub(offset) as usize;
        if src_start >= self.len() || dst_start >= buf.len() {
            return;
        }
        let copy_len = (self.len() - src_start).min(buf.len() - dst_start);
        match self {
            Self::Bytes(data) => {
                buf[dst_start..dst_start + copy_len]
                    .copy_from_slice(&data[src_start..src_start + copy_len]);
            }
            Self::Repeated { pattern, .. } => {
                if pattern.is_empty() {
                    return;
                }
                let mut copied = 0usize;
                while copied < copy_len {
                    let pattern_index = (src_start + copied) % pattern.len();
                    let chunk_len = (pattern.len() - pattern_index).min(copy_len - copied);
                    buf[dst_start + copied..dst_start + copied + chunk_len]
                        .copy_from_slice(&pattern[pattern_index..pattern_index + chunk_len]);
                    copied += chunk_len;
                }
            }
        }
    }

    fn truncate_to(&mut self, len: usize) {
        match self {
            Self::Bytes(data) => data.truncate(len),
            Self::Repeated {
                len: extent_len, ..
            } => *extent_len = (*extent_len).min(len),
        }
    }

    fn try_append(&mut self, buf: &[u8], allocated_payload_len: usize) -> Option<usize> {
        match self {
            Self::Bytes(data) => {
                if !data.is_empty()
                    && Self::matching_pattern_prefix(data, data.len(), buf) == buf.len()
                {
                    let pattern = core::mem::take(data);
                    let Some(len) = pattern.len().checked_add(buf.len()) else {
                        return Some(0);
                    };
                    *self = Self::Repeated { len, pattern };
                    return Some(buf.len());
                }
                if data.len() >= TMPFS_SPARSE_EXTENT_LIMIT {
                    return None;
                }
                let copy_len = buf.len().min(TMPFS_SPARSE_EXTENT_LIMIT - data.len());
                if allocated_payload_len.saturating_add(copy_len) > TMPFS_ALLOCATED_PAYLOAD_LIMIT
                    || data.try_reserve(copy_len).is_err()
                {
                    return Some(0);
                }
                data.extend_from_slice(&buf[..copy_len]);
                Some(copy_len)
            }
            Self::Repeated { pattern, len } => {
                let matched = Self::matching_pattern_prefix(pattern, *len, buf);
                if matched == 0 {
                    return None;
                }
                let Some(new_len) = len.checked_add(matched) else {
                    return Some(0);
                };
                *len = new_len;
                Some(matched)
            }
        }
    }
}

struct TmpfsInode {
    kind: FsNodeKind,
    mode: u32,
    uid: u32,
    gid: u32,
    nlink: u32,
    rdev: u64,
    flags: u32,
    open_count: usize,
    pending_delete: bool,
    size: u64,
    data: Vec<u8>,
    sparse_data: BTreeMap<u64, TmpfsSparseExtent>,
    children: BTreeMap<String, u32>,
    parent_ino: u32,
    atime: FileTimestamp,
    btime: FileTimestamp,
    ctime: FileTimestamp,
    mtime: FileTimestamp,
}

pub(super) struct TmpFs {
    inodes: BTreeMap<u32, TmpfsInode>,
    next_ino: u32,
    statfs_magic: i64,
    logical_quota_bytes: Option<u64>,
    inline_file_limit: usize,
    synthetic_sync_loop_device: Option<usize>,
}

impl TmpfsInode {
    fn new(kind: FsNodeKind, mode: u32, parent_ino: u32, rdev: u64) -> Self {
        let now = FileTimestamp::now();
        let nlink = if kind == FsNodeKind::Directory { 2 } else { 1 };
        Self {
            kind,
            mode,
            uid: 0,
            gid: 0,
            nlink,
            rdev,
            flags: 0,
            open_count: 0,
            pending_delete: false,
            size: 0,
            data: Vec::new(),
            sparse_data: BTreeMap::new(),
            children: BTreeMap::new(),
            parent_ino,
            atime: now,
            btime: now,
            ctime: now,
            mtime: now,
        }
    }

    fn touch(&mut self) {
        let now = FileTimestamp::now();
        self.ctime = now;
        self.mtime = now;
    }

    fn allocated_payload_len(&self) -> usize {
        self.data.len()
            + self
                .sparse_data
                .values()
                .map(TmpfsSparseExtent::allocated_len)
                .sum::<usize>()
    }

    fn allocated_logical_len(&self) -> u64 {
        self.data.len() as u64
            + self
                .sparse_data
                .values()
                .map(|extent| extent.len() as u64)
                .sum::<u64>()
    }

    fn clear_payload(&mut self) {
        self.data.clear();
        self.data.shrink_to_fit();
        self.sparse_data.clear();
    }

    fn remove_sparse_range(&mut self, start: u64, end: u64) {
        if start >= end {
            return;
        }
        let overlapping: Vec<u64> = self
            .sparse_data
            .range(..end)
            .filter_map(|(&extent_start, extent)| {
                let extent_end = extent_start.saturating_add(extent.len() as u64);
                if extent_end > start {
                    Some(extent_start)
                } else {
                    None
                }
            })
            .collect();

        for extent_start in overlapping {
            let Some(extent) = self.sparse_data.remove(&extent_start) else {
                continue;
            };
            let extent_end = extent_start.saturating_add(extent.len() as u64);
            let left = if extent_start < start {
                extent.slice(0, (start - extent_start) as usize)
            } else {
                None
            };
            let right = if extent_end > end {
                let right_offset = (end - extent_start) as usize;
                extent.slice(right_offset, (extent_end - end) as usize)
            } else {
                None
            };
            if extent_start < start
                && let Some(left) = left
            {
                self.sparse_data.insert(extent_start, left);
            }
            if extent_end > end
                && let Some(right) = right
            {
                self.sparse_data.insert(end, right);
            }
        }
    }

    fn truncate_sparse_to(&mut self, len: u64) {
        let affected: Vec<u64> = self
            .sparse_data
            .range(..)
            .filter_map(|(&extent_start, extent)| {
                let extent_end = extent_start.saturating_add(extent.len() as u64);
                if extent_start >= len || extent_end > len {
                    Some(extent_start)
                } else {
                    None
                }
            })
            .collect();

        for extent_start in affected {
            let Some(mut extent) = self.sparse_data.remove(&extent_start) else {
                continue;
            };
            if extent_start < len {
                extent.truncate_to((len - extent_start) as usize);
                if extent.len() > 0 {
                    self.sparse_data.insert(extent_start, extent);
                }
            }
        }
    }

    fn copy_sparse_to(&self, offset: u64, buf: &mut [u8]) {
        let end = offset.saturating_add(buf.len() as u64);
        for (&extent_start, extent) in self.sparse_data.range(..end) {
            let extent_end = extent_start.saturating_add(extent.len() as u64);
            if extent_end <= offset {
                continue;
            }
            extent.copy_to(extent_start, offset, buf);
        }
    }

    fn write_zero_range(&mut self, offset: u64, end: u64, inline_file_limit: usize) {
        if end <= inline_file_limit as u64 {
            let inline_end = end as usize;
            if self.data.len() < inline_end {
                self.data.resize(inline_end, 0);
            }
            let start = offset as usize;
            self.data[start..inline_end].fill(0);
            self.remove_sparse_range(offset, end);
            return;
        }
        if offset < self.data.len() as u64 {
            let start = offset as usize;
            let inline_end = end.min(self.data.len() as u64) as usize;
            self.data[start..inline_end].fill(0);
        }
        self.remove_sparse_range(offset, end);
    }

    fn append_sparse_tail(&mut self, offset: u64, buf: &[u8]) -> Option<usize> {
        let allocated_payload_len = self.allocated_payload_len();
        let (&extent_start, extent) = self.sparse_data.range_mut(..=offset).next_back()?;
        let extent_end = extent_start.saturating_add(extent.len() as u64);
        if extent_end != offset {
            return None;
        }
        extent.try_append(buf, allocated_payload_len)
    }

    fn write_sparse_data(&mut self, mut offset: u64, mut buf: &[u8]) -> bool {
        while !buf.is_empty() {
            if let Some(appended) = self.append_sparse_tail(offset, buf) {
                if appended == 0 {
                    return false;
                }
                offset += appended as u64;
                buf = &buf[appended..];
                continue;
            }

            let copy_len = buf.len().min(TMPFS_SPARSE_EXTENT_LIMIT);
            if let Some(extent) = TmpfsSparseExtent::repeated_byte(&buf[..copy_len]) {
                self.sparse_data.insert(offset, extent);
                offset += copy_len as u64;
                buf = &buf[copy_len..];
                continue;
            }
            if self.allocated_payload_len().saturating_add(copy_len) > TMPFS_ALLOCATED_PAYLOAD_LIMIT
            {
                return false;
            }
            let mut data = Vec::new();
            if data.try_reserve(copy_len).is_err() {
                return false;
            }
            data.extend_from_slice(&buf[..copy_len]);
            self.sparse_data
                .insert(offset, TmpfsSparseExtent::Bytes(data));
            offset += copy_len as u64;
            buf = &buf[copy_len..];
        }
        true
    }

    fn allocated_bytes_in_range(&self, start: u64, end: u64) -> u64 {
        if start >= end {
            return 0;
        }
        let inline_end = (self.data.len() as u64).min(end);
        let mut allocated = inline_end.saturating_sub(start);
        for (&extent_start, extent) in self.sparse_data.range(..end) {
            let extent_end = extent_start.saturating_add(extent.len() as u64);
            if extent_end <= start {
                continue;
            }
            allocated = allocated.saturating_add(extent_end.min(end) - extent_start.max(start));
        }
        allocated
    }

    fn write_allocated_growth(&self, start: u64, end: u64) -> u64 {
        end.saturating_sub(start)
            .saturating_sub(self.allocated_bytes_in_range(start, end))
    }
}

impl TmpFs {
    pub(super) fn new() -> Self {
        Self::new_with_statfs_magic(TMPFS_MAGIC)
    }

    pub(super) fn new_with_statfs_magic(statfs_magic: i64) -> Self {
        Self::new_with_statfs_magic_and_quota(statfs_magic, None)
    }

    pub(super) fn new_with_statfs_magic_and_quota(
        statfs_magic: i64,
        logical_quota_bytes: Option<u64>,
    ) -> Self {
        Self::new_with_statfs_magic_quota_and_synthetic_sync(
            statfs_magic,
            logical_quota_bytes,
            TMPFS_DEFAULT_INLINE_FILE_LIMIT,
            None,
        )
    }

    pub(super) fn new_ext_scratch(loop_id: usize, logical_quota_bytes: Option<u64>) -> Self {
        Self::new_with_statfs_magic_quota_and_synthetic_sync(
            EXT234_SUPER_MAGIC,
            logical_quota_bytes,
            EXT_SCRATCH_INLINE_FILE_LIMIT,
            Some(loop_id),
        )
    }

    fn new_with_statfs_magic_quota_and_synthetic_sync(
        statfs_magic: i64,
        logical_quota_bytes: Option<u64>,
        inline_file_limit: usize,
        synthetic_sync_loop_device: Option<usize>,
    ) -> Self {
        let mut inodes = BTreeMap::new();
        inodes.insert(
            ROOT_INO,
            TmpfsInode::new(FsNodeKind::Directory, S_IFDIR | 0o1777, ROOT_INO, 0),
        );
        Self {
            inodes,
            next_ino: ROOT_INO + 1,
            statfs_magic,
            logical_quota_bytes,
            inline_file_limit,
            synthetic_sync_loop_device,
        }
    }

    fn alloc_ino(&mut self) -> u32 {
        let ino = self.next_ino;
        self.next_ino += 1;
        ino
    }

    fn statx_supported_inode_flags(&self) -> u32 {
        if self.statfs_magic == EXT234_SUPER_MAGIC {
            FS_STATX_ATTR_FLAGS
        } else {
            FS_STATX_COMMON_ATTR_FLAGS
        }
    }

    fn allocated_logical_len(&self) -> u64 {
        self.inodes
            .values()
            .filter(|inode| inode.kind == FsNodeKind::RegularFile)
            .map(TmpfsInode::allocated_logical_len)
            .sum()
    }

    fn write_fits_quota(&self, ino: u32, offset: u64, len: usize) -> FsResult {
        let Some(quota) = self.logical_quota_bytes else {
            return Ok(());
        };
        let Some(end) = offset.checked_add(len as u64) else {
            return Err(FsError::InvalidInput);
        };
        let inode = self.inode(ino)?;
        if inode.kind != FsNodeKind::RegularFile {
            return Ok(());
        }
        let used = self.allocated_logical_len();
        let growth = inode.write_allocated_growth(offset, end);
        if used.saturating_add(growth) > quota {
            Err(FsError::NoSpace)
        } else {
            Ok(())
        }
    }

    fn quota_limited_write_len(&self, ino: u32, offset: u64, len: usize) -> usize {
        let Some(quota) = self.logical_quota_bytes else {
            return len;
        };
        let Ok(inode) = self.inode(ino) else {
            return 0;
        };
        if inode.kind != FsNodeKind::RegularFile {
            return 0;
        }
        let mut used = self.allocated_logical_len();
        let mut accepted = 0usize;
        while accepted < len {
            let Some(pos) = offset.checked_add(accepted as u64) else {
                break;
            };
            let next = len.min(accepted.saturating_add(4096));
            let Some(next_pos) = offset.checked_add(next as u64) else {
                break;
            };
            let growth = inode.write_allocated_growth(pos, next_pos);
            if used.saturating_add(growth) > quota {
                break;
            }
            used = used.saturating_add(growth);
            accepted = next;
        }
        accepted
    }

    fn inode(&self, ino: u32) -> FsResult<&TmpfsInode> {
        self.inodes.get(&ino).ok_or(FsError::NotFound)
    }

    fn inode_mut(&mut self, ino: u32) -> FsResult<&mut TmpfsInode> {
        self.inodes.get_mut(&ino).ok_or(FsError::NotFound)
    }

    fn ensure_dir(&self, ino: u32) -> FsResult<&TmpfsInode> {
        let inode = self.inode(ino)?;
        if inode.kind != FsNodeKind::Directory {
            return Err(FsError::NotDir);
        }
        Ok(inode)
    }

    fn create_node(
        &mut self,
        parent_ino: u32,
        name: &str,
        kind: FsNodeKind,
        mode: u32,
        rdev: u64,
    ) -> FsResult<u32> {
        if name.is_empty() || name == "." || name == ".." {
            return Err(FsError::InvalidInput);
        }
        {
            let parent = self.ensure_dir(parent_ino)?;
            if parent.children.contains_key(name) {
                return Err(FsError::AlreadyExists);
            }
        }

        let ino = self.alloc_ino();
        let mut inode = TmpfsInode::new(kind, mode, parent_ino, rdev);
        if kind == FsNodeKind::Directory {
            self.inode_mut(parent_ino)?.nlink += 1;
        }
        inode.touch();
        self.inodes.insert(ino, inode);
        let mark_parent_encrypted =
            self.statfs_magic == EXT234_SUPER_MAGIC && name == E4CRYPT_ENCRYPTED_MARKER;
        let parent = self.inode_mut(parent_ino)?;
        parent.children.insert(name.into(), ino);
        if mark_parent_encrypted {
            parent.flags |= FS_ENCRYPT_FL;
        }
        parent.touch();
        Ok(ino)
    }

    fn remove_child(&mut self, parent_ino: u32, name: &str) -> FsResult<u32> {
        let child_ino = {
            let parent = self.ensure_dir(parent_ino)?;
            *parent.children.get(name).ok_or(FsError::NotFound)?
        };
        if child_ino == ROOT_INO {
            return Err(FsError::Busy);
        }
        {
            let child = self.inode(child_ino)?;
            if child.kind == FsNodeKind::Directory && !child.children.is_empty() {
                return Err(FsError::NotEmpty);
            }
        }
        self.inode_mut(parent_ino)?.children.remove(name);
        self.inode_mut(parent_ino)?.touch();
        Ok(child_ino)
    }

    fn drop_inode_link(&mut self, ino: u32) {
        let Some((kind, parent_ino)) = self
            .inodes
            .get(&ino)
            .map(|inode| (inode.kind, inode.parent_ino))
        else {
            return;
        };
        if kind == FsNodeKind::Directory {
            // UNFINISHED: Linux keeps opened directories alive across unlink
            // until the last file reference is closed. This tmpfs currently
            // delays deletion only for non-directory inodes needed by
            // mkstemp/unlink/fstat style file workloads.
            if parent_ino != ino
                && let Some(parent) = self.inodes.get_mut(&parent_ino)
            {
                parent.nlink = parent.nlink.saturating_sub(1);
                parent.touch();
            }
            self.inodes.remove(&ino);
            return;
        }
        if let Some(inode) = self.inodes.get_mut(&ino) {
            inode.nlink = inode.nlink.saturating_sub(1);
            inode.touch();
            if inode.nlink == 0 {
                if inode.open_count == 0 {
                    self.inodes.remove(&ino);
                } else {
                    inode.pending_delete = true;
                }
            }
        }
    }

    fn dir_entries(&self, ino: u32) -> FsResult<Vec<RawDirEntry>> {
        let dir = self.ensure_dir(ino)?;
        let mut entries = Vec::new();
        entries.push(RawDirEntry {
            ino,
            name: ".".into(),
            dtype: DT_DIR,
        });
        entries.push(RawDirEntry {
            ino: dir.parent_ino,
            name: "..".into(),
            dtype: DT_DIR,
        });
        for (name, child_ino) in dir.children.iter() {
            let child = self.inode(*child_ino)?;
            let dtype = match child.kind {
                FsNodeKind::Directory => DT_DIR,
                FsNodeKind::RegularFile => DT_REG,
                FsNodeKind::Symlink => DT_LNK,
                FsNodeKind::Fifo => DT_FIFO,
                FsNodeKind::CharacterDevice => DT_CHR,
                FsNodeKind::BlockDevice => DT_BLK,
                FsNodeKind::Socket => DT_SOCK,
                FsNodeKind::Other => 0,
            };
            entries.push(RawDirEntry {
                ino: *child_ino,
                name: name.clone(),
                dtype,
            });
        }
        Ok(entries)
    }
}

impl FileSystemBackend for TmpFs {
    fn root_ino(&self) -> u32 {
        ROOT_INO
    }

    fn statfs(&mut self) -> super::vfs::FileSystemStat {
        let block_size = 4096;
        let (blocks, free_blocks) = if let Some(quota) = self.logical_quota_bytes {
            let blocks = quota.div_ceil(block_size);
            let used = self.allocated_logical_len().div_ceil(block_size);
            (blocks, blocks.saturating_sub(used))
        } else {
            (4096, 4096)
        };
        super::vfs::FileSystemStat {
            magic: self.statfs_magic,
            block_size,
            blocks,
            free_blocks,
            available_blocks: free_blocks,
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
    ) -> FsResult<(u32, FsNodeKind)> {
        let parent = self.ensure_dir(parent_ino)?;
        match component {
            "." => Ok((parent_ino, FsNodeKind::Directory)),
            ".." => Ok((parent.parent_ino, FsNodeKind::Directory)),
            _ => {
                let ino = *parent.children.get(component).ok_or(FsError::NotFound)?;
                Ok((ino, self.inode(ino)?.kind))
            }
        }
    }

    fn create_file(&mut self, parent_ino: u32, leaf_name: &str) -> FsResult<u32> {
        self.create_node(
            parent_ino,
            leaf_name,
            FsNodeKind::RegularFile,
            S_IFREG | 0o666,
            0,
        )
    }

    fn create_node(
        &mut self,
        parent_ino: u32,
        leaf_name: &str,
        kind: FsNodeKind,
        mode: u32,
        rdev: u64,
    ) -> FsResult<u32> {
        let file_type = match kind {
            FsNodeKind::RegularFile => S_IFREG,
            FsNodeKind::Fifo => S_IFIFO,
            FsNodeKind::CharacterDevice => S_IFCHR,
            FsNodeKind::BlockDevice => S_IFBLK,
            FsNodeKind::Socket => S_IFSOCK,
            _ => return Err(FsError::InvalidInput),
        };
        self.create_node(
            parent_ino,
            leaf_name,
            kind,
            file_type | (mode & 0o7777),
            rdev,
        )
    }

    fn create_dir(&mut self, parent_ino: u32, leaf_name: &str, mode: u32) -> FsResult<u32> {
        self.create_node(
            parent_ino,
            leaf_name,
            FsNodeKind::Directory,
            S_IFDIR | (mode & 0o7777),
            0,
        )
    }

    fn link(&mut self, parent_ino: u32, leaf_name: &str, child_ino: u32) -> FsResult {
        if leaf_name.is_empty() || leaf_name == "." || leaf_name == ".." {
            return Err(FsError::InvalidInput);
        }
        {
            let parent = self.ensure_dir(parent_ino)?;
            if parent.children.contains_key(leaf_name) {
                return Err(FsError::AlreadyExists);
            }
        }
        if self.inode(child_ino)?.kind == FsNodeKind::Directory {
            return Err(FsError::PermissionDenied);
        }
        self.inode_mut(child_ino)?.nlink += 1;
        self.inode_mut(child_ino)?.touch();
        let parent = self.inode_mut(parent_ino)?;
        parent.children.insert(leaf_name.into(), child_ino);
        parent.touch();
        Ok(())
    }

    fn symlink(&mut self, parent_ino: u32, leaf_name: &str, target: &[u8]) -> FsResult {
        let ino = self.create_node(
            parent_ino,
            leaf_name,
            FsNodeKind::Symlink,
            S_IFLNK | 0o777,
            0,
        )?;
        let inode = self.inode_mut(ino)?;
        inode.data.extend_from_slice(target);
        inode.size = target.len() as u64;
        inode.touch();
        Ok(())
    }

    fn unlink(&mut self, parent_ino: u32, leaf_name: &str) -> FsResult {
        if leaf_name.is_empty() || leaf_name == "." || leaf_name == ".." {
            return Err(FsError::InvalidInput);
        }
        let child_ino = self.remove_child(parent_ino, leaf_name)?;
        self.drop_inode_link(child_ino);
        Ok(())
    }

    fn rename(&mut self, src_dir: u32, src_name: &str, dst_dir: u32, dst_name: &str) -> FsResult {
        if src_name.is_empty() || dst_name.is_empty() || src_name == "." || src_name == ".." {
            return Err(FsError::InvalidInput);
        }
        if dst_name == "." || dst_name == ".." {
            return Err(FsError::InvalidInput);
        }

        let src_ino = {
            let src_parent = self.ensure_dir(src_dir)?;
            *src_parent.children.get(src_name).ok_or(FsError::NotFound)?
        };
        {
            let dst_parent = self.ensure_dir(dst_dir)?;
            if let Some(existing_ino) = dst_parent.children.get(dst_name).copied()
                && existing_ino != src_ino
            {
                let existing = self.inode(existing_ino)?;
                if existing.kind == FsNodeKind::Directory && !existing.children.is_empty() {
                    return Err(FsError::NotEmpty);
                }
            }
        }

        let replaced = self.inode_mut(dst_dir)?.children.remove(dst_name);
        if let Some(replaced_ino) = replaced
            && replaced_ino != src_ino
        {
            self.drop_inode_link(replaced_ino);
        }
        self.inode_mut(src_dir)?.children.remove(src_name);
        self.inode_mut(dst_dir)?
            .children
            .insert(dst_name.into(), src_ino);
        if self.inode(src_ino)?.kind == FsNodeKind::Directory {
            self.inode_mut(src_ino)?.parent_ino = dst_dir;
        }
        self.inode_mut(src_ino)?.touch();
        self.inode_mut(src_dir)?.touch();
        self.inode_mut(dst_dir)?.touch();
        Ok(())
    }

    fn exchange(&mut self, src_dir: u32, src_name: &str, dst_dir: u32, dst_name: &str) -> FsResult {
        if src_name.is_empty()
            || dst_name.is_empty()
            || src_name == "."
            || src_name == ".."
            || dst_name == "."
            || dst_name == ".."
        {
            return Err(FsError::InvalidInput);
        }

        let src_ino = {
            let src_parent = self.ensure_dir(src_dir)?;
            *src_parent.children.get(src_name).ok_or(FsError::NotFound)?
        };
        let dst_ino = {
            let dst_parent = self.ensure_dir(dst_dir)?;
            *dst_parent.children.get(dst_name).ok_or(FsError::NotFound)?
        };
        if src_ino == dst_ino {
            return Ok(());
        }

        let src_kind = self.inode(src_ino)?.kind;
        let dst_kind = self.inode(dst_ino)?.kind;
        if src_dir == dst_dir {
            let parent = self.inode_mut(src_dir)?;
            parent.children.insert(src_name.into(), dst_ino);
            parent.children.insert(dst_name.into(), src_ino);
            parent.touch();
        } else {
            self.inode_mut(src_dir)?
                .children
                .insert(src_name.into(), dst_ino);
            self.inode_mut(dst_dir)?
                .children
                .insert(dst_name.into(), src_ino);
            if src_kind == FsNodeKind::Directory && dst_kind != FsNodeKind::Directory {
                self.inode_mut(src_dir)?.nlink = self.inode(src_dir)?.nlink.saturating_sub(1);
                self.inode_mut(dst_dir)?.nlink += 1;
            } else if src_kind != FsNodeKind::Directory && dst_kind == FsNodeKind::Directory {
                self.inode_mut(src_dir)?.nlink += 1;
                self.inode_mut(dst_dir)?.nlink = self.inode(dst_dir)?.nlink.saturating_sub(1);
            }
            self.inode_mut(src_dir)?.touch();
            self.inode_mut(dst_dir)?.touch();
        }

        if src_kind == FsNodeKind::Directory {
            let inode = self.inode_mut(src_ino)?;
            inode.parent_ino = dst_dir;
            inode.touch();
        }
        if dst_kind == FsNodeKind::Directory {
            let inode = self.inode_mut(dst_ino)?;
            inode.parent_ino = src_dir;
            inode.touch();
        }
        Ok(())
    }

    fn check_write_at(&mut self, ino: u32, offset: u64, len: usize) -> FsResult {
        self.write_fits_quota(ino, offset, len)
    }

    fn check_set_len(&mut self, ino: u32, _len: u64) -> FsResult {
        self.inode(ino).map(|_| ())
    }

    fn set_len(&mut self, ino: u32, len: u64) -> FsResult {
        let inline_file_limit = self.inline_file_limit;
        let inode = self.inode_mut(ino)?;
        if inode.kind == FsNodeKind::Directory {
            return Err(FsError::IsDir);
        }
        if len == 0 {
            inode.clear_payload();
        } else if len < inode.size {
            inode.truncate_sparse_to(len);
            if len as usize <= inline_file_limit {
                inode.data.resize(len as usize, 0);
            } else if inode.data.len() as u64 > len {
                inode.data.truncate(len as usize);
            }
        } else if inode.data.len() as u64 > len {
            inode.data.truncate(len as usize);
        }
        if inode.allocated_payload_len() > TMPFS_ALLOCATED_PAYLOAD_LIMIT {
            return Err(FsError::NoSpace);
        }
        inode.size = len;
        inode.touch();
        Ok(())
    }

    fn sync(&mut self, _ino: u32, _data_only: bool) -> FsResult {
        if let Some(loop_id) = self.synthetic_sync_loop_device {
            let _ = super::devfs::loop_device_note_synthetic_write(loop_id, EXT_SCRATCH_SYNC_BYTES);
        }
        Ok(())
    }

    fn retain_inode(&mut self, ino: u32) -> FsResult {
        let inode = self.inode_mut(ino)?;
        inode.open_count += 1;
        Ok(())
    }

    fn release_inode(&mut self, ino: u32) -> FsResult {
        let should_remove = {
            let inode = self.inode_mut(ino)?;
            inode.open_count = inode.open_count.saturating_sub(1);
            inode.open_count == 0 && inode.nlink == 0 && inode.pending_delete
        };
        if should_remove {
            self.inodes.remove(&ino);
        }
        Ok(())
    }

    fn stat(&mut self, ino: u32) -> FsResult<FileStat> {
        let inode = self.inode(ino)?;
        let size = match inode.kind {
            FsNodeKind::Directory => inode.children.len() as u64,
            _ => inode.size,
        };
        let blocks = match inode.kind {
            FsNodeKind::Directory => size.div_ceil(512),
            FsNodeKind::Symlink => 0,
            _ => (inode.allocated_payload_len() as u64).div_ceil(512),
        };
        Ok(FileStat {
            ino: ino as u64,
            mode: inode.mode,
            nlink: inode.nlink,
            uid: inode.uid,
            gid: inode.gid,
            rdev: inode.rdev,
            inode_flags: inode.flags,
            inode_flags_supported: self.statx_supported_inode_flags(),
            size,
            blocks,
            blksize: super::DEFAULT_BLOCK_SIZE,
            atime_sec: inode.atime.sec,
            atime_nsec: inode.atime.nsec,
            btime_sec: inode.btime.sec,
            btime_nsec: inode.btime.nsec,
            mtime_sec: inode.mtime.sec,
            mtime_nsec: inode.mtime.nsec,
            ctime_sec: inode.ctime.sec,
            ctime_nsec: inode.ctime.nsec,
            ..FileStat::default()
        })
    }

    fn set_times(
        &mut self,
        ino: u32,
        atime: Option<FileTimestamp>,
        mtime: Option<FileTimestamp>,
        ctime: FileTimestamp,
    ) -> FsResult {
        let inode = self.inode_mut(ino)?;
        if let Some(atime) = atime {
            inode.atime = atime;
        }
        if let Some(mtime) = mtime {
            inode.mtime = mtime;
        }
        inode.ctime = ctime;
        Ok(())
    }

    fn set_mode(&mut self, ino: u32, mode: u32) -> FsResult {
        let inode = self.inode_mut(ino)?;
        inode.mode = (inode.mode & !0o7777) | (mode & 0o7777);
        inode.ctime = FileTimestamp::now();
        Ok(())
    }

    fn set_owner(&mut self, ino: u32, uid: Option<u32>, gid: Option<u32>) -> FsResult {
        let inode = self.inode_mut(ino)?;
        if let Some(uid) = uid {
            inode.uid = uid;
        }
        if let Some(gid) = gid {
            inode.gid = gid;
        }
        inode.ctime = FileTimestamp::now();
        Ok(())
    }

    fn inode_flags(&mut self, ino: u32) -> FsResult<u32> {
        Ok(self.inode(ino)?.flags)
    }

    fn set_inode_flags(&mut self, ino: u32, flags: u32) -> FsResult {
        let inode = self.inode_mut(ino)?;
        inode.flags = flags;
        inode.ctime = FileTimestamp::now();
        Ok(())
    }

    fn readlink(&mut self, ino: u32, buf: &mut [u8]) -> FsResult<usize> {
        let inode = self.inode(ino)?;
        if inode.kind != FsNodeKind::Symlink {
            return Err(FsError::InvalidInput);
        }
        let len = buf.len().min(inode.size as usize).min(inode.data.len());
        buf[..len].copy_from_slice(&inode.data[..len]);
        Ok(len)
    }

    fn read_at(&mut self, ino: u32, buf: &mut [u8], offset: u64) -> usize {
        let Ok(inode) = self.inode_mut(ino) else {
            return 0;
        };
        if inode.kind == FsNodeKind::Directory {
            return 0;
        }
        if !buf.is_empty() {
            inode.atime = FileTimestamp::now();
        }
        if offset >= inode.size {
            return 0;
        }
        let len = buf.len().min((inode.size - offset) as usize);
        let out = &mut buf[..len];
        out.fill(0);
        if offset < inode.data.len() as u64 {
            let start = offset as usize;
            let inline_len = out.len().min(inode.data.len() - start);
            out[..inline_len].copy_from_slice(&inode.data[start..start + inline_len]);
        }
        inode.copy_sparse_to(offset, out);
        len
    }

    fn write_at(&mut self, ino: u32, buf: &[u8], offset: u64) -> usize {
        let write_len = self.quota_limited_write_len(ino, offset, buf.len());
        if write_len == 0 && !buf.is_empty() {
            return 0;
        }
        let buf = &buf[..write_len];
        let Some(end) = offset.checked_add(buf.len() as u64) else {
            return 0;
        };
        let inline_file_limit = self.inline_file_limit;
        let Ok(inode) = self.inode_mut(ino) else {
            return 0;
        };
        if inode.kind != FsNodeKind::RegularFile {
            return 0;
        }
        if buf.iter().all(|byte| *byte == 0) {
            if offset >= inline_file_limit as u64 {
                // CONTEXT: user writes can be split at page boundaries. Keeping a
                // zero tail contiguous lets repeated-page payloads compress.
                if let Some(appended) = inode.append_sparse_tail(offset, buf) {
                    if appended == 0 {
                        return 0;
                    }
                    if appended < buf.len() {
                        let zero_start = offset + appended as u64;
                        inode.write_zero_range(zero_start, end, inline_file_limit);
                    }
                    if end > inode.size {
                        inode.size = end;
                    }
                    inode.touch();
                    return buf.len();
                }
            }
            inode.write_zero_range(offset, end, inline_file_limit);
            if end > inode.size {
                inode.size = end;
            }
            inode.touch();
            return buf.len();
        }

        let mut sparse_offset = offset;
        let mut sparse_buf = buf;
        if offset < inline_file_limit as u64 {
            let start = offset as usize;
            let inline_end = end.min(inline_file_limit as u64) as usize;
            if inline_end > inode.data.len() {
                let extra = inline_end - inode.data.len();
                if inode.allocated_payload_len().saturating_add(extra)
                    > TMPFS_ALLOCATED_PAYLOAD_LIMIT
                    || inode.data.try_reserve(extra).is_err()
                {
                    return 0;
                }
                inode.data.resize(inline_end, 0);
            }
            let inline_len = inline_end - start;
            inode.data[start..inline_end].copy_from_slice(&buf[..inline_len]);
            inode.remove_sparse_range(offset, inline_end as u64);
            if inline_end as u64 == end {
                if end > inode.size {
                    inode.size = end;
                }
                inode.touch();
                return buf.len();
            }
            sparse_offset = inline_end as u64;
            sparse_buf = &buf[inline_len..];
        }

        inode.remove_sparse_range(sparse_offset, end);
        if inode
            .allocated_payload_len()
            .saturating_add(sparse_buf.len())
            > TMPFS_ALLOCATED_PAYLOAD_LIMIT
        {
            return 0;
        }
        if !inode.write_sparse_data(sparse_offset, sparse_buf) {
            return 0;
        }
        if end > inode.size {
            inode.size = end;
        }
        inode.touch();
        buf.len()
    }

    fn read_dirent64(&mut self, ino: u32, offset: u64, buf: &mut [u8]) -> FsResult<(usize, u64)> {
        write_dir_entries(&self.dir_entries(ino)?, offset, buf)
    }

    fn list_root_names(&mut self) -> Vec<String> {
        self.inodes
            .get(&ROOT_INO)
            .map(|root| root.children.keys().cloned().collect())
            .unwrap_or_default()
    }
}
