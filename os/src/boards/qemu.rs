use crate::BOOT_HART_ID;
use crate::drivers::block::BLOCK_DEVICE;
use crate::drivers::chardev::{CharDevice, UART};
use crate::drivers::plic::{IntrTargetPriority, PLIC};
use crate::drivers::{KEYBOARD_DEVICE, MOUSE_DEVICE};
use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicBool, Ordering};
use fdt::{Fdt, node::FdtNode};
use riscv::register::sie;

const MMIO_REGION_CAPACITY: usize = 7;
const NET_IRQ: usize = 4;
const KEYBOARD_IRQ: usize = 5;
const MOUSE_IRQ: usize = 6;
const GPU_IRQ: usize = 7;
const BLOCK_IRQ: usize = 8;
const EARLY_UART_BASE: usize = 0x1000_0000;

pub type BlockDeviceImpl = crate::drivers::block::VirtIOBlock;
pub type CharDeviceImpl = crate::drivers::chardev::NS16550a;

#[derive(Clone, Copy, Default)]
pub struct MmioRange {
    pub base: usize,
    pub size: usize,
}

#[derive(Clone, Copy, Default)]
struct IrqDevice {
    base: usize,
    size: usize,
    irq: usize,
}

#[derive(Clone, Copy)]
struct BoardConfig {
    clock_freq: usize,
    memory_end: usize,
    uart: IrqDevice,
    plic: MmioRange,
    block: IrqDevice,
    gpu: IrqDevice,
    keyboard: IrqDevice,
    mouse: IrqDevice,
    net: Option<IrqDevice>,
    mmio_regions: [MmioRange; MMIO_REGION_CAPACITY],
    mmio_region_count: usize,
}

impl BoardConfig {
    const fn empty() -> Self {
        Self {
            clock_freq: 0,
            memory_end: 0,
            uart: IrqDevice {
                base: 0,
                size: 0,
                irq: 0,
            },
            plic: MmioRange { base: 0, size: 0 },
            block: IrqDevice {
                base: 0,
                size: 0,
                irq: 0,
            },
            gpu: IrqDevice {
                base: 0,
                size: 0,
                irq: 0,
            },
            keyboard: IrqDevice {
                base: 0,
                size: 0,
                irq: 0,
            },
            mouse: IrqDevice {
                base: 0,
                size: 0,
                irq: 0,
            },
            net: None,
            mmio_regions: [MmioRange { base: 0, size: 0 }; MMIO_REGION_CAPACITY],
            mmio_region_count: 0,
        }
    }
}

struct BoardConfigCell {
    initialized: AtomicBool,
    inner: UnsafeCell<BoardConfig>,
}

unsafe impl Sync for BoardConfigCell {}

impl BoardConfigCell {
    const fn new() -> Self {
        Self {
            initialized: AtomicBool::new(false),
            inner: UnsafeCell::new(BoardConfig::empty()),
        }
    }

    fn init(&self, config: BoardConfig) {
        assert!(
            !self.initialized.load(Ordering::Relaxed),
            "board config initialized twice"
        );
        unsafe {
            *self.inner.get() = config;
        }
        self.initialized.store(true, Ordering::Release);
    }

    fn get(&self) -> &'static BoardConfig {
        assert!(
            self.initialized.load(Ordering::Acquire),
            "board config accessed before DTB init"
        );
        unsafe { &*self.inner.get() }
    }
}

static BOARD_CONFIG: BoardConfigCell = BoardConfigCell::new();

fn board_config() -> &'static BoardConfig {
    BOARD_CONFIG.get()
}

fn compatible_contains(node: FdtNode<'_, '_>, compatibles: &[&str]) -> bool {
    node.compatible()
        .map(|node_compatibles| {
            node_compatibles
                .all()
                .any(|name| compatibles.contains(&name))
        })
        .unwrap_or(false)
}

fn property_str<'a>(node: FdtNode<'_, 'a>, name: &str) -> Option<&'a str> {
    node.property(name)
        .and_then(|property| core::str::from_utf8(property.value).ok())
        .map(|value| value.trim_end_matches('\0'))
}

