use crate::DEV_NON_BLOCKING_ACCESS;
use crate::board::{BlockDeviceImpl, IrqDevice};
use crate::drivers::virtio::{VirtioHal, VirtioTransport, mmio_transport};
use crate::sync::{Condvar, UPIntrFreeCell};
use crate::task::schedule;
use alloc::collections::BTreeMap;
use alloc::sync::Arc;
use alloc::vec::Vec;
use lazy_static::*;
use log::info;
use virtio_drivers::device::blk::{BlkReq, BlkResp, VirtIOBlk};

pub struct VirtIOBlock {
    virtio_blk: UPIntrFreeCell<VirtIOBlk<VirtioHal, VirtioTransport>>,
    base_addr: usize,
    irq: usize,
    capacity_blocks: usize,
    condvars: BTreeMap<u16, Condvar>,
}

impl VirtIOBlock {
    pub fn read_block(&self, block_id: usize, buf: &mut [u8]) {
        let nb = *DEV_NON_BLOCKING_ACCESS.exclusive_access();
        if nb {
            let mut req = BlkReq::default();
            let mut resp = BlkResp::default();
            let mut token = 0;
            let task_cx_ptr = self.virtio_blk.exclusive_session(|blk| {
                token = unsafe {
                    blk.read_blocks_nb(block_id, &mut req, buf, &mut resp)
                        .unwrap()
                };
                self.condvars.get(&token).unwrap().wait_no_sched()
            });
            schedule(task_cx_ptr);
            self.virtio_blk.exclusive_session(|blk| {
                unsafe {
                    blk.complete_read_blocks(token, &req, buf, &mut resp)
                        .expect("Error when reading VirtIOBlk");
                }
                self.signal_next_completed(blk);
            });
        } else {
            self.virtio_blk
                .exclusive_access()
                .read_blocks(block_id, buf)
                .expect("Error when reading VirtIOBlk");
        }
    }

    pub fn write_block(&self, block_id: usize, buf: &[u8]) {
        let nb = *DEV_NON_BLOCKING_ACCESS.exclusive_access();
        if nb {
            let mut req = BlkReq::default();
            let mut resp = BlkResp::default();
            let mut token = 0;
            let task_cx_ptr = self.virtio_blk.exclusive_session(|blk| {
                token = unsafe {
                    blk.write_blocks_nb(block_id, &mut req, buf, &mut resp)
                        .unwrap()
                };
                self.condvars.get(&token).unwrap().wait_no_sched()
            });
            schedule(task_cx_ptr);
            self.virtio_blk.exclusive_session(|blk| {
                unsafe {
                    blk.complete_write_blocks(token, &req, buf, &mut resp)
                        .expect("Error when writing VirtIOBlk");
                }
                self.signal_next_completed(blk);
            });
        } else {
            self.virtio_blk
                .exclusive_access()
                .write_blocks(block_id, buf)
                .expect("Error when writing VirtIOBlk");
        }
    }

    pub fn handle_irq(&self) {
        self.virtio_blk.exclusive_session(|blk| {
            let _ = blk.ack_interrupt();
            self.signal_next_completed(blk);
        });
    }

    pub fn num_blocks(&self) -> u64 {
        self.capacity_blocks as u64
    }

    pub fn irq(&self) -> usize {
        self.irq
    }

    pub fn base_addr(&self) -> usize {
        self.base_addr
    }

    fn signal_next_completed(&self, blk: &mut VirtIOBlk<VirtioHal, VirtioTransport>) {
        if let Some(token) = blk.peek_used() {
            self.condvars.get(&token).unwrap().signal();
        }
    }

    pub fn new(device: IrqDevice) -> Self {
        let transport = mmio_transport(device.base, device.size);
        let virtio_blk = VirtIOBlk::<VirtioHal, _>::new(transport).unwrap();
        let capacity_blocks = virtio_blk.capacity() as usize;
        let channels = virtio_blk.virt_queue_size();
        let virtio_blk = unsafe { UPIntrFreeCell::new(virtio_blk) };
        let mut condvars = BTreeMap::new();
        for i in 0..channels {
            let condvar = Condvar::new();
            condvars.insert(i, condvar);
        }
        Self {
            virtio_blk,
            base_addr: device.base,
            irq: device.irq,
            capacity_blocks,
            condvars,
        }
    }
}

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
