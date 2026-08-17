use crate::board::IrqDevice;
use crate::config::PAGE_SIZE;
use crate::drivers::block_cache;
use crate::mm::{FrameTracker, PhysAddr, frame_alloc_more};
use crate::sync::SpinNoIrqLock;
use alloc::vec::Vec;
use core::hint::spin_loop;
use core::mem::size_of;
use core::ptr::{copy_nonoverlapping, read_volatile, write_bytes, write_volatile};
use log::info;

pub const LS2K1000_AHCI_BASE: usize = 0x400e_0000;
pub const LS2K1000_AHCI_SIZE: usize = 0x1_0000;
pub const LS2K1000_AHCI_IRQ: usize = 19;

const SECTOR_SIZE: usize = 512;
const MAX_BLOCKS_PER_COMMAND: usize = 128;
const TRANSFER_BUFFER_BYTES: usize = MAX_BLOCKS_PER_COMMAND * SECTOR_SIZE;
const PORT0_BASE: usize = 0x100;
const MIN_REGISTER_WINDOW: usize = PORT0_BASE + 0x80;

// The LS2K1000 firmware programs DMW0 as uncached/strongly ordered and DMW1
// as coherent cached memory. MMIO must use DMW0, while ordinary DMA buffers
// use the DMW1 mapping returned by phys_to_virt().
const LOONGARCH_DMW_MASK: usize = 0xf000_0000_0000_0000;
const LOONGARCH_DMW0_UNCACHED: usize = 0x8000_0000_0000_0000;
const LOONGARCH_DMW1_CACHED: usize = 0x9000_0000_0000_0000;
const LOONGARCH_PHYS_MASK: usize = 0x0000_ffff_ffff_ffff;

// HBA registers.
const HOST_CAP: usize = 0x00;
const HOST_GHC: usize = 0x04;
const HOST_IS: usize = 0x08;
const HOST_PI: usize = 0x0c;
const HOST_VS: usize = 0x10;

const GHC_HR: u32 = 1 << 0;
const GHC_IE: u32 = 1 << 1;
const GHC_AE: u32 = 1 << 31;

// Port registers, relative to PORT0_BASE.
const PORT_CLB: usize = 0x00;
const PORT_CLBU: usize = 0x04;
const PORT_FB: usize = 0x08;
const PORT_FBU: usize = 0x0c;
const PORT_IS: usize = 0x10;
const PORT_IE: usize = 0x14;
const PORT_CMD: usize = 0x18;
const PORT_TFD: usize = 0x20;
const PORT_SIG: usize = 0x24;
const PORT_SSTS: usize = 0x28;
const PORT_SCTL: usize = 0x2c;
const PORT_SERR: usize = 0x30;
const PORT_SACT: usize = 0x34;
const PORT_CI: usize = 0x38;

const PORT_CMD_ST: u32 = 1 << 0;
const PORT_CMD_SUD: u32 = 1 << 1;
const PORT_CMD_POD: u32 = 1 << 2;
const PORT_CMD_FRE: u32 = 1 << 4;
const PORT_CMD_FR: u32 = 1 << 14;
const PORT_CMD_CR: u32 = 1 << 15;
const PORT_CMD_ICC_MASK: u32 = 0xf << 28;
const PORT_CMD_ICC_ACTIVE: u32 = 1 << 28;

const PORT_TFD_ERR: u32 = 1 << 0;
const PORT_TFD_DRQ: u32 = 1 << 3;
const PORT_TFD_BSY: u32 = 1 << 7;
const PORT_IS_ERROR_MASK: u32 =
    (1 << 30) | (1 << 29) | (1 << 28) | (1 << 27) | (1 << 26) | (1 << 24) | (1 << 23);

const SATA_DET_MASK: u32 = 0xf;
const SATA_DET_PRESENT: u32 = 0x3;
const SATA_IPM_MASK: u32 = 0xf << 8;
const SATA_IPM_ACTIVE: u32 = 0x1 << 8;
const SATA_SIG_ATA: u32 = 0x0000_0101;

const FIS_TYPE_REG_H2D: u8 = 0x27;
const ATA_CMD_IDENTIFY: u8 = 0xec;
const ATA_CMD_READ_DMA: u8 = 0xc8;
const ATA_CMD_WRITE_DMA: u8 = 0xca;
const ATA_CMD_READ_DMA_EXT: u8 = 0x25;
const ATA_CMD_WRITE_DMA_EXT: u8 = 0x35;

