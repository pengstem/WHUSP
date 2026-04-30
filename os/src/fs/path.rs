use super::mount::{MountId, primary_mount_id};
use lwext4_rust::ffi::EXT4_ROOT_INO;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct WorkingDir {
    mount_id: MountId,
    ino: u32,
}

impl WorkingDir {
    pub(crate) fn root() -> Self {
        Self {
            mount_id: primary_mount_id(),
            ino: EXT4_ROOT_INO,
        }
    }

    pub(crate) fn new(mount_id: MountId, ino: u32) -> Self {
        Self { mount_id, ino }
    }

    pub(crate) fn mount_id(self) -> MountId {
        self.mount_id
    }

    pub(crate) fn ino(self) -> u32 {
        self.ino
    }
}

pub(crate) fn normalize_path(cwd_path: &str, path: &str) -> Option<alloc::string::String> {
    let mut segments = alloc::vec::Vec::new();
    if path.starts_with('/') {
        for segment in path.split('/') {
            if segment.is_empty() || segment == "." {
                continue;
            }
            if segment == ".." {
                segments.pop();
            } else {
                segments.push(segment);
            }
        }
    } else {
        for segment in cwd_path.split('/') {
            if segment.is_empty() {
                continue;
            }
            segments.push(segment);
        }
        for segment in path.split('/') {
            if segment.is_empty() || segment == "." {
                continue;
            }
            if segment == ".." {
                segments.pop();
            } else {
                segments.push(segment);
            }
        }
    }

    if segments.is_empty() {
        Some("/".into())
    } else {
        Some(alloc::format!("/{}", segments.join("/")))
    }
}
