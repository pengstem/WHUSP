use crate::drivers::chardev::{CharDevice, UART, UartConfig};
use crate::drivers::plic::{IntrTargetPriority, PLIC};
use crate::drivers::{KEYBOARD_DEVICE, MOUSE_DEVICE};
use core::cell::UnsafeCell;
use core::ptr::NonNull;
use core::sync::atomic::{AtomicBool, Ordering};
use fdt::{Fdt, node::FdtNode};
use log::info;
use riscv::register::sie;
use virtio_drivers::transport::{
    DeviceType, Transport,
    mmio::{MmioTransport, VirtIOHeader},
};

const BLOCK_DEVICE_CAPACITY: usize = 8;
const MMIO_REGION_CAPACITY: usize = 12;
const EARLY_UART_BASE: usize = 0x1000_0000;

pub type BlockDeviceImpl = crate::drivers::block::KernelBlockDevice;
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
    // transport; not read today because RV currently selects the MMIO path.
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
    // CONTEXT: RV constructs only the `Mmio` variant today; LA already
    // constructs `Pci`. Variant kept so the type stays symmetric across arches.
    #[allow(dead_code)]
    Pci(PciDevice),
    StarFiveMmc(IrqDevice),
    RamDisk {
        base: usize,
        size: usize,
    },
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
    uart_register_shift: usize,
    uart_register_width: usize,
    plic: MmioRange,
    blocks: [BlockDeviceConfig; BLOCK_DEVICE_CAPACITY],
    block_count: usize,
    gpu: Option<IrqDevice>,
    keyboard: Option<IrqDevice>,
    mouse: Option<IrqDevice>,
    net: Option<IrqDevice>,
    rtc_base: usize,
    starfive_syscrg: Option<MmioRange>,
    starfive_watchdog: Option<MmioRange>,
    mmio_regions: [MmioRange; MMIO_REGION_CAPACITY],
    mmio_region_count: usize,
    boot_ramdisk: Option<MmioRange>,
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
            uart_register_shift: 0,
            uart_register_width: 1,
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
            rtc_base: 0,
            starfive_syscrg: None,
            starfive_watchdog: None,
            mmio_regions: [MmioRange { base: 0, size: 0 }; MMIO_REGION_CAPACITY],
            mmio_region_count: 0,
            boot_ramdisk: None,
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

fn property_u32(node: FdtNode<'_, '_>, name: &str) -> Option<u32> {
    let value = node.property(name)?.value;
    let bytes: [u8; 4] = value.get(..4)?.try_into().ok()?;
    Some(u32::from_be_bytes(bytes))
}

