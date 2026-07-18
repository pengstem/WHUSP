use crate::arch::loongarch64::{irq, mm::phys_to_virt};
use crate::drivers::chardev::{CharDevice, UART};
use crate::drivers::{KEYBOARD_DEVICE, MOUSE_DEVICE};
use core::cell::UnsafeCell;
use core::mem::size_of;
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
const PCI_INTX_PIN_A: usize = 1;
const PCI_INTERRUPT_MAP_ENTRY_CELLS: usize = 7;
const PCI_INTERRUPT_MAP_CHILD_CELLS: usize = 4;
const PCI_INTERRUPT_MAP_PARENT_IRQ_CELL: usize = 5;
const FALLBACK_PCI_INTX_BASE: usize = 0x10;
const FALLBACK_PCI_INTX_COUNT: usize = 4;

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
    // CONTEXT: BAR window captured during DTB scan for future PCI block-device
    // transport; currently set but not read on the LA boot path.
    #[allow(dead_code)]
    pub bar_mem_start: usize,
    #[allow(dead_code)]
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
    eiointc_base: usize,
    eiointc_irq: usize,
    pch_pic: MmioRange,
    blocks: [BlockDeviceConfig; BLOCK_DEVICE_CAPACITY],
    block_count: usize,
    gpu: Option<IrqDevice>,
    keyboard: Option<IrqDevice>,
    mouse: Option<IrqDevice>,
    // CONTEXT: virtio-net DTB node captured for future LA net support; no
    // in-kernel net stack consumes it today.
    #[allow(dead_code)]
    net: Option<IrqDevice>,
    rtc_base: usize,
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
            eiointc_base: 0,
            eiointc_irq: 0,
            pch_pic: MmioRange { base: 0, size: 0 },
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
            rtc_base: 0,
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

fn first_physical_reg(node: FdtNode<'_, '_>, context: &str) -> MmioRange {
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
    let irq = first_irq(node, context);

    IrqDevice {
        base: range.base,
        size: range.size,
        irq,
    }
}

fn first_irq(node: FdtNode<'_, '_>, context: &str) -> usize {
    node.property("interrupts")
        .and_then(|property| property.value.get(0..4))
        .and_then(|bytes| bytes.try_into().ok())
        .map(u32::from_be_bytes)
        .map(|irq| irq as usize)
        .unwrap_or_else(|| panic!("{} node is missing an interrupts property", context))
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

fn read_be_u32(value: &[u8], cell: usize) -> Option<u32> {
    let start = cell.checked_mul(4)?;
    let bytes = value.get(start..start + 4)?;
    Some(u32::from_be_bytes(bytes.try_into().ok()?))
}

fn pci_child_interrupt_cells(device_function: DeviceFunction, pin: usize) -> [u32; 4] {
    let phys_hi = ((device_function.bus as u32) << 16)
        | ((device_function.device as u32) << 11)
        | ((device_function.function as u32) << 8);
    [phys_hi, 0, 0, pin as u32]
}

fn pci_intx_irq_from_map(
    pci_node: FdtNode<'_, '_>,
    device_function: DeviceFunction,
    pin: usize,
) -> Option<usize> {
    let map = pci_node.property("interrupt-map")?;
    let mask = pci_node.property("interrupt-map-mask")?;
    let child = pci_child_interrupt_cells(device_function, pin);
    let mask_cells = [
        read_be_u32(mask.value, 0)?,
        read_be_u32(mask.value, 1)?,
        read_be_u32(mask.value, 2)?,
        read_be_u32(mask.value, 3)?,
    ];

    for entry in map
        .value
        .chunks_exact(PCI_INTERRUPT_MAP_ENTRY_CELLS * size_of::<u32>())
    {
        let mut matches = true;
        for i in 0..PCI_INTERRUPT_MAP_CHILD_CELLS {
            let entry_cell = read_be_u32(entry, i)?;
            if (child[i] & mask_cells[i]) != (entry_cell & mask_cells[i]) {
                matches = false;
                break;
            }
        }
        if matches {
            return read_be_u32(entry, PCI_INTERRUPT_MAP_PARENT_IRQ_CELL).map(|irq| irq as usize);
        }
    }
    None
}

fn fallback_pci_intx_irq(device_function: DeviceFunction) -> usize {
    FALLBACK_PCI_INTX_BASE + (device_function.device as usize % FALLBACK_PCI_INTX_COUNT)
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
            let irq = pci_intx_irq_from_map(pci_node, device_function, PCI_INTX_PIN_A)
                .unwrap_or_else(|| {
                    let irq = fallback_pci_intx_irq(device_function);
                    warn!(
                        "PCI interrupt-map missing BDF {}:{:02x}.{} INTA; using QEMU fallback irq {}",
                        device_function.bus,
                        device_function.device,
                        device_function.function,
                        irq
                    );
                    irq
                });
            push_block_device(
                config,
                BlockDeviceConfig::Pci(PciDevice {
                    ecam_base: ecam.base,
                    bar_mem_start: bar_start,
                    bar_mem_end: bar_start + bar_size,
                    bus: device_function.bus,
                    device: device_function.device,
                    function: device_function.function,
                    irq,
                }),
            );
        }
    }
}

