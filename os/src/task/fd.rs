use crate::fs::{File, OpenFlags};
use alloc::string::String;
use alloc::sync::Arc;
use bitflags::bitflags;

pub const FD_LIMIT: usize = 1024;

bitflags! {
    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
    pub struct FdFlags: u32 {
        const CLOEXEC = 1;
    }
}

#[derive(Clone)]
pub struct FdTableEntry {
    file: Arc<dyn File + Send + Sync>,
    fd_flags: FdFlags,
    // UNFINISHED: This is a pathname snapshot for getcwd-compatible fchdir;
    // Linux keeps directory objects alive across rename/unlink and reconstructs
    // cwd differently.
    dir_path: Option<String>,
}

impl FdTableEntry {
    pub fn from_file(file: Arc<dyn File + Send + Sync>, open_flags: OpenFlags) -> Self {
        Self::from_file_with_dir_path(file, open_flags, None)
    }

    pub fn from_file_with_dir_path(
        file: Arc<dyn File + Send + Sync>,
        open_flags: OpenFlags,
        dir_path: Option<String>,
    ) -> Self {
        let fd_flags = if open_flags.contains(OpenFlags::CLOEXEC) {
            FdFlags::CLOEXEC
        } else {
            FdFlags::empty()
        };
        file.set_status_flags(OpenFlags::file_status_flags(open_flags));
        Self {
            file,
            fd_flags,
            dir_path,
        }
    }

    pub fn duplicate(&self, fd_flags: FdFlags) -> Self {
        Self {
            file: Arc::clone(&self.file),
            fd_flags,
            dir_path: self.dir_path.clone(),
        }
    }

    pub fn file(&self) -> Arc<dyn File + Send + Sync> {
        Arc::clone(&self.file)
    }

    pub fn vfs_mount_id(&self) -> Option<crate::fs::MountId> {
        self.file.vfs_mount_id()
    }

    pub fn dir_path(&self) -> Option<&str> {
        self.dir_path.as_deref()
    }

    pub fn fd_flags(&self) -> FdFlags {
        self.fd_flags
    }

    pub fn set_fd_flags(&mut self, flags: FdFlags) {
        self.fd_flags = flags;
    }

    pub fn status_flags(&self) -> OpenFlags {
        self.file.status_flags()
    }

    pub fn set_status_flags(&self, flags: OpenFlags) {
        self.file.set_status_flags(flags);
    }

    pub fn close_on_exec(&self) -> bool {
        self.fd_flags.contains(FdFlags::CLOEXEC)
    }
}