const ENGINE_TIMEOUT_US: usize = 500_000;
const RESET_TIMEOUT_US: usize = 1_000_000;
const LINK_TIMEOUT_US: usize = 1_000_000;
const SPINUP_TIMEOUT_US: usize = 20_000_000;
const IO_TIMEOUT_US: usize = 10_000_000;
const COMRESET_ASSERT_US: usize = 1_000;

const COMMAND_LIST_OFFSET: usize = 0;
const RECEIVED_FIS_OFFSET: usize = 1024;
const COMMAND_TABLE_OFFSET: usize = 1280;
const DISK_HEADER_BLOCKS: usize = 4;
const EXT4_SUPERBLOCK_OFFSET: usize = 1024;
const EXT4_MAGIC_OFFSET: usize = EXT4_SUPERBLOCK_OFFSET + 0x38;
const MBR_SIGNATURE_OFFSET: usize = 510;
const MBR_PARTITION_TABLE_OFFSET: usize = 446;
const MBR_PARTITION_ENTRY_SIZE: usize = 16;
const MBR_PARTITION_COUNT: usize = 4;
const MBR_LINUX_PARTITION_TYPE: u8 = 0x83;

#[repr(C)]
#[derive(Clone, Copy)]
struct CommandHeader {
    flags: u16,
    prdt_length: u16,
    prd_bytes_transferred: u32,
    command_table_base: u32,
    command_table_base_upper: u32,
    reserved: [u32; 4],
}

#[repr(C)]
#[derive(Clone, Copy)]
struct PrdtEntry {
    data_base: u32,
    data_base_upper: u32,
    reserved: u32,
    byte_count_and_interrupt: u32,
}

#[repr(C, align(128))]
struct CommandTable {
    command_fis: [u8; 64],
    atapi_command: [u8; 16],
    reserved: [u8; 48],
    prdt: [PrdtEntry; 1],
}

const _: () = assert!(size_of::<CommandHeader>() == 32);
const _: () = assert!(size_of::<PrdtEntry>() == 16);
const _: () = assert!(size_of::<CommandTable>() == 256);
const _: () = assert!(COMMAND_TABLE_OFFSET + size_of::<CommandTable>() <= PAGE_SIZE);

#[derive(Debug)]
enum AhciError {
    Timeout {
        stage: &'static str,
        command_issue: u32,
        task_file: u32,
        interrupt_status: u32,
        sata_error: u32,
    },
    Port {
        command_issue: u32,
        task_file: u32,
        interrupt_status: u32,
        sata_error: u32,
    },
    LinkDown(u32),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DiskLayout {
    RawExt4,
    MbrLinux { start: u64, sectors: u64 },
}

impl DiskLayout {
    fn start(self) -> u64 {
        match self {
            Self::RawExt4 => 0,
            Self::MbrLinux { start, .. } => start,
        }
    }

    fn sectors(self, disk_sectors: u64) -> u64 {
        match self {
            Self::RawExt4 => disk_sectors,
            Self::MbrLinux { sectors, .. } => sectors,
        }
    }
}

enum TransferBuffer<'a> {
    Read(&'a mut [u8]),
    Write(&'a [u8]),
}

impl TransferBuffer<'_> {
    fn len(&self) -> usize {
        match self {
            Self::Read(buffer) => buffer.len(),
            Self::Write(buffer) => buffer.len(),
        }
    }

    fn is_write(&self) -> bool {
        matches!(self, Self::Write(_))
    }
}

struct DmaRegion {
    _frames: Vec<FrameTracker>,
    physical: usize,
    virtual_address: usize,
    len: usize,
}

impl DmaRegion {
    fn new(pages: usize, purpose: &'static str) -> Self {
        assert_ne!(pages, 0, "AHCI {purpose} DMA allocation is empty");
        let frames = frame_alloc_more(pages).unwrap_or_else(|| {
            panic!("failed to allocate {pages} contiguous AHCI {purpose} pages")
        });
        let first_ppn = frames
            .iter()
            .map(|frame| frame.ppn.0)
            .min()
            .expect("AHCI DMA allocation returned no pages");
        let last_ppn = frames
            .iter()
            .map(|frame| frame.ppn.0)
            .max()
            .expect("AHCI DMA allocation returned no pages");
        assert_eq!(
            last_ppn - first_ppn + 1,
            pages,
            "AHCI DMA allocation is not physically contiguous"
        );
        let physical = PhysAddr::from(crate::mm::PhysPageNum(first_ppn)).0;
        let len = pages
            .checked_mul(PAGE_SIZE)
            .expect("AHCI DMA allocation length overflow");
        let physical_end = physical
            .checked_add(len)
            .expect("AHCI DMA physical range overflow");
        // The board DT describes this controller with dma-mask = <0 0xffffffff>.
        assert!(
            physical_end <= (u32::MAX as usize) + 1,
            "LS2K1000 AHCI {purpose} DMA range exceeds its 32-bit mask: {physical:#x}..{physical_end:#x}"
        );
        Self {
            _frames: frames,
            physical,
            virtual_address: crate::arch::mm::phys_to_virt(physical),
            len,
        }
    }

