use alloc::string::String;

use crate::fs::{MountError, lookup_mount_target_dir_at, mount_block_device_at, unmount_at};
use crate::task::{current_process, current_user_token};

use super::super::errno::{SysError, SysResult};
use super::user_ptr::{UserBufferAccess, translated_byte_buffer_checked};

const PATH_MAX: usize = 4096;

fn read_user_c_string(token: usize, ptr: *const u8, max_len: usize) -> SysResult<String> {
    if ptr.is_null() {
        return Err(SysError::EFAULT);
    }

    let mut string = String::new();
    for offset in 0..max_len {
        let addr = (ptr as usize).checked_add(offset).ok_or(SysError::EFAULT)?;
        let buffers =
            translated_byte_buffer_checked(token, addr as *const u8, 1, UserBufferAccess::Read)?;
        let byte = buffers
            .first()
            .and_then(|buffer| buffer.first())
            .copied()
            .ok_or(SysError::EFAULT)?;
        if byte == 0 {
            return Ok(string);
        }
        string.push(byte as char);
    }
    Err(SysError::ENAMETOOLONG)
}

fn parse_virtio_block_source(source: &str) -> SysResult<usize> {
    let Some(suffix) = source.strip_prefix("/dev/vd") else {
        return Err(SysError::ENODEV);
    };
    let bytes = suffix.as_bytes();
    if bytes.len() == 1 && bytes[0].is_ascii_lowercase() {
        return Ok((bytes[0] - b'a') as usize);
    }
    if bytes.len() > 1 && bytes[0].is_ascii_lowercase() && bytes[1..].iter().all(u8::is_ascii_digit)
    {
        // UNFINISHED: Linux mounts can target partitions such as /dev/vda2,
        // but this kernel has no partition parser or block-device inode layer yet.
        return Err(SysError::ENOTBLK);
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
    if fstype != "ext4" {
        return Err(SysError::ENODEV);
    }

    let device_index = parse_virtio_block_source(source.as_str())?;
    let process = current_process();
    let target_dir = lookup_mount_target_dir_at(process.working_dir(), target.as_str())?;
    mount_block_device_at(target_dir, device_index).map_err(mount_error_to_errno)?;
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