fn property_address(node: FdtNode<'_, '_>, name: &str) -> Option<usize> {
    let value = node.property(name)?.value;
    match value.len() {
        4 => Some(u32::from_be_bytes(value.try_into().ok()?) as usize),
        8.. => Some(u64::from_be_bytes(value[..8].try_into().ok()?) as usize),
        _ => None,
    }
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

fn push_block_config(config: &mut BoardConfig, value: BlockDeviceConfig) {
    assert!(
        config.block_count < config.blocks.len(),
        "too many block devices discovered in DTB"
    );
    config.blocks[config.block_count] = value;
    config.block_count += 1;
}

fn push_virtio_block_device(config: &mut BoardConfig, value: IrqDevice) {
    push_block_config(config, BlockDeviceConfig::Mmio(value));
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

/// Derives the RISC-V board configuration from QEMU's flattened device tree.
///
/// Device discovery here is the source of truth for memory bounds, interrupt
/// routing, and virtio block order. Keep block ordering stable because mount
/// code treats index 0 as the contest `x0` root disk.
pub fn init_from_dtb(dtb_addr: usize, boot_hw_id: usize) {
    let fdt = unsafe { Fdt::from_ptr(dtb_addr as *const u8) }
        .unwrap_or_else(|err| panic!("failed to parse DTB at {:#x}: {:?}", dtb_addr, err));
    crate::cpu::init_from_dtb(&fdt, boot_hw_id);

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
        .find_compatible(&["ns16550a", "starfive,jh7110-uart", "snps,dw-apb-uart"])
        .unwrap_or_else(|| panic!("DTB is missing a supported 16550-compatible UART"));
    let uart_range = first_reg(uart_node, "UART");
    config.uart = IrqDevice {
        base: uart_range.base,
        size: uart_range.size,
        // CONTEXT: StarFive Release-31's U-Boot control DT does not provide
        // enough interrupt-parent metadata for fdt::FdtNode::interrupts().
        // RISC-V PLIC interrupt specifiers are one u32 cell, so read that cell
        // directly and retain polling-only UART output when it is absent.
        irq: property_u32(uart_node, "interrupts").unwrap_or(0) as usize,
    };
    config.uart_register_shift = property_u32(uart_node, "reg-shift").unwrap_or(0) as usize;
    config.uart_register_width = property_u32(uart_node, "reg-io-width").unwrap_or(1) as usize;
    assert!(
        matches!(config.uart_register_width, 1 | 4),
        "unsupported UART reg-io-width {}",
        config.uart_register_width
    );

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
                push_virtio_block_device(&mut config, device);
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

    let is_starfive = fdt
        .find_node("/")
        .is_some_and(|root| compatible_contains(root, &["starfive,jh7110"]));
    if is_starfive {
        let syscrg = fdt
            .find_compatible(&["starfive,jh7110-syscrg"])
            .map(|node| first_reg(node, "JH7110 SYSCRG"))
            .unwrap_or(MmioRange {
                base: 0x1302_0000,
                size: 0x1_0000,
            });
        let watchdog = fdt
            .find_compatible(&["starfive,jh7110-wdt"])
            .map(|node| first_reg(node, "JH7110 watchdog"))
            .unwrap_or(MmioRange {
                base: 0x1307_0000,
                size: 0x1_0000,
            });
        config.starfive_syscrg = Some(syscrg);
        config.starfive_watchdog = Some(watchdog);
        push_mmio_region(&mut config, syscrg);
        push_mmio_region(&mut config, watchdog);

        let mmc_node = fdt
            .all_nodes()
            .filter(|node| {
                compatible_contains(
                    *node,
                    &[
                        "starfive,jh7110-sdio",
                        "starfive,jh7110-mmc",
                        "snps,dw-mshc",
                    ],
                ) && property_str(*node, "status") != Some("disabled")
            })
            .find(|node| first_reg(*node, "JH7110 MMC").base == 0x1602_0000)
            .unwrap_or_else(|| panic!("DTB is missing the removable JH7110 mmc1 controller"));
        let mmc_range = first_reg(mmc_node, "JH7110 MMC");
        let mmc = IrqDevice {
            base: mmc_range.base,
            size: mmc_range.size,
            // The SDK Release-31 U-Boot control DT omits this interrupt. The
            // current driver deliberately polls command and IDMAC status.
            irq: mmc_node
                .interrupts()
                .and_then(|mut interrupts| interrupts.next())
                .unwrap_or(0),
        };
        push_block_config(&mut config, BlockDeviceConfig::StarFiveMmc(mmc));
        push_device_mmio_region(&mut config, mmc);
    }

    if let Some(chosen) = fdt.find_node("/chosen") {
        let start = property_address(chosen, "linux,initrd-start");
        let end = property_address(chosen, "linux,initrd-end");
        match (start, end) {
            (Some(start), Some(end)) if end > start => {
                let size = end - start;
                push_block_config(
                    &mut config,
                    BlockDeviceConfig::RamDisk { base: start, size },
                );
                config.boot_ramdisk = Some(MmioRange { base: start, size });
            }
            (None, None) => {}
            _ => panic!("DTB has an invalid linux,initrd-start/end range"),
        }
    }

    // QEMU's MMIO addresses encode the contest disk order. Sorting before
    // publishing keeps `block_devices()[0]` aligned with x0 even if DTB node
    // iteration order changes.
    config.blocks[..config.block_count].sort_by_key(|device| match device {
        BlockDeviceConfig::Mmio(device) => device.base,
        BlockDeviceConfig::Pci(device) => {
            ((device.bus as usize) << 16)
                | ((device.device as usize) << 8)
                | device.function as usize
        }
        BlockDeviceConfig::StarFiveMmc(device) => device.base,
        BlockDeviceConfig::RamDisk { .. } => usize::MAX,
    });

    if let Some(rtc_node) = fdt.find_compatible(&["google,goldfish-rtc"]) {
        let rtc_range = first_reg(rtc_node, "goldfish-rtc");
        config.rtc_base = rtc_range.base;
        push_mmio_region(&mut config, rtc_range);
    }

    assert_ne!(
        config.block_count, 0,
        "DTB is missing a supported block device"
    );
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

pub fn boot_ramdisk() -> Option<MmioRange> {
    board_config().boot_ramdisk
}

pub fn uart_base() -> usize {
    if BOARD_CONFIG.initialized.load(Ordering::Acquire) {
        crate::arch::mm::mmio_phys_to_virt(board_config().uart.base)
    } else {
        EARLY_UART_BASE
    }
}

pub fn uart_config() -> UartConfig {
    if BOARD_CONFIG.initialized.load(Ordering::Acquire) {
        let config = board_config();
        UartConfig {
            base_addr: crate::arch::mm::mmio_phys_to_virt(config.uart.base),
            register_shift: config.uart_register_shift,
            register_width: config.uart_register_width,
        }
    } else {
        UartConfig {
            base_addr: EARLY_UART_BASE,
            register_shift: 0,
            register_width: 1,
        }
    }
}

pub fn uart_irq() -> usize {
    board_config().uart.irq
}

pub fn plic_base() -> usize {
    crate::arch::mm::mmio_phys_to_virt(board_config().plic.base)
}

pub fn block_devices() -> &'static [BlockDeviceConfig] {
    let config = board_config();
    &config.blocks[..config.block_count]
}

pub fn block_irq_available() -> bool {
    block_devices().iter().any(|device| match device {
        BlockDeviceConfig::Mmio(device) => device.irq != 0,
        BlockDeviceConfig::Pci(device) => device.irq != 0,
        BlockDeviceConfig::StarFiveMmc(_) | BlockDeviceConfig::RamDisk { .. } => false,
    })
}

fn arm_jh7110_watchdog(half_timeout_ticks: u32) -> Option<(u32, u32)> {
    const CLOCK_ENABLE: u32 = 1 << 31;
    const WDT_APB_CLOCK_ID: usize = 122;
    const WDT_CORE_CLOCK_ID: usize = 123;
    const RESET_ASSERT_OFFSET: usize = 0x2f8;
    const RESET_STATUS_OFFSET: usize = 0x308;
    const WDT_APB_RESET_ID: usize = 109;
    const WDT_CORE_RESET_ID: usize = 110;

    const WDT_LOAD: usize = 0x000;
    const WDT_VALUE: usize = 0x004;
    const WDT_CONTROL: usize = 0x008;
    const WDT_INTCLR: usize = 0x00c;
    const WDT_LOCK: usize = 0xc00;
    const WDT_UNLOCK_KEY: u32 = 0x1acc_e551;
    const WDT_ENABLE: u32 = 1 << 0;
    const WDT_RESET_ENABLE: u32 = 1 << 1;
    fn read32(base: usize, offset: usize) -> u32 {
        unsafe { core::ptr::read_volatile((base + offset) as *const u32) }
    }

    fn write32(base: usize, offset: usize, value: u32) {
        unsafe { core::ptr::write_volatile((base + offset) as *mut u32, value) };
    }

    fn device_barrier() {
        unsafe { core::arch::asm!("fence iorw, iorw") };
    }

    let config = board_config();
    let (Some(syscrg), Some(watchdog)) = (config.starfive_syscrg, config.starfive_watchdog) else {
        return None;
    };
    let syscrg = crate::arch::mm::mmio_phys_to_virt(syscrg.base);
    let watchdog = crate::arch::mm::mmio_phys_to_virt(watchdog.base);

    for clock_id in [WDT_APB_CLOCK_ID, WDT_CORE_CLOCK_ID] {
        let offset = clock_id * core::mem::size_of::<u32>();
        write32(syscrg, offset, read32(syscrg, offset) | CLOCK_ENABLE);
    }
    device_barrier();

    let reset_word = WDT_APB_RESET_ID / 32;
    debug_assert_eq!(reset_word, WDT_CORE_RESET_ID / 32);
    let reset_mask = (1 << (WDT_APB_RESET_ID % 32)) | (1 << (WDT_CORE_RESET_ID % 32));
    let assert_offset = RESET_ASSERT_OFFSET + reset_word * core::mem::size_of::<u32>();
    let status_offset = RESET_STATUS_OFFSET + reset_word * core::mem::size_of::<u32>();
    write32(
        syscrg,
        assert_offset,
        read32(syscrg, assert_offset) & !reset_mask,
    );
    device_barrier();
    let reset_start = crate::timer::get_time_us();
    while read32(syscrg, status_offset) & reset_mask != reset_mask {
        if crate::timer::get_time_us().saturating_sub(reset_start) >= 1_000 {
            log::warn!(
                "JH7110 watchdog reset deassert timed out: status={:#x}",
                read32(syscrg, status_offset)
            );
            return None;
        }
        core::hint::spin_loop();
    }

    write32(watchdog, WDT_LOCK, WDT_UNLOCK_KEY);
    device_barrier();
    let control = read32(watchdog, WDT_CONTROL) & !WDT_ENABLE;
    write32(watchdog, WDT_CONTROL, control | WDT_RESET_ENABLE);
    write32(watchdog, WDT_INTCLR, 1);
    write32(watchdog, WDT_LOAD, half_timeout_ticks);
    write32(
        watchdog,
        WDT_CONTROL,
        control | WDT_RESET_ENABLE | WDT_ENABLE,
    );
    write32(watchdog, WDT_LOCK, !WDT_UNLOCK_KEY);
    device_barrier();

    let first = read32(watchdog, WDT_VALUE);
    let verify_start = crate::timer::get_time_us();
    while crate::timer::get_time_us().saturating_sub(verify_start) < 1_000 {
        core::hint::spin_loop();
    }
    let second = read32(watchdog, WDT_VALUE);
    if first == 0 || second >= first {
        log::warn!("JH7110 watchdog did not count down: first={first:#x}, second={second:#x}");
        return None;
    }
    Some((first, second))
}

/// Arms a boot-progress watchdog for physical-board diagnostic FITs.
///
/// The JH7110 watchdog expires in two phases. Its 24 MHz core clock therefore
/// permits a maximum total timeout of roughly 357 seconds with a u32 load.
#[cfg(feature = "starfive-recovery-watchdog")]
pub fn arm_jh7110_recovery_watchdog(timeout_secs: usize) -> bool {
    const HALF_TIMEOUT_TICKS_PER_SECOND: u64 = 12_000_000;

    let Some(half_timeout_ticks) = (timeout_secs as u64)
        .checked_mul(HALF_TIMEOUT_TICKS_PER_SECOND)
        .and_then(|ticks| u32::try_from(ticks).ok())
    else {
        log::warn!("JH7110 recovery watchdog timeout is out of range: {timeout_secs}s");
        return false;
    };
    let Some((first, second)) = arm_jh7110_watchdog(half_timeout_ticks) else {
        return false;
    };
    info!(
        "JH7110 recovery watchdog armed: first={first:#x}, second={second:#x}, reset_secs={timeout_secs}"
    );
    true
}

/// Arms the JH7110 watchdog as a board-local reboot fallback.
///
/// Release-31 OpenSBI can return from SRST without resetting this board. The
/// watchdog path is selected only when the JH7110 DT nodes were discovered;
/// QEMU therefore continues to use SBI SRST.
pub fn try_jh7110_watchdog_reboot() -> bool {
    // The 24 MHz oscillator feeds WDT_CORE. JH7110 needs two expirations, so
    // this half-count requests a reset about one second after arming.
    let Some((first, second)) = arm_jh7110_watchdog(12_000_000) else {
        return false;
    };
    info!("JH7110 watchdog reboot armed: first={first:#x}, second={second:#x}, reset_ms=1000");
    true
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

pub fn rtc_base() -> usize {
    let base = board_config().rtc_base;
    if base == 0 {
        0
    } else {
        crate::arch::mm::mmio_phys_to_virt(base)
    }
}

pub fn device_init(hart_id: usize) {
    assert_eq!(
        hart_id,
        crate::cpu::external_irq_owner_hardware_id(),
        "RISC-V PLIC initialized for a non-owner hart"
    );
    let mut plic = unsafe { PLIC::new(plic_base()) };
    let supervisor = IntrTargetPriority::Supervisor;
    let machine = IntrTargetPriority::Machine;
    plic.set_threshold(hart_id, supervisor, 0);
    plic.set_threshold(hart_id, machine, 1);

    if uart_irq() != 0 {
        plic.enable(hart_id, supervisor, uart_irq());
        plic.set_priority(uart_irq(), 1);
    }
    for irq in [keyboard_irq(), mouse_irq()].into_iter().flatten() {
        plic.enable(hart_id, supervisor, irq);
        plic.set_priority(irq, 1);
    }
    let block_irq_delivery_enabled = !cfg!(feature = "block-io-force-sync");
    for block in block_devices() {
        let BlockDeviceConfig::Mmio(block) = block else {
            continue;
        };
        if block_irq_delivery_enabled {
            plic.enable(hart_id, supervisor, block.irq);
            plic.set_priority(block.irq, 1);
        } else {
            plic.disable(hart_id, supervisor, block.irq);
        }
    }
    info!(
        "KERN: block completion irq delivery enabled={}",
        block_irq_delivery_enabled
    );

    unsafe {
        sie::set_sext();
    }
}

pub fn irq_handler() {
    let mut plic = unsafe { PLIC::new(plic_base()) };
    // PLIC claim/complete registers are per-hart contexts. Read the current
    // CPU-local hardware ID even while delivery remains fixed on logical CPU0.
    let hart_id = crate::cpu::assert_current_external_irq_owner();
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
    } else if uart_irq != 0 && intr_src_id == uart_irq {
        UART.handle_irq();
    } else if crate::drivers::block::handle_irq(intr_src_id as usize) {
    } else {
        panic!("unsupported IRQ {}", intr_src_id);
    }

    plic.complete(hart_id, IntrTargetPriority::Supervisor, intr_src_id);
}