    fn zero(&mut self) {
        unsafe { write_bytes(self.virtual_address as *mut u8, 0, self.len) };
    }
}

struct AhciState {
    mmio: usize,
    command_dma: DmaRegion,
    transfer_dma: DmaRegion,
    disk_capacity_blocks: u64,
    logical_start_block: u64,
    logical_capacity_blocks: u64,
    lba48: bool,
}

impl AhciState {
    fn new(mmio: usize) -> Self {
        let mut command_dma = DmaRegion::new(1, "command");
        let mut transfer_dma =
            DmaRegion::new(TRANSFER_BUFFER_BYTES.div_ceil(PAGE_SIZE), "transfer");
        command_dma.zero();
        transfer_dma.zero();
        Self {
            mmio,
            command_dma,
            transfer_dma,
            disk_capacity_blocks: 0,
            logical_start_block: 0,
            logical_capacity_blocks: 0,
            lba48: false,
        }
    }

    #[inline(always)]
    fn read_host(&self, offset: usize) -> u32 {
        unsafe { read_volatile((self.mmio + offset) as *const u32) }
    }

    #[inline(always)]
    fn write_host(&self, offset: usize, value: u32) {
        unsafe { write_volatile((self.mmio + offset) as *mut u32, value) };
        crate::arch::mm::memory_barrier();
        let _ = self.read_host(offset);
    }

    #[inline(always)]
    fn read_port(&self, offset: usize) -> u32 {
        self.read_host(PORT0_BASE + offset)
    }

    #[inline(always)]
    fn write_port(&self, offset: usize, value: u32) {
        self.write_host(PORT0_BASE + offset, value);
    }

    fn timeout_error(&self, stage: &'static str) -> AhciError {
        AhciError::Timeout {
            stage,
            command_issue: self.read_port(PORT_CI),
            task_file: self.read_port(PORT_TFD),
            interrupt_status: self.read_port(PORT_IS),
            sata_error: self.read_port(PORT_SERR),
        }
    }

    fn wait_port_clear(
        &self,
        offset: usize,
        mask: u32,
        timeout_us: usize,
        stage: &'static str,
    ) -> Result<(), AhciError> {
        let start = crate::timer::get_time_us();
        while self.read_port(offset) & mask != 0 {
            let now_us = crate::timer::get_time_us();
            if now_us.saturating_sub(start) >= timeout_us {
                return Err(self.timeout_error(stage));
            }
            spin_loop();
        }
        Ok(())
    }

    fn stop_engine(&self) -> Result<(), AhciError> {
        let command = self.read_port(PORT_CMD);
        self.write_port(PORT_CMD, command & !PORT_CMD_ST);
        self.wait_port_clear(
            PORT_CMD,
            PORT_CMD_CR,
            ENGINE_TIMEOUT_US,
            "stop command-list engine",
        )?;
        let command = self.read_port(PORT_CMD);
        self.write_port(PORT_CMD, command & !PORT_CMD_FRE);
        self.wait_port_clear(
            PORT_CMD,
            PORT_CMD_FR,
            ENGINE_TIMEOUT_US,
            "stop received-FIS engine",
        )
    }

    fn start_engine(&self) -> Result<(), AhciError> {
        self.wait_port_clear(
            PORT_CMD,
            PORT_CMD_CR | PORT_CMD_FR,
            ENGINE_TIMEOUT_US,
            "start port engines",
        )?;
        let mut command = self.read_port(PORT_CMD);
        command &= !PORT_CMD_ICC_MASK;
        command |= PORT_CMD_ICC_ACTIVE | PORT_CMD_SUD | PORT_CMD_POD | PORT_CMD_FRE;
        self.write_port(PORT_CMD, command);
        self.write_port(PORT_CMD, command | PORT_CMD_ST);
        Ok(())
    }