fn first_reg(node: FdtNode<'_, '_>, context: &str) -> MmioRange {
    let region = node
        .reg()
        .and_then(|mut regions| regions.next())
        .unwrap_or_else(|| panic!("{} node is missing a usable reg property", context));

    MmioRange {
        base: region.starting_address as usize,
        size: region
            .size
            .unwrap_or_else(|| panic!("{} node reg is missing size", context)),
    }
}

fn irq_device(node: FdtNode<'_, '_>, context: &str) -> IrqDevice {
    let range = first_reg(node, context);
    let irq = node
        .interrupts()
        .and_then(|mut interrupts| interrupts.next())
        .unwrap_or_else(|| panic!("{} node is missing an interrupts property", context));

    IrqDevice {
        base: range.base,
        size: range.size,
        irq,
    }
}

fn push_mmio_region(config: &mut BoardConfig, range: MmioRange) {
    assert!(
        config.mmio_region_count < config.mmio_regions.len(),
        "too many MMIO regions discovered in DTB"
    );
    config.mmio_regions[config.mmio_region_count] = range;
    config.mmio_region_count += 1;
}

fn set_required_device(slot: &mut IrqDevice, value: IrqDevice, context: &str) {
    assert_eq!(slot.base, 0, "duplicate {} device in DTB", context);
    *slot = value;
}

unsafe extern "C" {
    safe fn ekernel();
}

pub fn init_from_dtb(dtb_addr: usize) {
    let fdt = unsafe { Fdt::from_ptr(dtb_addr as *const u8) }
        .unwrap_or_else(|err| panic!("failed to parse DTB at {:#x}: {:?}", dtb_addr, err));

    let mut config = BoardConfig::empty();
    config.clock_freq = fdt
        .cpus()
        .next()
        .map(|cpu| cpu.timebase_frequency())
        .expect("DTB is missing cpu timebase-frequency");

    let kernel_end = ekernel as usize;
    let mut memory_region = None;
    for node in fdt.all_nodes() {
        let is_memory_node = property_str(node, "device_type") == Some("memory")
            || node.name.split('@').next() == Some("memory");
        if !is_memory_node {
            continue;
        }
        if let Some(regions) = node.reg() {
            for region in regions {
                let start = region.starting_address as usize;
                let end = start + region.size.unwrap_or(0);
                if kernel_end >= start && kernel_end < end {
                    memory_region = Some(region);
                    break;
                }
            }
        }
        if memory_region.is_some() {
            break;
        }
    }
    let memory_region = memory_region
        .unwrap_or_else(|| panic!("no memory region in DTB contains ekernel={:#x}", kernel_end));
    let memory_size = memory_region
        .size
        .unwrap_or_else(|| panic!("selected memory region is missing size"));
    config.memory_end = memory_region.starting_address as usize + memory_size;

    let uart_node = fdt
        .find_compatible(&["ns16550a"])
        .unwrap_or_else(|| panic!("DTB is missing ns16550a UART"));
    config.uart = irq_device(uart_node, "UART");

    let plic_node = fdt
        .find_compatible(&["sifive,plic-1.0.0", "riscv,plic0"])
        .unwrap_or_else(|| panic!("DTB is missing PLIC"));
    config.plic = first_reg(plic_node, "PLIC");

    let uart_range = MmioRange {
        base: config.uart.base,
        size: config.uart.size,
    };
    let plic_range = config.plic;
    push_mmio_region(&mut config, uart_range);
    push_mmio_region(&mut config, plic_range);

    for node in fdt.all_nodes() {
        if !compatible_contains(node, &["virtio,mmio"]) {
            continue;
        }
        let device = irq_device(node, "virtio-mmio");
        match device.irq {
            BLOCK_IRQ => {
                set_required_device(&mut config.block, device, "virtio block");
                push_mmio_region(
                    &mut config,
                    MmioRange {
                        base: device.base,
                        size: device.size,
                    },
                );
            }
            GPU_IRQ => {
                set_required_device(&mut config.gpu, device, "virtio gpu");
                push_mmio_region(
                    &mut config,
                    MmioRange {
                        base: device.base,
                        size: device.size,
                    },
                );
            }
            KEYBOARD_IRQ => {
                set_required_device(&mut config.keyboard, device, "virtio keyboard");
                push_mmio_region(
                    &mut config,
                    MmioRange {
                        base: device.base,
                        size: device.size,
                    },
                );
            }
            MOUSE_IRQ => {
                set_required_device(&mut config.mouse, device, "virtio mouse");
                push_mmio_region(
                    &mut config,
                    MmioRange {
                        base: device.base,
                        size: device.size,
                    },
                );
            }
            NET_IRQ => {
                assert!(config.net.is_none(), "duplicate virtio net device in DTB");
                config.net = Some(device);
                push_mmio_region(
                    &mut config,
                    MmioRange {
                        base: device.base,
                        size: device.size,
                    },
                );
            }
            _ => {}
        }
    }

    assert_ne!(config.block.base, 0, "DTB is missing virtio block device");
    assert_ne!(config.gpu.base, 0, "DTB is missing virtio gpu device");
    assert_ne!(
        config.keyboard.base, 0,
        "DTB is missing virtio keyboard device"
    );
    assert_ne!(config.mouse.base, 0, "DTB is missing virtio mouse device");
    assert_ne!(config.uart.base, 0, "DTB is missing uart base");
    assert_ne!(config.plic.base, 0, "DTB is missing plic base");

    BOARD_CONFIG.init(config);
}

