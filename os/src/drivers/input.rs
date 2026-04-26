use crate::board::IrqDevice;
use crate::drivers::virtio::{VirtioHal, VirtioTransport, mmio_transport};
use crate::sync::UPIntrFreeCell;
use alloc::sync::Arc;
use core::any::Any;
use virtio_drivers::device::input::VirtIOInput;

struct VirtIOInputInner {
    virtio_input: VirtIOInput<VirtioHal, VirtioTransport>,
}

struct VirtIOInputWrapper {
    inner: UPIntrFreeCell<VirtIOInputInner>,
}

pub trait InputDevice: Send + Sync + Any {
    fn handle_irq(&self);
}

lazy_static::lazy_static!(
    pub static ref KEYBOARD_DEVICE: Option<Arc<dyn InputDevice>> = crate::board::keyboard_device()
        .map(|device| Arc::new(VirtIOInputWrapper::new(device)) as Arc<dyn InputDevice>);
    pub static ref MOUSE_DEVICE: Option<Arc<dyn InputDevice>> = crate::board::mouse_device()
        .map(|device| Arc::new(VirtIOInputWrapper::new(device)) as Arc<dyn InputDevice>);
);

impl VirtIOInputWrapper {
    pub fn new(device: IrqDevice) -> Self {
        let inner = VirtIOInputInner {
            virtio_input: VirtIOInput::<VirtioHal, _>::new(mmio_transport(
                device.base,
                device.size,
            ))
            .unwrap(),
        };
        Self {
            inner: unsafe { UPIntrFreeCell::new(inner) },
        }
    }
}

impl InputDevice for VirtIOInputWrapper {
    fn handle_irq(&self) {
        self.inner.exclusive_session(|inner| {
            inner.virtio_input.ack_interrupt();
            while inner.virtio_input.pop_pending_event().is_some() {}
        });
    }
}