    fn wait_link(&self, timeout_us: usize) -> bool {
        let start = crate::timer::get_time_us();
        loop {
            let status = self.read_port(PORT_SSTS);
            if status & SATA_DET_MASK == SATA_DET_PRESENT
                && status & SATA_IPM_MASK == SATA_IPM_ACTIVE
            {
                return true;
            }
            let now_us = crate::timer::get_time_us();
            if now_us.saturating_sub(start) >= timeout_us {
                return false;
            }
            spin_loop();
        }
    }

    fn comreset(&self) -> Result<(), AhciError> {
        let control = self.read_port(PORT_SCTL);
        self.write_port(PORT_SCTL, (control & !SATA_DET_MASK) | 1);
        let start = crate::timer::get_time_us();
        while crate::timer::get_time_us().saturating_sub(start) < COMRESET_ASSERT_US {
            spin_loop();
        }
        self.write_port(PORT_SCTL, control & !SATA_DET_MASK);
        if self.wait_link(LINK_TIMEOUT_US) {
            Ok(())
        } else {
            Err(AhciError::LinkDown(self.read_port(PORT_SSTS)))
        }
    }

    fn reset_hba(&self) -> Result<(), AhciError> {
        // The Synopsys controller used by LS2K1000 has writable CAP/PI setup
        // fields. Preserve the U-Boot-established values around the AHCI reset.
        let capability = self.read_host(HOST_CAP);
        let ports_implemented = self.read_host(HOST_PI);
        self.write_host(HOST_GHC, (self.read_host(HOST_GHC) | GHC_AE) & !GHC_IE);
        self.write_host(HOST_GHC, GHC_AE | GHC_HR);
        let start = crate::timer::get_time_us();
        while self.read_host(HOST_GHC) & GHC_HR != 0 {
            if crate::timer::get_time_us().saturating_sub(start) >= RESET_TIMEOUT_US {
                return Err(self.timeout_error("HBA reset"));
            }
            spin_loop();
        }
        self.write_host(HOST_GHC, GHC_AE);
        self.write_host(HOST_CAP, capability);
        self.write_host(HOST_PI, ports_implemented | 1);
        Ok(())
    }

    fn initialize(&mut self) -> Result<(), AhciError> {
        self.reset_hba()?;
        self.stop_engine()?;
        self.write_port(PORT_IE, 0);
        self.write_port(PORT_IS, u32::MAX);
        self.write_port(PORT_SERR, u32::MAX);
        self.write_host(HOST_IS, 1);

        if !self.wait_link(LINK_TIMEOUT_US) {
            self.comreset()?;
        }

        let command_list = self.command_dma.physical + COMMAND_LIST_OFFSET;
        let received_fis = self.command_dma.physical + RECEIVED_FIS_OFFSET;
        self.write_port(PORT_CLB, command_list as u32);
        self.write_port(PORT_CLBU, 0);
        self.write_port(PORT_FB, received_fis as u32);
        self.write_port(PORT_FBU, 0);
        self.start_engine()?;

        self.wait_port_clear(
            PORT_TFD,
            PORT_TFD_BSY | PORT_TFD_DRQ,
            SPINUP_TIMEOUT_US,
            "device spin-up",
        )?;
        let signature = self.read_port(PORT_SIG);
        if signature != SATA_SIG_ATA {
            return Err(AhciError::Port {
                command_issue: self.read_port(PORT_CI),
                task_file: self.read_port(PORT_TFD),
                interrupt_status: self.read_port(PORT_IS),
                sata_error: self.read_port(PORT_SERR) | signature,
            });
        }
        Ok(())
    }

