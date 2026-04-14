use super::ext4::Ext4Mount;
use crate::drivers::block::BLOCK_DEVICES;
use crate::sync::UPIntrFreeCell;
use alloc::string::String;
use alloc::vec::Vec;
use lazy_static::*;
use log::{info, warn};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct MountId(pub(super) usize);

lazy_static! {
    static ref MOUNTS: Vec<UPIntrFreeCell<Option<Ext4Mount>>> = BLOCK_DEVICES
        .iter()
        .map(|_| unsafe { UPIntrFreeCell::new(None) })
        .collect();
    static ref MOUNTS_INITIALIZED: UPIntrFreeCell<bool> = unsafe { UPIntrFreeCell::new(false) };
}

pub fn init_mounts() {
    // TODO: a little bit too much ...
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

    for (index, device) in BLOCK_DEVICES.iter().enumerate() {
        let mount = if index == 0 {
            Some(Ext4Mount::open(device.clone()).expect("failed to mount primary ext4 filesystem"))
        } else {
            match Ext4Mount::open(device.clone()) {
                Ok(mount) => Some(mount),
                Err(err) => {
                    warn!("failed to mount filesystem on BLOCK_DEVICES[{index}]: {err:?}");
                    None
                }
            }
        };
        MOUNTS[index].exclusive_session(|slot| *slot = mount);
    }
}

pub(super) fn with_mount<V>(mount_id: MountId, f: impl FnOnce(&mut Ext4Mount) -> V) -> Option<V> {
    MOUNTS
        .get(mount_id.0)
        .and_then(|slot| slot.exclusive_session(|mount| mount.as_mut().map(f)))
}

pub(super) fn mount_exists(mount_id: MountId) -> bool {
    MOUNTS
        .get(mount_id.0)
        .is_some_and(|slot| slot.exclusive_session(|mount| mount.is_some()))
}

// TODO: maybe we could skip this function
pub(super) fn primary_mount_id() -> MountId {
    MountId(0)
}

pub fn mount_status_log() {
    info!("filesystem mounted from BLOCK_DEVICES[0] at /");
    for index in 1..MOUNTS.len() {
        if mount_exists(MountId(index)) {
            info!("filesystem mounted from BLOCK_DEVICES[{index}] at /x{index}");
        } else {
            info!("filesystem on BLOCK_DEVICES[{index}] is unavailable at /x{index}",);
        }
    }
}

pub fn list_root_apps() -> Vec<String> {
    with_mount(primary_mount_id(), |mount| mount.list_root_names()).unwrap_or_default()
}
