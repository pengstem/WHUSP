///! Ref: https://www.lammertbies.nl/comm/info/serial-uart
///! Ref: ns16550a datasheet: https://datasheetspdf.com/pdf-file/605590/NationalSemiconductor/NS16550A/1
///! Ref: ns16450 datasheet: https://datasheetspdf.com/pdf-file/1311818/NationalSemiconductor/NS16450/1
use super::CharDevice;
use crate::sync::{Condvar, UPIntrFreeCell};
use crate::task::schedule;
use alloc::collections::VecDeque;
use bitflags::*;
use core::ptr::NonNull;
use volatile::{
    VolatilePtr,
    access::{ReadOnly, WriteOnly},
};

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

pub struct NS16550a {
    inner: UPIntrFreeCell<NS16550aInner>,
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

    pub fn read_buffer_is_empty(&self) -> bool {
        self.inner
            .exclusive_session(|inner| inner.read_buffer.is_empty())
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
            if let Some(ch) = inner.read_buffer.pop_front() {
                return ch;
            } else {
                let task_cx_ptr = self.condvar.wait_no_sched();
                drop(inner);
                schedule(task_cx_ptr);
            }
        }
    }
    fn write(&self, ch: u8) {
        let mut inner = self.inner.exclusive_access();
        inner.ns16550a.write(ch);
    }
    fn handle_irq(&self) {
        let mut count = 0;
        self.inner.exclusive_session(|inner| {
            while let Some(ch) = inner.ns16550a.read() {
                count += 1;
                inner.read_buffer.push_back(ch);
            }
        });
        if count > 0 {
            self.condvar.signal();
        }
    }
}