    fn prepare_command(&mut self, fis: [u8; 20], len: usize, write: bool) {
        assert!((1..=TRANSFER_BUFFER_BYTES).contains(&len));
        assert_eq!(
            len % SECTOR_SIZE,
            0,
            "AHCI DMA length is not sector aligned"
        );

        let header_ptr =
            (self.command_dma.virtual_address + COMMAND_LIST_OFFSET) as *mut CommandHeader;
        let table_ptr =
            (self.command_dma.virtual_address + COMMAND_TABLE_OFFSET) as *mut CommandTable;
        unsafe {
            write_bytes(table_ptr.cast::<u8>(), 0, size_of::<CommandTable>());
            let table = &mut *table_ptr;
            table.command_fis[..fis.len()].copy_from_slice(&fis);
            table.prdt[0] = PrdtEntry {
                data_base: self.transfer_dma.physical as u32,
                data_base_upper: 0,
                reserved: 0,
                byte_count_and_interrupt: (len as u32 - 1) & 0x003f_ffff,
            };
            write_volatile(
                header_ptr,
                CommandHeader {
                    flags: 5 | if write { 1 << 6 } else { 0 },
                    prdt_length: 1,
                    prd_bytes_transferred: 0,
                    command_table_base: (self.command_dma.physical + COMMAND_TABLE_OFFSET) as u32,
                    command_table_base_upper: 0,
                    reserved: [0; 4],
                },
            );
        }
        crate::arch::mm::memory_barrier();
    }

    fn execute_prepared(&self) -> Result<(), AhciError> {
        self.wait_port_clear(
            PORT_TFD,
            PORT_TFD_BSY | PORT_TFD_DRQ,
            IO_TIMEOUT_US,
            "wait for ATA task file",
        )?;
        self.wait_port_clear(PORT_CI, 1, IO_TIMEOUT_US, "wait for command slot 0")?;
        if self.read_port(PORT_SACT) & 1 != 0 {
            return Err(self.timeout_error("NCQ slot 0 remained active"));
        }

        self.write_port(PORT_IS, u32::MAX);
        self.write_port(PORT_SERR, u32::MAX);
        self.write_host(HOST_IS, 1);
        crate::arch::mm::memory_barrier();
        self.write_port(PORT_CI, 1);

        let start = crate::timer::get_time_us();
        while self.read_port(PORT_CI) & 1 != 0 {
            let interrupt_status = self.read_port(PORT_IS);
            if interrupt_status & PORT_IS_ERROR_MASK != 0 {
                return Err(AhciError::Port {
                    command_issue: self.read_port(PORT_CI),
                    task_file: self.read_port(PORT_TFD),
                    interrupt_status,
                    sata_error: self.read_port(PORT_SERR),
                });
            }
            let now_us = crate::timer::get_time_us();
            if now_us.saturating_sub(start) >= IO_TIMEOUT_US {
                return Err(self.timeout_error("ATA command completion"));
            }
            spin_loop();
        }
        crate::arch::mm::memory_barrier();
        let command_issue = self.read_port(PORT_CI);
        let task_file = self.read_port(PORT_TFD);
        let interrupt_status = self.read_port(PORT_IS);
        if task_file & PORT_TFD_ERR != 0 || interrupt_status & PORT_IS_ERROR_MASK != 0 {
            return Err(AhciError::Port {
                command_issue,
                task_file,
                interrupt_status,
                sata_error: self.read_port(PORT_SERR),
            });
        }
        self.write_port(PORT_IS, interrupt_status);
        self.write_host(HOST_IS, 1);
        Ok(())
    }

    fn execute_data_command(
        &mut self,
        fis: [u8; 20],
        len: usize,
        write: bool,
    ) -> Result<(), AhciError> {
        self.prepare_command(fis, len, write);
        self.execute_prepared()
    }

    fn identify(&mut self) -> Result<(), AhciError> {
        let mut fis = [0u8; 20];
        fis[0] = FIS_TYPE_REG_H2D;
        fis[1] = 1 << 7;
        fis[2] = ATA_CMD_IDENTIFY;
        self.execute_data_command(fis, SECTOR_SIZE, false)?;

        let identify = unsafe {
            core::slice::from_raw_parts(self.transfer_dma.virtual_address as *const u8, SECTOR_SIZE)
        };
        let word =
            |index: usize| u16::from_le_bytes([identify[index * 2], identify[index * 2 + 1]]);
        assert_ne!(
            word(49) & (1 << 9),
            0,
            "LS2K1000 SATA disk lacks LBA support"
        );

        let word106 = word(106);
        if word106 & 0xc000 == 0x4000 && word106 & (1 << 12) != 0 {
            let logical_words = u32::from(word(117)) | u32::from(word(118)) << 16;
            assert_eq!(
                logical_words, 256,
                "LS2K1000 AHCI supports only 512-byte logical sectors"
            );
        }

        let word83 = word(83);
        self.lba48 = word83 & 0xc000 == 0x4000 && word83 & (1 << 10) != 0;
        self.disk_capacity_blocks = if self.lba48 {
            u64::from(word(100))
                | u64::from(word(101)) << 16
                | u64::from(word(102)) << 32
                | u64::from(word(103)) << 48
        } else {
            u64::from(word(60)) | u64::from(word(61)) << 16
        };
        assert_ne!(
            self.disk_capacity_blocks, 0,
            "LS2K1000 SATA disk reported zero capacity"
        );
        Ok(())
    }

