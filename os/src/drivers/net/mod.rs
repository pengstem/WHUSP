use core::any::Any;

use crate::drivers::virtio::{VirtioHal, VirtioTransport, mmio_transport};
use crate::sync::UPIntrFreeCell;
use alloc::{sync::Arc, vec};
use lazy_static::*;
use virtio_drivers::device::net::VirtIONetRaw;

const NET_QUEUE_SIZE: usize = 16;
const NET_RECEIVE_BUFFER_SIZE: usize = 2048;

lazy_static! {
    pub static ref NET_DEVICE: Arc<dyn NetDevice> = Arc::new(VirtIONetWrapper::new());
}

pub trait NetDevice: Send + Sync + Any {
    fn transmit(&self, data: &[u8]);
    fn receive(&self, data: &mut [u8]) -> usize;
}

pub struct VirtIONetWrapper(
    UPIntrFreeCell<VirtIONetRaw<VirtioHal, VirtioTransport, NET_QUEUE_SIZE>>,
);

impl NetDevice for VirtIONetWrapper {
    fn transmit(&self, data: &[u8]) {
        self.0
            .exclusive_access()
            .send(data)
            .expect("can't send data")
    }

    fn receive(&self, data: &mut [u8]) -> usize {
        let mut recv_buf = vec![0u8; data.len().max(NET_RECEIVE_BUFFER_SIZE)];
        let (header_len, packet_len) = self
            .0
            .exclusive_access()
            .receive_wait(&mut recv_buf)
            .expect("can't receive data");
        assert!(
            packet_len <= data.len(),
            "receive buffer is too small for network packet"
        );
        data[..packet_len].copy_from_slice(&recv_buf[header_len..header_len + packet_len]);
        packet_len
    }
}

impl VirtIONetWrapper {
    pub fn new() -> Self {
        let device =
            crate::board::net_device().expect("DTB is missing a virtio net device for NET_DEVICE");
        let virtio = VirtIONetRaw::<VirtioHal, _, NET_QUEUE_SIZE>::new(mmio_transport(
            device.base,
            device.size,
        ))
        .expect("can't create net device by virtio");
        unsafe { VirtIONetWrapper(UPIntrFreeCell::new(virtio)) }
    }
}
