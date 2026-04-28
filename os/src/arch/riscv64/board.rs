use crate::BOOT_HART_ID;
use crate::drivers::chardev::{CharDevice, UART};
use crate::drivers::plic::{IntrTargetPriority, PLIC};
use crate::drivers::{KEYBOARD_DEVICE, MOUSE_DEVICE};
use core::cell::UnsafeCell;
use core::ptr::NonNull;
use core::sync::atomic::{AtomicBool, Ordering};
use fdt::{Fdt, node::FdtNode};
use riscv::register::sie;
use virtio_drivers::transport::{
    DeviceType, Transport,
    mmio::{MmioTransport, VirtIOHeader},
};

const BLOCK_DEVICE_CAPACITY: usize = 8;
const MMIO_REGION_CAPACITY: usize = 10;
const EARLY_UART_BASE: usize = 0x1000_0000;

pub type BlockDeviceImpl = crate::drivers::block::VirtIOBlock;
pub type CharDeviceImpl = crate::drivers::chardev::NS16550a;

#[derive(Clone, Copy, Default)]
pub struct MmioRange {
    pub base: usize,
    pub size: usize,
}

#[derive(Clone, Copy, Default)]
pub struct IrqDevice {
    pub base: usize,
    pub size: usize,
    pub irq: usize,
}

#[derive(Clone, Copy, Default)]
pub struct PciDevice {
    pub ecam_base: usize,
    pub bar_mem_start: usize,
    pub bar_mem_end: usize,
    pub bus: u8,
    pub device: u8,
    pub function: u8,
    pub irq: usize,
}

#[derive(Clone, Copy)]
pub enum BlockDeviceConfig {
    Mmio(IrqDevice),
    Pci(PciDevice),
}

impl Default for BlockDeviceConfig {
    fn default() -> Self {
        Self::Mmio(IrqDevice::default())
    }
}

#[derive(Clone, Copy)]
struct BoardConfig {
    clock_freq: usize,
    memory_end: usize,
    uart: IrqDevice,
    plic: MmioRange,
    blocks: [BlockDeviceConfig; BLOCK_DEVICE_CAPACITY],
    block_count: usize,
    gpu: Option<IrqDevice>,
    keyboard: Option<IrqDevice>,
    mouse: Option<IrqDevice>,
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
            blocks: [BlockDeviceConfig::Mmio(IrqDevice {
                base: 0,
                size: 0,
                irq: 0,
            }); BLOCK_DEVICE_CAPACITY],
            block_count: 0,
            gpu: None,
            keyboard: None,
            mouse: None,
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

fn set_required_device(slot: &mut Option<IrqDevice>, value: IrqDevice, context: &str) {
    assert!(slot.is_none(), "duplicate {context} device in DTB");
    *slot = Some(value);
}

fn push_block_device(config: &mut BoardConfig, value: IrqDevice) {
    assert!(
        config.block_count < config.blocks.len(),
        "too many virtio block devices discovered in DTB"
    );
    config.blocks[config.block_count] = BlockDeviceConfig::Mmio(value);
    config.block_count += 1;
}

fn push_device_mmio_region(config: &mut BoardConfig, device: IrqDevice) {
    push_mmio_region(
        config,
        MmioRange {
            base: device.base,
            size: device.size,
        },
    );
}

unsafe extern "C" {
    safe fn ekernel();
}

fn virtio_device_type(device: IrqDevice) -> Option<DeviceType> {
    let header = NonNull::new(device.base as *mut VirtIOHeader).unwrap();
    unsafe { MmioTransport::new(header, device.size) }
        .ok()
        .map(|transport| transport.device_type())
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
        match virtio_device_type(device) {
            Some(DeviceType::Block) => {
                push_block_device(&mut config, device);
                push_device_mmio_region(&mut config, device);
            }
            Some(DeviceType::GPU) => {
                set_required_device(&mut config.gpu, device, "virtio gpu");
                push_device_mmio_region(&mut config, device);
            }
            Some(DeviceType::Input) => {
                if config.keyboard.is_none() {
                    set_required_device(&mut config.keyboard, device, "virtio keyboard");
                } else if config.mouse.is_none() {
                    set_required_device(&mut config.mouse, device, "virtio mouse");
                } else {
                    panic!("too many virtio input devices in DTB");
                }
                push_device_mmio_region(&mut config, device);
            }
            Some(DeviceType::Network) => {
                assert!(config.net.is_none(), "duplicate virtio net device in DTB");
                config.net = Some(device);
                push_device_mmio_region(&mut config, device);
            }
            _ => {}
        }
    }