    fn detect_disk_layout(&mut self) -> DiskLayout {
        let mut header = [0u8; DISK_HEADER_BLOCKS * SECTOR_SIZE];
        self.transfer_physical_blocks(0, TransferBuffer::Read(&mut header));
        parse_disk_layout(&header, self.disk_capacity_blocks).unwrap_or_else(|| {
            panic!(
                "LS2K1000 SATA disk is neither raw ext4 nor an MBR disk with a valid Linux 0x83 partition"
            )
        })
    }

    fn configure_disk_layout(&mut self) -> DiskLayout {
        let layout = self.detect_disk_layout();
        self.logical_start_block = layout.start();
        self.logical_capacity_blocks = layout.sectors(self.disk_capacity_blocks);
        layout
    }

    fn make_rw_fis(&self, lba: u64, blocks: usize, write: bool) -> [u8; 20] {
        assert!((1..=MAX_BLOCKS_PER_COMMAND).contains(&blocks));
        let mut fis = [0u8; 20];
        fis[0] = FIS_TYPE_REG_H2D;
        fis[1] = 1 << 7;
        fis[7] = 1 << 6;
        fis[4] = lba as u8;
        fis[5] = (lba >> 8) as u8;
        fis[6] = (lba >> 16) as u8;
        if self.lba48 {
            fis[2] = if write {
                ATA_CMD_WRITE_DMA_EXT
            } else {
                ATA_CMD_READ_DMA_EXT
            };
            fis[8] = (lba >> 24) as u8;
            fis[9] = (lba >> 32) as u8;
            fis[10] = (lba >> 40) as u8;
            fis[12] = blocks as u8;
            fis[13] = (blocks >> 8) as u8;
        } else {
            assert!(
                lba + blocks as u64 <= 1 << 28,
                "28-bit ATA command exceeds the LBA28 range"
            );
            fis[2] = if write {
                ATA_CMD_WRITE_DMA
            } else {
                ATA_CMD_READ_DMA
            };
            fis[7] |= ((lba >> 24) as u8) & 0x0f;
            fis[12] = blocks as u8;
        }
        fis
    }

    fn transfer_physical_blocks(&mut self, block_id: u64, mut buffer: TransferBuffer<'_>) {
        let len = buffer.len();
        assert_eq!(
            len % SECTOR_SIZE,
            0,
            "LS2K1000 AHCI I/O is not sector aligned"
        );
        let blocks = len / SECTOR_SIZE;
        assert!(
            block_id
                .checked_add(blocks as u64)
                .is_some_and(|end| end <= self.disk_capacity_blocks),
            "LS2K1000 AHCI physical I/O exceeds disk capacity"
        );
        let write = buffer.is_write();

        let mut completed = 0usize;
        while completed < blocks {
            let chunk_blocks = (blocks - completed).min(MAX_BLOCKS_PER_COMMAND);
            let chunk_len = chunk_blocks * SECTOR_SIZE;
            let chunk_start = completed * SECTOR_SIZE;
            if let TransferBuffer::Write(source) = &buffer {
                unsafe {
                    copy_nonoverlapping(
                        source[chunk_start..chunk_start + chunk_len].as_ptr(),
                        self.transfer_dma.virtual_address as *mut u8,
                        chunk_len,
                    )
                };
            }
            let lba = block_id + completed as u64;
            let fis = self.make_rw_fis(lba, chunk_blocks, write);
            self.execute_data_command(
                fis,
                chunk_len,
                write,
            )
                .unwrap_or_else(|error| {
                    panic!(
                        "LS2K1000 AHCI transfer failed: lba={lba}, blocks={chunk_blocks}, write={write}, error={error:?}"
                    )
                });
            if let TransferBuffer::Read(destination) = &mut buffer {
                unsafe {
                    copy_nonoverlapping(
                        self.transfer_dma.virtual_address as *const u8,
                        destination[chunk_start..chunk_start + chunk_len].as_mut_ptr(),
                        chunk_len,
                    )
                };
            }
            completed += chunk_blocks;
        }
    }

