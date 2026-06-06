use loongArch64::iocsr::{iocsr_read_d, iocsr_write_d, iocsr_write_w};

const EIOINTC_NODEMAP_OFFSET: usize = 0x0a0;
const EIOINTC_IPMAP_OFFSET: usize = 0x0c0;
const EIOINTC_ENABLE_OFFSET: usize = 0x200;
const EIOINTC_BOUNCE_OFFSET: usize = 0x280;
const EIOINTC_ISR_OFFSET: usize = 0x400;
const EIOINTC_ROUTE_OFFSET: usize = 0x800;
const EIOINTC_VEC_REG_COUNT: usize = 4;
const EIOINTC_VEC_COUNT_PER_REG: usize = 64;
const EIOINTC_VEC_COUNT: usize = EIOINTC_VEC_REG_COUNT * EIOINTC_VEC_COUNT_PER_REG;

const PCH_PIC_MASK_OFFSET: usize = 0x20;
const PCH_PIC_EDGE_OFFSET: usize = 0x60;
const PCH_PIC_POL_OFFSET: usize = 0x3e0;
const PCH_PIC_HTVEC_OFFSET: usize = 0x200;
const PCH_PIC_COUNT_PER_REG: usize = 32;
const PCH_PIC_REG_COUNT: usize = 2;

#[derive(Clone, Copy)]
pub struct Eiointc {
    base: usize,
}

impl Eiointc {
    pub const fn new(base: usize) -> Self {
        Self { base }
    }

    pub fn init(self) {
        // CONTEXT: QEMU LA virt routes PCH PIC output through extended IOI.
        // Keep the setup uniprocessor: route all vectors to node/core 0,
        // matching the current single-hart contest runtime. The contest QEMU
        // already exposes EIOINTC; probing IOCSR MISC_FUNC is not required for
        // this virtual platform and may trap on trimmed implementations.
        for i in 0..(EIOINTC_VEC_COUNT / 32) {
            let data = ((1 << (i * 2 + 1)) << 16) | (1 << (i * 2));
            iocsr_write_w(self.base + EIOINTC_NODEMAP_OFFSET + i * 4, data);
        }
        for i in 0..(EIOINTC_VEC_COUNT / 32 / 4) {
            let bit = 1 << 1;
            let data = bit | (bit << 8) | (bit << 16) | (bit << 24);
            iocsr_write_w(self.base + EIOINTC_IPMAP_OFFSET + i * 4, data);
        }
        for i in 0..(EIOINTC_VEC_COUNT / 4) {
            let bit = 1;
            let data = bit | (bit << 8) | (bit << 16) | (bit << 24);
            iocsr_write_w(self.base + EIOINTC_ROUTE_OFFSET + i * 4, data);
        }
        for i in 0..(EIOINTC_VEC_COUNT / 32) {
            iocsr_write_w(self.base + EIOINTC_BOUNCE_OFFSET + i * 4, u32::MAX);
        }
    }

    pub fn enable(self, irq: usize) {
        let (offset, bit) = split_eiointc_bit(irq);
        for base in [EIOINTC_ENABLE_OFFSET, EIOINTC_BOUNCE_OFFSET] {
            let addr = self.base + base + offset;
            iocsr_write_d(addr, iocsr_read_d(addr) | bit);
        }
    }

    pub fn claim(self) -> Option<usize> {
        for i in 0..(EIOINTC_VEC_COUNT / 64) {
            let flags = iocsr_read_d(self.base + EIOINTC_ISR_OFFSET + i * 8);
            if flags != 0 {
                return Some(flags.trailing_zeros() as usize + 64 * i);
            }
        }
        None
    }

    pub fn complete(self, irq: usize) {
        let (offset, bit) = split_eiointc_bit(irq);
        iocsr_write_d(self.base + EIOINTC_ISR_OFFSET + offset, bit);
    }
}

fn split_eiointc_bit(irq: usize) -> (usize, u64) {
    (irq / 64 * 8, 1u64 << (irq % 64))
}

#[derive(Clone, Copy)]
pub struct PchPic {
    base: usize,
}

impl PchPic {
    pub const fn new(base: usize) -> Self {
        Self { base }
    }

    pub fn init(self) {
        for reg in 0..PCH_PIC_REG_COUNT {
            let offset = reg * 4;
            self.write_word(PCH_PIC_EDGE_OFFSET + offset, 0);
            self.write_word(PCH_PIC_POL_OFFSET + offset, 0);
            self.write_word(PCH_PIC_MASK_OFFSET + offset, u32::MAX);
        }
    }

    pub fn enable(self, irq: usize) {
        let (offset, bit) = split_pch_pic_bit(irq);
        let mask_addr = PCH_PIC_MASK_OFFSET + offset;
        self.write_word(mask_addr, self.read_word(mask_addr) & !bit);
        self.write_byte(PCH_PIC_HTVEC_OFFSET + irq, irq as u8);
    }

    fn read_word(self, offset: usize) -> u32 {
        unsafe { ((self.base + offset) as *mut u32).read_volatile() }
    }

    fn write_word(self, offset: usize, value: u32) {
        unsafe {
            ((self.base + offset) as *mut u32).write_volatile(value);
        }
    }

    fn write_byte(self, offset: usize, value: u8) {
        unsafe {
            ((self.base + offset) as *mut u8).write_volatile(value);
        }
    }
}

fn split_pch_pic_bit(irq: usize) -> (usize, u32) {
    (
        irq / PCH_PIC_COUNT_PER_REG * 4,
        1u32 << (irq % PCH_PIC_COUNT_PER_REG),
    )
}