    config.blocks[..config.block_count].sort_by_key(|device| match device {
        BlockDeviceConfig::Mmio(device) => device.base,
        BlockDeviceConfig::Pci(device) => {
            ((device.bus as usize) << 16)
                | ((device.device as usize) << 8)
                | device.function as usize
        }
    });

    assert_ne!(config.block_count, 0, "DTB is missing virtio block device");
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

pub fn block_devices() -> &'static [BlockDeviceConfig] {
    let config = board_config();
    &config.blocks[..config.block_count]
}

pub fn pci_transport(_device: PciDevice) -> virtio_drivers::transport::pci::PciTransport {
    unreachable!("RISC-V QEMU uses virtio-mmio block devices")
}

pub fn gpu_device() -> Option<IrqDevice> {
    board_config().gpu
}

pub fn keyboard_device() -> Option<IrqDevice> {
    board_config().keyboard
}

pub fn keyboard_irq() -> Option<usize> {
    board_config().keyboard.map(|device| device.irq)
}

pub fn mouse_device() -> Option<IrqDevice> {
    board_config().mouse
}

pub fn mouse_irq() -> Option<usize> {
    board_config().mouse.map(|device| device.irq)
}

pub fn net_device() -> Option<IrqDevice> {
    board_config().net
}

pub fn device_init(hart_id: usize) {
    let mut plic = unsafe { PLIC::new(plic_base()) };
    let supervisor = IntrTargetPriority::Supervisor;
    let machine = IntrTargetPriority::Machine;
    plic.set_threshold(hart_id, supervisor, 0);
    plic.set_threshold(hart_id, machine, 1);

    plic.enable(hart_id, supervisor, uart_irq());
    plic.set_priority(uart_irq(), 1);
    for irq in [keyboard_irq(), mouse_irq()].into_iter().flatten() {
        plic.enable(hart_id, supervisor, irq);
        plic.set_priority(irq, 1);
    }
    for block in block_devices() {
        let BlockDeviceConfig::Mmio(block) = block else {
            unreachable!("RISC-V QEMU uses virtio-mmio block devices");
        };
        plic.enable(hart_id, supervisor, block.irq);
        plic.set_priority(block.irq, 1);
    }

    unsafe {
        sie::set_sext();
    }
}

pub fn irq_handler() {
    let mut plic = unsafe { PLIC::new(plic_base()) };
    let hart_id = BOOT_HART_ID.load(Ordering::Relaxed);
    let intr_src_id = plic.claim(hart_id, IntrTargetPriority::Supervisor);
    let keyboard_irq = keyboard_irq().map(|irq| irq as u32);
    let mouse_irq = mouse_irq().map(|irq| irq as u32);
    let uart_irq = uart_irq() as u32;

    if keyboard_irq == Some(intr_src_id) {
        if let Some(device) = KEYBOARD_DEVICE.as_ref() {
            device.handle_irq();
        }
    } else if mouse_irq == Some(intr_src_id) {
        if let Some(device) = MOUSE_DEVICE.as_ref() {
            device.handle_irq();
        }
    } else if intr_src_id == uart_irq {
        UART.handle_irq();
    } else if crate::drivers::block::handle_irq(intr_src_id as usize) {
    } else {
        panic!("unsupported IRQ {}", intr_src_id);
    }

    plic.complete(hart_id, IntrTargetPriority::Supervisor, intr_src_id);
}