    fn transfer_logical_blocks(&mut self, block_id: usize, buffer: TransferBuffer<'_>) {
        let blocks = buffer.len() / SECTOR_SIZE;
        let logical_block = block_id as u64;
        assert!(
            logical_block
                .checked_add(blocks as u64)
                .is_some_and(|end| end <= self.logical_capacity_blocks),
            "LS2K1000 AHCI logical I/O exceeds exposed filesystem capacity"
        );
        let physical_block = self
            .logical_start_block
            .checked_add(logical_block)
            .expect("LS2K1000 AHCI partition offset overflow");
        self.transfer_physical_blocks(physical_block, buffer);
    }
}

pub struct Ls2kAhciBlock {
    state: SpinNoIrqLock<AhciState>,
    base_addr: usize,
    irq: usize,
    capacity_blocks: u64,
    cache_key: usize,
}

impl Ls2kAhciBlock {
    pub fn new(device: IrqDevice) -> Self {
        assert!(
            device.size >= MIN_REGISTER_WINDOW,
            "LS2K1000 AHCI register window is too small: {:#x}",
            device.size
        );
        let base_addr = physical_address(device.base);
        assert_eq!(
            base_addr, LS2K1000_AHCI_BASE,
            "unexpected LS2K1000 AHCI physical base"
        );
        let mmio = LOONGARCH_DMW0_UNCACHED | base_addr;
        let mut state = AhciState::new(mmio);
        state
            .initialize()
            .unwrap_or_else(|error| panic!("LS2K1000 AHCI initialization failed: {error:?}"));
        state
            .identify()
            .unwrap_or_else(|error| panic!("LS2K1000 AHCI IDENTIFY failed: {error:?}"));
        let layout = state.configure_disk_layout();
        let capacity_blocks = state.logical_capacity_blocks;
        let (layout_name, layout_start) = match layout {
            DiskLayout::RawExt4 => ("raw-ext4", 0),
            DiskLayout::MbrLinux { start, .. } => ("mbr-linux-0x83", start),
        };
        info!(
            "LS2K1000 AHCI ready: base={base_addr:#x}, version={:#x}, disk_sectors={}, layout={}, logical_start={}, logical_sectors={}, lba48={}, max_blocks_per_command={}",
            state.read_host(HOST_VS),
            state.disk_capacity_blocks,
            layout_name,
            layout_start,
            capacity_blocks,
            state.lba48,
            MAX_BLOCKS_PER_COMMAND,
        );
        Self {
            state: SpinNoIrqLock::new(state),
            base_addr,
            irq: device.irq,
            capacity_blocks,
            cache_key: base_addr ^ (layout_start as usize).rotate_left(23) ^ 0x4c53_324b_4148_4349,
        }
    }

    fn read_blocks_uncached(&self, block_id: usize, buf: &mut [u8]) {
        self.state
            .with_lock(|state| state.transfer_logical_blocks(block_id, TransferBuffer::Read(buf)));
    }

    fn write_blocks_uncached(&self, block_id: usize, buf: &[u8]) {
        self.state
            .with_lock(|state| state.transfer_logical_blocks(block_id, TransferBuffer::Write(buf)));
    }

    pub fn read_blocks(&self, block_id: usize, buf: &mut [u8]) {
        block_cache::read_with_cache(self.cache_key, block_id, buf, |block_id, buf| {
            self.read_blocks_uncached(block_id, buf)
        });
    }

    pub fn write_blocks(&self, block_id: usize, buf: &[u8]) {
        block_cache::write_with_cache(self.cache_key, block_id, buf, |block_id, buf| {
            self.write_blocks_uncached(block_id, buf)
        });
    }

    pub(crate) fn read_blocks_versioned_fill_for_file_plan(
        &self,
        block_id: usize,
        buf: &mut [u8],
    ) -> block_cache::VersionedReadStats {
        block_cache::read_with_cache_versioned_fill(
            self.cache_key,
            block_id,
            buf,
            |block_id, buf| self.read_blocks_uncached(block_id, buf),
        )
    }

    pub(crate) fn write_blocks_for_file_plan(
        &self,
        block_id: usize,
        buf: &[u8],
    ) -> block_cache::WriteStats {
        block_cache::write_with_cache(self.cache_key, block_id, buf, |block_id, buf| {
            self.write_blocks_uncached(block_id, buf)
        })
    }

