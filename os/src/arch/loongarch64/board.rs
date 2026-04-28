use crate::arch::loongarch64::mm::phys_to_virt;
use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicBool, Ordering};
use fdt::{Fdt, node::FdtNode};
use log::{info, warn};
use virtio_drivers::transport::{
    DeviceType,
    pci::{
        PciTransport,
        bus::{BarInfo, Cam, Command, DeviceFunction, MemoryBarType, MmioCam, PciRoot},
        virtio_device_type,
    },
};

const BLOCK_DEVICE_CAPACITY: usize = 8;
const MMIO_REGION_CAPACITY: usize = 16;
const EARLY_UART_BASE: usize = 0x1fe0_01e0;
const FALLBACK_CLOCK_FREQ: usize = 100_000_000;
const FALLBACK_PCI_ECAM_BASE: usize = 0x2000_0000;
const FALLBACK_PCI_MEM_BASE: usize = 0x4000_0000;
const FALLBACK_PCI_MEM_SIZE: usize = 0x4000_0000;

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

fn property_usize(value: &[u8]) -> Option<usize> {
    match value.len() {
        4 => Some(u32::from_be_bytes(value.try_into().ok()?) as usize),
        8 => Some(u64::from_be_bytes(value.try_into().ok()?) as usize),
        _ => None,
    }
}

fn cpu_timer_frequency(fdt: &Fdt<'_>) -> usize {
    fdt.cpus()
        .next()
        .and_then(|cpu| {
            let mut clock_frequency = None;
            for property in cpu.properties() {
                if property.name == "timebase-frequency" {
                    return property_usize(property.value);
                }
                if property.name == "clock-frequency" {
                    clock_frequency = property_usize(property.value);
                }
            }
            clock_frequency
        })
        .unwrap_or(FALLBACK_CLOCK_FREQ)
}

