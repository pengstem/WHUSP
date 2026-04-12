use super::ext4::Ext4Mount;
use crate::drivers::{BLOCK_DEVICE, block::block_device};
use crate::sync::UPIntrFreeCell;
use alloc::string::String;
use alloc::vec::Vec;
use lazy_static::*;
use log::{info, warn};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct MountId(pub(super) usize);

const PRIMARY_MOUNT_ID: MountId = MountId(0);
const AUX_MOUNT_ID: MountId = MountId(1);

lazy_static! {
    static ref PRIMARY_MOUNT: UPIntrFreeCell<Option<Ext4Mount>> =
        unsafe { UPIntrFreeCell::new(None) };
    static ref AUX_MOUNT: UPIntrFreeCell<Option<Ext4Mount>> = unsafe { UPIntrFreeCell::new(None) };
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

    PRIMARY_MOUNT.exclusive_session(|slot| {
        *slot = Some(
            Ext4Mount::open(BLOCK_DEVICE.clone()).expect("failed to mount primary ext4 filesystem"),
        );
    });

    AUX_MOUNT.exclusive_session(|slot| {
        *slot = block_device(1).and_then(|device| match Ext4Mount::open(device) {
            Ok(mount) => Some(mount),
            Err(err) => {
                warn!(
                    "failed to mount auxiliary ext4 disk on BLOCK_DEVICES[1]: {:?}",
                    err
                );
                None
            }
        });
    });
}

pub(super) fn resolve_primary_mount(path: &str) -> Option<(MountId, &str)> {
    let path = path.trim_start_matches('/');
    Some((PRIMARY_MOUNT_ID, path))
}

pub(super) fn with_mount<V>(mount_id: MountId, f: impl FnOnce(&mut Ext4Mount) -> V) -> Option<V> {
    match mount_id {
        PRIMARY_MOUNT_ID => PRIMARY_MOUNT.exclusive_session(|slot| slot.as_mut().map(f)),
        AUX_MOUNT_ID => AUX_MOUNT.exclusive_session(|slot| slot.as_mut().map(f)),
        _ => None,
    }
}

pub(super) fn with_primary_mount<V>(f: impl FnOnce(&mut Ext4Mount) -> V) -> Option<V> {
    PRIMARY_MOUNT.exclusive_session(|slot| slot.as_mut().map(f))
}

pub(super) fn primary_mount_id() -> MountId {
    PRIMARY_MOUNT_ID
}

pub(super) fn aux_mount_id() -> MountId {
    AUX_MOUNT_ID
}

pub(super) fn has_aux_mount() -> bool {
    AUX_MOUNT.exclusive_session(|slot| slot.is_some())
}

pub(super) fn is_aux_mount(mount_id: MountId) -> bool {
    mount_id == AUX_MOUNT_ID
}

pub fn mount_status_log() {
    info!("primary filesystem mounted from BLOCK_DEVICES[0]");
    if has_aux_mount() {
        info!("auxiliary filesystem mounted from BLOCK_DEVICES[1] for bootstrap lookup");
    } else {
        info!("auxiliary filesystem is unavailable; bare-name fallback is primary-only");
    }
}

pub fn list_root_apps() -> Vec<String> {
    with_primary_mount(|mount| mount.list_root_names()).unwrap_or_default()
}