    pub fn num_blocks(&self) -> u64 {
        self.capacity_blocks
    }

    pub fn irq(&self) -> usize {
        self.irq
    }

    pub fn base_addr(&self) -> usize {
        self.base_addr
    }
}

fn physical_address(address: usize) -> usize {
    match address & LOONGARCH_DMW_MASK {
        LOONGARCH_DMW0_UNCACHED | LOONGARCH_DMW1_CACHED => address & LOONGARCH_PHYS_MASK,
        _ => address,
    }
}

fn parse_disk_layout(header: &[u8], disk_sectors: u64) -> Option<DiskLayout> {
    if header.len() > EXT4_MAGIC_OFFSET + 1
        && u16::from_le_bytes([header[EXT4_MAGIC_OFFSET], header[EXT4_MAGIC_OFFSET + 1]]) == 0xef53
    {
        return Some(DiskLayout::RawExt4);
    }
    if header.get(MBR_SIGNATURE_OFFSET..MBR_SIGNATURE_OFFSET + 2) != Some(&[0x55, 0xaa]) {
        return None;
    }
    for index in 0..MBR_PARTITION_COUNT {
        let offset = MBR_PARTITION_TABLE_OFFSET + index * MBR_PARTITION_ENTRY_SIZE;
        let entry = header.get(offset..offset + MBR_PARTITION_ENTRY_SIZE)?;
        if entry[4] != MBR_LINUX_PARTITION_TYPE {
            continue;
        }
        let start = u64::from(u32::from_le_bytes(entry[8..12].try_into().ok()?));
        let sectors = u64::from(u32::from_le_bytes(entry[12..16].try_into().ok()?));
        if start == 0 || sectors == 0 {
            continue;
        }
        if start
            .checked_add(sectors)
            .is_some_and(|end| end <= disk_sectors)
        {
            return Some(DiskLayout::MbrLinux { start, sectors });
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raw_ext4_takes_precedence_over_partition_bytes() {
        let mut header = [0u8; DISK_HEADER_BLOCKS * SECTOR_SIZE];
        header[EXT4_MAGIC_OFFSET..EXT4_MAGIC_OFFSET + 2].copy_from_slice(&0xef53u16.to_le_bytes());
        header[MBR_SIGNATURE_OFFSET..MBR_SIGNATURE_OFFSET + 2].copy_from_slice(&[0x55, 0xaa]);
        assert_eq!(
            parse_disk_layout(&header, 10_000),
            Some(DiskLayout::RawExt4)
        );
    }

    #[test]
    fn finds_first_valid_linux_mbr_partition() {
        let mut header = [0u8; DISK_HEADER_BLOCKS * SECTOR_SIZE];
        header[MBR_SIGNATURE_OFFSET..MBR_SIGNATURE_OFFSET + 2].copy_from_slice(&[0x55, 0xaa]);
        let entry = &mut header
            [MBR_PARTITION_TABLE_OFFSET..MBR_PARTITION_TABLE_OFFSET + MBR_PARTITION_ENTRY_SIZE];
        entry[4] = MBR_LINUX_PARTITION_TYPE;
        entry[8..12].copy_from_slice(&4_194_367u32.to_le_bytes());
        entry[12..16].copy_from_slice(&2_000_000u32.to_le_bytes());
        assert_eq!(
            parse_disk_layout(&header, 8_000_000),
            Some(DiskLayout::MbrLinux {
                start: 4_194_367,
                sectors: 2_000_000,
            })
        );
    }

    #[test]
    fn rejects_partition_outside_physical_disk() {
        let mut header = [0u8; DISK_HEADER_BLOCKS * SECTOR_SIZE];
        header[MBR_SIGNATURE_OFFSET..MBR_SIGNATURE_OFFSET + 2].copy_from_slice(&[0x55, 0xaa]);
        let entry = &mut header
            [MBR_PARTITION_TABLE_OFFSET..MBR_PARTITION_TABLE_OFFSET + MBR_PARTITION_ENTRY_SIZE];
        entry[4] = MBR_LINUX_PARTITION_TYPE;
        entry[8..12].copy_from_slice(&9_000u32.to_le_bytes());
        entry[12..16].copy_from_slice(&2_000u32.to_le_bytes());
        assert_eq!(parse_disk_layout(&header, 10_000), None);
    }
}