pub fn init_from_dtb(dtb_addr: usize, boot_hw_id: usize) {
    let fdt = unsafe { Fdt::from_ptr(dtb_addr as *const u8) }
        .unwrap_or_else(|err| panic!("failed to parse DTB at {:#x}: {:?}", dtb_addr, err));
    crate::cpu::init_from_dtb(&fdt, boot_hw_id);

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

    let eiointc_node = fdt
        .find_compatible(&["loongson,ls2k2000-eiointc"])
        .unwrap_or_else(|| panic!("DTB is missing LoongArch EIOINTC"));
    config.eiointc_base = first_physical_reg(eiointc_node, "EIOINTC").base;
    config.eiointc_irq = first_irq(eiointc_node, "EIOINTC");

    let pch_pic_node = fdt
        .find_compatible(&["loongson,pch-pic-1.0"])
        .unwrap_or_else(|| panic!("DTB is missing LoongArch PCH PIC"));
    let pch_pic = first_reg(pch_pic_node, "PCH PIC");
    config.pch_pic = pch_pic;
    push_mmio_region(&mut config, pch_pic);

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

    if let Some(rtc_node) = fdt.find_compatible(&["loongson,ls7a-rtc"]) {
        let rtc_range = first_reg(rtc_node, "LS7A RTC");
        config.rtc_base = rtc_range.base;
        push_mmio_region(&mut config, rtc_range);
    }

    assert_ne!(config.block_count, 0, "DTB is missing virtio block device");
    assert_ne!(config.uart.base, 0, "DTB is missing uart base");
    assert_ne!(config.eiointc_base, 0, "DTB is missing EIOINTC base");
    assert_ne!(config.pch_pic.base, 0, "DTB is missing PCH PIC base");

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

pub fn rtc_base() -> usize {
    board_config().rtc_base
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

pub fn external_irq_available() -> bool {
    let config = board_config();
    config.eiointc_base != 0 && config.pch_pic.base != 0
}

pub fn block_irq_available() -> bool {
    external_irq_available()
        && block_devices().iter().any(|device| match device {
            BlockDeviceConfig::Mmio(device) => device.irq != 0,
            BlockDeviceConfig::Pci(device) => device.irq != 0,
        })
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

fn enable_external_irq(eiointc: irq::Eiointc, pch_pic: irq::PchPic, irq: usize) {
    eiointc.enable(irq);
    pch_pic.enable(irq);
}

pub fn device_init(_hart_id: usize) {
    let config = board_config();
    let eiointc = irq::Eiointc::new(config.eiointc_base);
    let pch_pic = irq::PchPic::new(config.pch_pic.base);
    eiointc.init();
    pch_pic.init();

    let uart_irq = uart_irq();
    enable_external_irq(eiointc, pch_pic, uart_irq);
    for block in block_devices() {
        let irq = match block {
            BlockDeviceConfig::Mmio(device) => device.irq,
            BlockDeviceConfig::Pci(device) => device.irq,
        };
        if irq != 0 {
            enable_external_irq(eiointc, pch_pic, irq);
        }
    }

    crate::trap::enable_external_interrupt();
    info!(
        "KERN: LoongArch external IRQ ready: eiointc={:#x}, cpu_irq={}, pch_pic={:#x}, uart_irq={}, block_irq_ready={}",
        config.eiointc_base,
        config.eiointc_irq,
        config.pch_pic.base,
        uart_irq,
        block_irq_available(),
    );
}

pub fn irq_handler() {
    let config = board_config();
    let eiointc = irq::Eiointc::new(config.eiointc_base);
    let Some(intr_src_id) = eiointc.claim() else {
        warn!("spurious LoongArch external IRQ");
        return;
    };

    let uart_irq = uart_irq();
    let keyboard_irq = keyboard_irq();
    let mouse_irq = mouse_irq();
    let handled = if intr_src_id == uart_irq {
        UART.handle_irq();
        true
    } else if keyboard_irq == Some(intr_src_id) {
        if let Some(device) = KEYBOARD_DEVICE.as_ref() {
            device.handle_irq();
            true
        } else {
            false
        }
    } else if mouse_irq == Some(intr_src_id) {
        if let Some(device) = MOUSE_DEVICE.as_ref() {
            device.handle_irq();
            true
        } else {
            false
        }
    } else {
        crate::drivers::block::handle_irq(intr_src_id)
    };

    if !handled {
        warn!("unhandled LoongArch external IRQ {}", intr_src_id);
    }
    eiointc.complete(intr_src_id);
}