fn first_reg(node: FdtNode<'_, '_>, context: &str) -> MmioRange {
    let region = node
        .reg()
        .and_then(|mut regions| regions.next())
        .unwrap_or_else(|| panic!("{} node is missing a usable reg property", context));

    MmioRange {
        base: phys_to_virt(region.starting_address as usize),
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
        .unwrap_or(0);

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

fn push_block_device(config: &mut BoardConfig, value: BlockDeviceConfig) {
    assert!(
        config.block_count < config.blocks.len(),
        "too many virtio block devices discovered in DTB"
    );
    config.blocks[config.block_count] = value;
    config.block_count += 1;
}

unsafe extern "C" {
    safe fn ekernel();
}

struct PciMemory32Allocator {
    start: u32,
    end: u32,
}

impl PciMemory32Allocator {
    fn from_range(start: usize, size: usize) -> Self {
        Self {
            start: start as u32,
            end: (start + size) as u32,
        }
    }

    fn allocate(&mut self, size: u32) -> u32 {
        let allocated = align_up(self.start, size);
        assert!(
            allocated + size <= self.end,
            "PCI BAR memory range exhausted"
        );
        self.start = allocated + size;
        allocated
    }
}

const fn align_up(value: u32, alignment: u32) -> u32 {
    ((value - 1) | (alignment - 1)) + 1
}

fn pci_mem_range_from_dtb(pci_node: FdtNode<'_, '_>) -> Option<(usize, usize)> {
    let ranges = pci_node.property("ranges")?;
    let mut best_start = 0usize;
    let mut best_size = 0usize;
    for range in ranges.value.chunks_exact(28) {
        let prefetchable = range[0] & 0x80 != 0;
        let range_type = range[0] & 0x3;
        let bus_address = u64::from_be_bytes(range[4..12].try_into().unwrap()) as usize;
        let cpu_physical = u64::from_be_bytes(range[12..20].try_into().unwrap()) as usize;
        let size = u64::from_be_bytes(range[20..28].try_into().unwrap()) as usize;
        if !prefetchable
            && matches!(range_type, 0x2 | 0x3)
            && bus_address == cpu_physical
            && bus_address + size < u32::MAX as usize
            && size > best_size
        {
            best_start = cpu_physical;
            best_size = size;
        }
    }
    (best_size != 0).then_some((best_start, best_size))
}

fn allocate_pci_bars(
    root: &mut PciRoot<MmioCam<'static>>,
    device_function: DeviceFunction,
    allocator: &mut PciMemory32Allocator,
) {
    let mut bar_index = 0;
    while bar_index < 6 {
        let info = root
            .bar_info(device_function, bar_index)
            .expect("failed to read PCI BAR");
        if let Some(BarInfo::Memory {
            address_type, size, ..
        }) = info.as_ref()
        {
            if *size > 0 {
                let address = allocator.allocate(*size as u32);
                match address_type {
                    MemoryBarType::Width32 => root.set_bar_32(device_function, bar_index, address),
                    MemoryBarType::Width64 => {
                        root.set_bar_64(device_function, bar_index, address as u64)
                    }
                    _ => panic!("unsupported PCI memory BAR type {:?}", address_type),
                }
            }
        }
        bar_index += 1;
        if info.as_ref().is_some_and(BarInfo::takes_two_entries) {
            bar_index += 1;
        }
    }
    root.set_command(
        device_function,
        Command::IO_SPACE | Command::MEMORY_SPACE | Command::BUS_MASTER,
    );
}

fn discover_pci_blocks(config: &mut BoardConfig, pci_node: FdtNode<'_, '_>) {
    let ecam = first_reg(pci_node, "PCI ECAM");
    let (bar_start, bar_size) = pci_mem_range_from_dtb(pci_node).unwrap_or_else(|| {
        // CONTEXT: The official LoongArch QEMU virt DTS exposes the PCI memory
        // window at this range; keep it as a fallback for older or trimmed DTBs.
        (FALLBACK_PCI_MEM_BASE, FALLBACK_PCI_MEM_SIZE)
    });
    push_mmio_region(config, ecam);
    push_mmio_region(
        config,
        MmioRange {
            base: phys_to_virt(bar_start),
            size: bar_size,
        },
    );

    let mut allocator = PciMemory32Allocator::from_range(bar_start, bar_size);
    let mut root = PciRoot::new(unsafe { MmioCam::new(ecam.base as *mut u8, Cam::Ecam) });
    for (device_function, info) in root.enumerate_bus(0) {
        let Some(virtio_type) = virtio_device_type(&info) else {
            continue;
        };
        allocate_pci_bars(&mut root, device_function, &mut allocator);
        if virtio_type == DeviceType::Block {
            push_block_device(
                config,
                BlockDeviceConfig::Pci(PciDevice {
                    ecam_base: ecam.base,
                    bar_mem_start: bar_start,
                    bar_mem_end: bar_start + bar_size,
                    bus: device_function.bus,
                    device: device_function.device,
                    function: device_function.function,
                    irq: 0,
                }),
            );
        }
    }
}

pub fn init_from_dtb(dtb_addr: usize) {
    let fdt = unsafe { Fdt::from_ptr(dtb_addr as *const u8) }
        .unwrap_or_else(|err| panic!("failed to parse DTB at {:#x}: {:?}", dtb_addr, err));

    let mut config = BoardConfig::empty();
    config.clock_freq = cpu_timer_frequency(&fdt);

    let kernel_end = crate::arch::loongarch64::mm::virt_to_phys(ekernel as usize);
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
    config.memory_end =
        phys_to_virt(memory_region.starting_address as usize + memory_region.size.unwrap_or(0));

    let uart_node = fdt
        .find_compatible(&["ns16550a"])
        .unwrap_or_else(|| panic!("DTB is missing ns16550a UART"));
    config.uart = irq_device(uart_node, "UART");
    let uart_range = MmioRange {
        base: config.uart.base,
        size: config.uart.size,
    };
    push_mmio_region(&mut config, uart_range);

    if let Some(pci_node) = fdt.find_compatible(&["pci-host-ecam-generic"]) {
        discover_pci_blocks(&mut config, pci_node);
    } else {
        warn!("DTB is missing PCI ECAM node; using QEMU fallback");
        let ecam = MmioRange {
            base: phys_to_virt(FALLBACK_PCI_ECAM_BASE),
            size: Cam::Ecam.size() as usize,
        };
        push_mmio_region(&mut config, ecam);
    }

    assert_ne!(config.block_count, 0, "DTB is missing virtio block device");
    assert_ne!(config.uart.base, 0, "DTB is missing uart base");

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
        phys_to_virt(EARLY_UART_BASE)
    }
}

pub fn uart_irq() -> usize {
    board_config().uart.irq
}

pub fn plic_base() -> usize {
    0
}

pub fn block_devices() -> &'static [BlockDeviceConfig] {
    let config = board_config();
    &config.blocks[..config.block_count]
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

pub fn pci_transport(device: PciDevice) -> PciTransport {
    let mut root = PciRoot::new(unsafe { MmioCam::new(device.ecam_base as *mut u8, Cam::Ecam) });
    let device_function = DeviceFunction {
        bus: device.bus,
        device: device.device,
        function: device.function,
    };
    PciTransport::new::<crate::drivers::virtio::VirtioHal, _>(&mut root, device_function)
        .expect("failed to create virtio PCI transport")
}

pub fn device_init(_hart_id: usize) {
    info!("KERN: LoongArch external IRQ setup deferred; block I/O uses polling");
}

pub fn irq_handler() {
    warn!("unexpected LoongArch external IRQ");
}
