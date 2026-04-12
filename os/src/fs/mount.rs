use super::ext4::Ext4Mount;
use crate::drivers::{BLOCK_DEVICE, block::block_device};
use crate::sync::UPIntrFreeCell;
use alloc::string::String;
use alloc::vec::Vec;
use lazy_static::*;
use log::{info, warn};

pub(super) const TESTDISK_MOUNT_NAME: &str = "testdisk";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct MountId(pub(super) usize);

const ROOT_MOUNT_ID: MountId = MountId(0);
const TESTDISK_MOUNT_ID: MountId = MountId(1);

lazy_static! {
    static ref ROOT_MOUNT: UPIntrFreeCell<Option<Ext4Mount>> = unsafe { UPIntrFreeCell::new(None) };
    static ref TESTDISK_MOUNT: UPIntrFreeCell<Option<Ext4Mount>> =
        unsafe { UPIntrFreeCell::new(None) };
    static ref MOUNTS_INITIALIZED: UPIntrFreeCell<bool> = unsafe { UPIntrFreeCell::new(false) };
}

pub fn init_mounts() {
    let already_initialized = MOUNTS_INITIALIZED.exclusive_session(|initialized| {
        if *initialized {
            true
        } else {
            *initialized = true;
            false
        }
    });
    if already_initialized {
        return;
    }

    ROOT_MOUNT.exclusive_session(|slot| {
        *slot = Some(
            Ext4Mount::open(BLOCK_DEVICE.clone()).expect("failed to mount ext4 root filesystem"),
        );
    });

    TESTDISK_MOUNT.exclusive_session(|slot| {
        *slot = block_device(1).and_then(|device| match Ext4Mount::open(device) {
            Ok(mount) => Some(mount),
            Err(err) => {
                warn!(
                    "failed to mount dev-mode test disk on /{}: {:?}",
                    TESTDISK_MOUNT_NAME, err
                );
                None
            }
        });
    });
}

pub(super) fn resolve_mount(path: &str) -> Option<(MountId, &str)> {
    let path = path.trim_start_matches('/');
    if path.is_empty() {
        return Some((ROOT_MOUNT_ID, ""));
    }
    if path == TESTDISK_MOUNT_NAME {
        return Some((TESTDISK_MOUNT_ID, ""));
    }
    if let Some(rest) = path.strip_prefix(TESTDISK_MOUNT_NAME) {
        if let Some(rest) = rest.strip_prefix('/') {
            return Some((TESTDISK_MOUNT_ID, rest));
        }
    }
    Some((ROOT_MOUNT_ID, path))
}

pub(super) fn with_mount<V>(mount_id: MountId, f: impl FnOnce(&mut Ext4Mount) -> V) -> Option<V> {
    match mount_id {
        ROOT_MOUNT_ID => ROOT_MOUNT.exclusive_session(|slot| slot.as_mut().map(f)),
        TESTDISK_MOUNT_ID => TESTDISK_MOUNT.exclusive_session(|slot| slot.as_mut().map(f)),
        _ => None,
    }
}

pub(super) fn is_read_only(mount_id: MountId) -> bool {
    mount_id == TESTDISK_MOUNT_ID
}

pub fn mount_status_log() {
    let mounted = TESTDISK_MOUNT.exclusive_session(|slot| slot.is_some());
    if mounted {
        info!(
            "mounted dev-mode test disk at /{} from BLOCK_DEVICES[1]",
            TESTDISK_MOUNT_NAME
        );
    } else {
        info!(
            "dev-mode test disk is unavailable; /{} is not mounted",
            TESTDISK_MOUNT_NAME
        );
    }
}

pub fn list_root_apps() -> Vec<String> {
    with_mount(ROOT_MOUNT_ID, |mount| mount.list_root_names()).unwrap_or_default()
}
