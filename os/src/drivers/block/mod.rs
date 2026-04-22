mod virtio_blk;

pub use virtio_blk::VirtIOBlock;

use crate::board::BlockDeviceImpl;
use alloc::sync::Arc;
use alloc::vec::Vec;
use lazy_static::*;
use log::info;

lazy_static! {
    pub static ref BLOCK_DEVICES: Vec<Arc<BlockDeviceImpl>> = crate::board::block_devices()
        .iter()
        .enumerate()
        .map(|(index, device)| {
            let block_device = Arc::new(BlockDeviceImpl::new(*device));
            info!(
                "block device[{}]: base={:#x}, irq={}, sectors={}",
                index,
                block_device.base_addr(),
                block_device.irq(),
                block_device.num_blocks(),
            );
            block_device
        })
        .collect();
    pub static ref BLOCK_DEVICE: Arc<BlockDeviceImpl> = BLOCK_DEVICES
        .first()
        .expect("DTB is missing a block device")
        .clone();
}

#[allow(dead_code)]
pub fn block_device(index: usize) -> Option<Arc<BlockDeviceImpl>> {
    BLOCK_DEVICES.get(index).cloned()
}

#[allow(dead_code)]
pub fn block_count() -> usize {
    BLOCK_DEVICES.len()
}

pub fn handle_irq(irq: usize) -> bool {
    if let Some(device) = BLOCK_DEVICES.iter().find(|device| device.irq() == irq) {
        device.handle_irq();
        true
    } else {
        false
    }
}

#[allow(unused)]
pub fn block_device_test() {
    let block_device = BLOCK_DEVICE.clone();
    let mut write_buffer = [0u8; 512];
    let mut read_buffer = [0u8; 512];
    for i in 0..512 {
        for byte in write_buffer.iter_mut() {
            *byte = i as u8;
        }
        block_device.write_block(i as usize, &write_buffer);
        block_device.read_block(i as usize, &mut read_buffer);
        assert_eq!(write_buffer, read_buffer);
    }
    println!("block device test passed!");
}
