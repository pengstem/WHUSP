use crate::fs::{
    MountError, lookup_mount_target_dir_at, mount_block_device_at, mount_fat_device_at, unmount_at,
};
use crate::task::{current_process, current_user_token};

use super::super::errno::{SysError, SysResult};
use super::user_ptr::{PATH_MAX, read_user_c_string};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct VirtioBlockSource {
    device_index: usize,
    partition_index: Option<usize>,
}

fn parse_virtio_block_source(source: &str) -> SysResult<VirtioBlockSource> {
    let Some(suffix) = source.strip_prefix("/dev/vd") else {
        return Err(SysError::ENODEV);
    };
    let bytes = suffix.as_bytes();
    if bytes.len() == 1 && bytes[0].is_ascii_lowercase() {
        return Ok(VirtioBlockSource {
            device_index: (bytes[0] - b'a') as usize,
            partition_index: None,
        });
    }
    if bytes.len() > 1 && bytes[0].is_ascii_lowercase() && bytes[1..].iter().all(u8::is_ascii_digit)
    {
        let partition_index = source[8..].parse::<usize>().map_err(|_| SysError::ENODEV)?;
        return Ok(VirtioBlockSource {
            device_index: (bytes[0] - b'a') as usize,
            partition_index: Some(partition_index),
        });
    }
    Err(SysError::ENODEV)
}

fn mount_error_to_errno(error: MountError) -> SysError {
    match error {
        MountError::SourceMissing => SysError::ENODEV,
        MountError::InvalidFilesystem => SysError::EINVAL,
        MountError::InvalidTarget => SysError::ENOENT,
        MountError::TargetBusy | MountError::StaticRoot => SysError::EBUSY,
        MountError::TargetNotMounted => SysError::EINVAL,
    }
}

pub fn sys_mount(
    source: *const u8,
    target: *const u8,
    fstype: *const u8,
    flags: usize,
    data: *const u8,
) -> SysResult {
    if flags != 0 {
        // UNFINISHED: MS_BIND, MS_REMOUNT, MS_MOVE, propagation flags, and
        // per-mount access-time flags are not implemented yet.
        return Err(SysError::EINVAL);
    }
    if !data.is_null() {
        // UNFINISHED: Filesystem-specific mount data is ignored by this EXT4-only
        // mount path, so reject non-null data instead of silently misapplying it.
        return Err(SysError::EINVAL);
    }

    let token = current_user_token();
    let source = read_user_c_string(token, source, PATH_MAX)?;
    let target = read_user_c_string(token, target, PATH_MAX)?;
    let fstype = read_user_c_string(token, fstype, PATH_MAX)?;
    let block_source = parse_virtio_block_source(source.as_str())?;
    let process = current_process();
    let target_dir = lookup_mount_target_dir_at(process.working_dir(), target.as_str())?;
    match fstype.as_str() {
        "ext4" => {
            if block_source.partition_index.is_some() {
                // UNFINISHED: ext4 partition mounts such as /dev/vda1 are not
                // supported yet; the existing ext4 path still mounts whole
                // VirtIO block devices only.
                return Err(SysError::ENOTBLK);
            }
            mount_block_device_at(target_dir, block_source.device_index)
                .map_err(mount_error_to_errno)?;
        }
        "vfat" | "fat32" => {
            mount_fat_device_at(
                target_dir,
                block_source.device_index,
                block_source.partition_index,
            )
            .map_err(mount_error_to_errno)?;
        }
        _ => return Err(SysError::ENODEV),
    }
    Ok(0)
}

pub fn sys_umount2(target: *const u8, flags: i32) -> SysResult {
    if flags != 0 {
        // UNFINISHED: MNT_FORCE, MNT_DETACH, MNT_EXPIRE, and UMOUNT_NOFOLLOW
        // are not implemented yet.
        return Err(SysError::EINVAL);
    }

    let token = current_user_token();
    let target = read_user_c_string(token, target, PATH_MAX)?;
    let process = current_process();
    let target_dir = lookup_mount_target_dir_at(process.working_dir(), target.as_str())?;
    unmount_at(target_dir).map_err(mount_error_to_errno)?;
    Ok(0)
}