pub fn clock_freq() -> usize {
    board_config().clock_freq
}

pub fn memory_end() -> usize {
    board_config().memory_end
}

pub fn mmio_regions() -> &'static [MmioRange] {
    let config = board_config();
    &config.mmio_regions[..config.mmio_region_count]
}

pub fn uart_base() -> usize {
    if BOARD_CONFIG.initialized.load(Ordering::Acquire) {
        board_config().uart.base
    } else {
        EARLY_UART_BASE
    }
}

pub fn uart_irq() -> usize {
    board_config().uart.irq
}

pub fn plic_base() -> usize {
    board_config().plic.base
}

pub fn block_base() -> usize {
    board_config().block.base
}

pub fn block_irq() -> usize {
    board_config().block.irq
}

pub fn gpu_base() -> usize {
    board_config().gpu.base
}

pub fn keyboard_base() -> usize {
    board_config().keyboard.base
}

pub fn keyboard_irq() -> usize {
    board_config().keyboard.irq
}

pub fn mouse_base() -> usize {
    board_config().mouse.base
}

pub fn mouse_irq() -> usize {
    board_config().mouse.irq
}

pub fn net_base() -> Option<usize> {
    board_config().net.map(|device| device.base)
}

pub fn device_init(hart_id: usize) {
    let mut plic = unsafe { PLIC::new(plic_base()) };
    let supervisor = IntrTargetPriority::Supervisor;
    let machine = IntrTargetPriority::Machine;
    plic.set_threshold(hart_id, supervisor, 0);
    plic.set_threshold(hart_id, machine, 1);

    for irq in [keyboard_irq(), mouse_irq(), block_irq(), uart_irq()] {
        plic.enable(hart_id, supervisor, irq);
        plic.set_priority(irq, 1);
    }

    unsafe {
        sie::set_sext();
    }
}

pub fn irq_handler() {
    let mut plic = unsafe { PLIC::new(plic_base()) };
    let hart_id = BOOT_HART_ID.load(Ordering::Relaxed);
    let intr_src_id = plic.claim(hart_id, IntrTargetPriority::Supervisor);
    let keyboard_irq = keyboard_irq() as u32;
    let mouse_irq = mouse_irq() as u32;
    let block_irq = block_irq() as u32;
    let uart_irq = uart_irq() as u32;

    if intr_src_id == keyboard_irq {
        KEYBOARD_DEVICE.handle_irq();
    } else if intr_src_id == mouse_irq {
        MOUSE_DEVICE.handle_irq();
    } else if intr_src_id == block_irq {
        BLOCK_DEVICE.handle_irq();
    } else if intr_src_id == uart_irq {
        UART.handle_irq();
    } else {
        panic!("unsupported IRQ {}", intr_src_id);
    }

    plic.complete(hart_id, IntrTargetPriority::Supervisor, intr_src_id);
}
