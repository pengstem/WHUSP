use crate::DEV_NON_BLOCKING_ACCESS;
use crate::board::IrqDevice;
use crate::drivers::bus::virtio::{VirtioHal, VirtioTransport, mmio_transport};
use crate::sync::{Condvar, UPIntrFreeCell};
use crate::task::schedule;
use alloc::collections::BTreeMap;
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
