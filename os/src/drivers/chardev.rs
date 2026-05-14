///! Ref: https://www.lammertbies.nl/comm/info/serial-uart
///! Ref: ns16550a datasheet: https://datasheetspdf.com/pdf-file/605590/NationalSemiconductor/NS16550A/1
///! Ref: ns16450 datasheet: https://datasheetspdf.com/pdf-file/1311818/NationalSemiconductor/NS16450/1
use crate::board::CharDeviceImpl;
use crate::sync::{Condvar, UPIntrFreeCell};
#[cfg(not(target_arch = "loongarch64"))]
use crate::task::schedule;
#[cfg(target_arch = "loongarch64")]
use crate::task::suspend_current_and_run_next;
use alloc::collections::VecDeque;
use alloc::sync::Arc;
use bitflags::*;
use core::ptr::NonNull;
use lazy_static::*;
use volatile::{
    VolatilePtr,
    access::{ReadOnly, WriteOnly},
};

pub trait CharDevice {
    fn init(&self);
    #[allow(dead_code)]
    fn read(&self) -> u8;
    fn try_read(&self) -> Option<u8>;
    #[allow(dead_code)]
    fn has_input(&self) -> bool;
    fn write(&self, ch: u8);
    #[cfg(target_arch = "riscv64")]
    fn handle_irq(&self);
}

lazy_static! {
    pub static ref UART: Arc<CharDeviceImpl> =
        Arc::new(CharDeviceImpl::new(crate::board::uart_base()));
}

const RBR: usize = 0;
const THR: usize = 0;
const IER_REG: usize = 1;
const MCR_REG: usize = 4;
const LSR_REG: usize = 5;

bitflags! {
    /// InterruptEnableRegister
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct IER: u8 {
        const RX_AVAILABLE = 1 << 0;
        const TX_EMPTY = 1 << 1;
    }

    /// LineStatusRegister
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct LSR: u8 {
        const DATA_AVAILABLE = 1 << 0;
        const THR_EMPTY = 1 << 5;
    }

    /// Model Control Register
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct MCR: u8 {
        const DATA_TERMINAL_READY = 1 << 0;
        const REQUEST_TO_SEND = 1 << 1;
        const AUX_OUTPUT1 = 1 << 2;
        const AUX_OUTPUT2 = 1 << 3;
    }
}

pub struct NS16550aRaw {
    base_addr: usize,
}

impl NS16550aRaw {
    fn read_reg(&self, offset: usize) -> u8 {
        let ptr = NonNull::new((self.base_addr + offset) as *mut u8).unwrap();
        unsafe { VolatilePtr::new_restricted(ReadOnly, ptr).read() }
    }

    fn write_reg(&self, offset: usize, value: u8) {
        let ptr = NonNull::new((self.base_addr + offset) as *mut u8).unwrap();
        unsafe { VolatilePtr::new_restricted(WriteOnly, ptr).write(value) };
    }

    pub fn new(base_addr: usize) -> Self {
        Self { base_addr }
    }

    pub fn init(&mut self) {
        let mut mcr = MCR::empty();
        mcr |= MCR::DATA_TERMINAL_READY;
        mcr |= MCR::REQUEST_TO_SEND;
        mcr |= MCR::AUX_OUTPUT2;
        self.write_reg(MCR_REG, mcr.bits());
        let ier = IER::RX_AVAILABLE;
        self.write_reg(IER_REG, ier.bits());
    }

    pub fn read(&mut self) -> Option<u8> {
        let lsr = LSR::from_bits_truncate(self.read_reg(LSR_REG));
        if lsr.contains(LSR::DATA_AVAILABLE) {
            Some(self.read_reg(RBR))
        } else {
            None
        }
    }

    pub fn write(&mut self, ch: u8) {
        loop {
            let lsr = LSR::from_bits_truncate(self.read_reg(LSR_REG));
            if lsr.contains(LSR::THR_EMPTY) {
                self.write_reg(THR, ch);
                break;
            }
        }
    }
}

struct NS16550aInner {
    ns16550a: NS16550aRaw,
    read_buffer: VecDeque<u8>,
}

impl NS16550aInner {
    fn poll_rx(&mut self) {
        while let Some(ch) = self.ns16550a.read() {
            self.read_buffer.push_back(ch);
        }
    }
}

pub struct NS16550a {
    inner: UPIntrFreeCell<NS16550aInner>,
    // CONTEXT: signaled from the RV IRQ path; LA polls instead of taking
    // external UART interrupts, so the field stays allocated but unread there.
    #[allow(dead_code)]
    condvar: Condvar,
}

impl NS16550a {
    pub fn new(base_addr: usize) -> Self {
        let inner = NS16550aInner {
            ns16550a: NS16550aRaw::new(base_addr),
            read_buffer: VecDeque::new(),
        };
        //inner.ns16550a.init();
        Self {
            inner: unsafe { UPIntrFreeCell::new(inner) },
            condvar: Condvar::new(),
        }
    }
}

impl CharDevice for NS16550a {
    fn init(&self) {
        let mut inner = self.inner.exclusive_access();
        inner.ns16550a.init();
        drop(inner);
    }

    fn read(&self) -> u8 {
        loop {
            let mut inner = self.inner.exclusive_access();
            inner.poll_rx();
            if let Some(ch) = inner.read_buffer.pop_front() {
                return ch;
            } else {
                #[cfg(target_arch = "loongarch64")]
                {
                    // CONTEXT: LoongArch external UART IRQ routing is not wired yet, so
                    // blocking on the condvar would sleep forever. Poll until IRQ arrives.
                    drop(inner);
                    suspend_current_and_run_next();
                    continue;
                }
                #[cfg(not(target_arch = "loongarch64"))]
                {
                    let task_cx_ptr = self.condvar.wait_no_sched();
                    drop(inner);
                    schedule(task_cx_ptr);
                }
            }
        }
    }
    fn try_read(&self) -> Option<u8> {
        let mut inner = self.inner.exclusive_access();
        inner.poll_rx();
        inner.read_buffer.pop_front()
    }
    fn has_input(&self) -> bool {
        let mut inner = self.inner.exclusive_access();
        inner.poll_rx();
        !inner.read_buffer.is_empty()
    }
    fn write(&self, ch: u8) {
        let mut inner = self.inner.exclusive_access();
        inner.ns16550a.write(ch);
    }
    #[cfg(target_arch = "riscv64")]
    fn handle_irq(&self) {
        let mut count = 0;
        self.inner.exclusive_session(|inner| {
            while let Some(ch) = inner.ns16550a.read() {
                count += 1;
                inner.read_buffer.push_back(ch);
            }
        });
        if count > 0 {
            crate::fs::console_tty_drain_uart();
            self.condvar.signal();
        }
    }
}
