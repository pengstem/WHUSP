#[cfg(not(target_arch = "riscv64"))]
use crate::arch::mm::memory_barrier;
use crate::arch::mm::mmio_phys_to_virt;
use crate::board::IrqDevice;
use crate::drivers::block_cache;
use crate::sync::SpinNoIrqLock;
use alloc::vec::Vec;
use core::ptr::{read_volatile, write_volatile};
use log::{info, warn};

const SECTOR_SIZE: usize = 512;
// CONTEXT: SDK Release-31 leaves the SDIO1 CIU source near 198 MHz; its live
// CLKDIV value is 0xf8 for the 400 kHz identification clock. Clock/pinctrl
// programming remains a U-Boot prerequisite for this narrow board path.
const INPUT_CLOCK_HZ: usize = 198_000_000;
const IDENT_CLOCK_HZ: usize = 400_000;
const DEFAULT_CLOCK_HZ: usize = 25_000_000;
const IO_TIMEOUT_US: usize = 2_000_000;
const INIT_TIMEOUT_US: usize = 1_000_000;

const CTRL: usize = 0x000;
const PWREN: usize = 0x004;
const CLKDIV: usize = 0x008;
const CLKSRC: usize = 0x00c;
const CLKENA: usize = 0x010;
const TMOUT: usize = 0x014;
const CTYPE: usize = 0x018;
const BLKSIZ: usize = 0x01c;
const BYTCNT: usize = 0x020;
const INTMASK: usize = 0x024;
const CMDARG: usize = 0x028;
const CMD: usize = 0x02c;
const RESP0: usize = 0x030;
const RESP1: usize = 0x034;
const RESP2: usize = 0x038;
const RESP3: usize = 0x03c;
const RINTSTS: usize = 0x044;
const STATUS: usize = 0x048;
const FIFOTH: usize = 0x04c;
const CDETECT: usize = 0x050;
const BMOD: usize = 0x080;
const UHS_REG: usize = 0x074;
const DATA: usize = 0x200;

const CTRL_RESET: u32 = 1 << 0;
const CTRL_FIFO_RESET: u32 = 1 << 1;
const CTRL_DMA_RESET: u32 = 1 << 2;
const CTRL_INT_ENABLE: u32 = 1 << 4;
const CTRL_DMA_ENABLE: u32 = 1 << 5;
const CTRL_USE_IDMAC: u32 = 1 << 25;
const CTRL_ALL_RESET: u32 = CTRL_RESET | CTRL_FIFO_RESET | CTRL_DMA_RESET;

const CMD_START: u32 = 1 << 31;
const CMD_USE_HOLD: u32 = 1 << 29;
const CMD_UPDATE_CLOCK: u32 = 1 << 21;
const CMD_SEND_INIT: u32 = 1 << 15;
const CMD_STOP: u32 = 1 << 14;
const CMD_PREV_DATA_WAIT: u32 = 1 << 13;
const CMD_WRITE: u32 = 1 << 10;
const CMD_DATA_EXPECTED: u32 = 1 << 9;
const CMD_RESPONSE_CRC: u32 = 1 << 8;
const CMD_LONG_RESPONSE: u32 = 1 << 7;
const CMD_RESPONSE_EXPECTED: u32 = 1 << 6;

const INT_EBE: u32 = 1 << 15;
const INT_SBE: u32 = 1 << 13;
const INT_HLE: u32 = 1 << 12;
const INT_FRUN: u32 = 1 << 11;
const INT_HTO: u32 = 1 << 10;
const INT_DRTO: u32 = 1 << 9;
const INT_RTO: u32 = 1 << 8;
const INT_DCRC: u32 = 1 << 7;
const INT_RCRC: u32 = 1 << 6;
const INT_RXDR: u32 = 1 << 5;
const INT_TXDR: u32 = 1 << 4;
const INT_DATA_OVER: u32 = 1 << 3;
const INT_CMD_DONE: u32 = 1 << 2;
const INT_RESPONSE_ERROR: u32 = 1 << 1;
const COMMAND_ERRORS: u32 = INT_RTO | INT_RCRC | INT_RESPONSE_ERROR | INT_HLE;
const DATA_ERRORS: u32 = INT_EBE | INT_SBE | INT_HLE | INT_FRUN | INT_HTO | INT_DRTO | INT_DCRC;
const STATUS_FIFO_EMPTY: u32 = 1 << 2;
const STATUS_FIFO_FULL: u32 = 1 << 3;
const STATUS_DATA_BUSY: u32 = 1 << 9;
const STATUS_FIFO_COUNT_SHIFT: usize = 17;
const STATUS_FIFO_COUNT_MASK: u32 = 0x1fff;

const MAX_READ_BLOCKS_PER_TRANSFER: usize = 64;
// UNFINISHED: Keep SD writes on the already-validated CMD24 path until the
// CMD25 data-busy and recovery sequence has its own non-destructive fixture.
const MAX_WRITE_BLOCKS_PER_TRANSFER: usize = 1;
const MULTIBLOCK_PROBE_BLOCKS: usize = MAX_READ_BLOCKS_PER_TRANSFER;

#[derive(Clone, Copy)]
enum ResponseKind {
    None,
    Short,
    ShortNoCrc,
    Long,
}

struct MmcState {
    rca: u32,
    high_capacity: bool,
    capacity_blocks: usize,
    max_read_blocks: usize,
}

pub struct Jh7110MmcBlock {
    base_addr: usize,
    mapped_addr: usize,
    cache_key: usize,
    state: SpinNoIrqLock<MmcState>,
}

impl Jh7110MmcBlock {
    #[inline(always)]
    fn device_barrier() {
        #[cfg(target_arch = "riscv64")]
        unsafe {
            core::arch::asm!("fence iorw, iorw");
        }
        #[cfg(not(target_arch = "riscv64"))]
        memory_barrier();
    }

    pub fn new(device: IrqDevice) -> Self {
        let block = Self {
            base_addr: device.base,
            mapped_addr: mmio_phys_to_virt(device.base),
            cache_key: device.base ^ 0x4a48_3731_3130,
            state: SpinNoIrqLock::new(MmcState {
                rca: 0,
                high_capacity: false,
                capacity_blocks: 0,
                max_read_blocks: 1,
            }),
        };
        let (capacity_blocks, high_capacity, max_read_blocks) = block.state.with_lock(|state| {
            block.initialize(state);
            block.probe_multiblock_read(state);
            (
                state.capacity_blocks,
                state.high_capacity,
                state.max_read_blocks,
            )
        });
        info!(
            "JH7110 MMC ready: base={:#x}, sectors={}, high_capacity={}, max_read_blocks={}",
            block.base_addr, capacity_blocks, high_capacity, max_read_blocks,
        );
        block
    }

    fn read_reg(&self, offset: usize) -> u32 {
        unsafe { read_volatile((self.mapped_addr + offset) as *const u32) }
    }

    fn write_reg(&self, offset: usize, value: u32) {
        unsafe { write_volatile((self.mapped_addr + offset) as *mut u32, value) };
    }

    fn wait_reg_clear(&self, offset: usize, mask: u32, timeout_us: usize) -> bool {
        let start = crate::timer::get_time_us();
        while self.read_reg(offset) & mask != 0 {
            if crate::timer::get_time_us().saturating_sub(start) >= timeout_us {
                return false;
            }
            core::hint::spin_loop();
        }
        true
    }

    fn reset_bits(&self, bits: u32) {
        self.write_reg(CTRL, self.read_reg(CTRL) | bits);
        Self::device_barrier();
        assert!(
            self.wait_reg_clear(CTRL, bits, 500_000),
            "JH7110 MMC reset timed out: bits={bits:#x}, ctrl={:#x}",
            self.read_reg(CTRL)
        );
    }

    fn update_clock(&self) {
        self.write_reg(CMDARG, 0);
        Self::device_barrier();
        self.write_reg(CMD, CMD_START | CMD_UPDATE_CLOCK | CMD_PREV_DATA_WAIT);
        Self::device_barrier();
        assert!(
            self.wait_reg_clear(CMD, CMD_START, 500_000),
            "JH7110 MMC clock update timed out"
        );
        let status = self.read_reg(RINTSTS);
        assert_eq!(status & INT_HLE, 0, "JH7110 MMC clock update locked");
        self.write_reg(RINTSTS, status);
    }

    fn set_clock(&self, target_hz: usize) {
        self.write_reg(CLKENA, 0);
        self.update_clock();
        let divisor = INPUT_CLOCK_HZ.div_ceil(target_hz.saturating_mul(2)).max(1);
        self.write_reg(CLKDIV, divisor as u32);
        self.write_reg(CLKSRC, 0);
        self.update_clock();
        self.write_reg(CLKENA, 1);
        self.update_clock();
    }

    fn command_flags(index: u32, response: ResponseKind) -> u32 {
        let mut flags = index | CMD_USE_HOLD;
        match response {
            ResponseKind::None => {}
            ResponseKind::Short => flags |= CMD_RESPONSE_EXPECTED | CMD_RESPONSE_CRC,
            ResponseKind::ShortNoCrc => flags |= CMD_RESPONSE_EXPECTED,
            ResponseKind::Long => {
                flags |= CMD_RESPONSE_EXPECTED | CMD_RESPONSE_CRC | CMD_LONG_RESPONSE
            }
        }
        flags
    }

    fn send_command_raw(
        &self,
        index: u32,
        argument: u32,
        response: ResponseKind,
        extra_flags: u32,
    ) -> Result<[u32; 4], u32> {
        if extra_flags & CMD_PREV_DATA_WAIT != 0
            && !self.wait_reg_clear(STATUS, STATUS_DATA_BUSY, 500_000)
        {
            return Err(INT_DRTO);
        }
        self.write_reg(RINTSTS, u32::MAX);
        self.write_reg(CMDARG, argument);
        // The controller samples CMDARG and the data-path registers when CMD
        // is published. Volatile accesses prevent compiler reordering, while
        // the I/O fence also orders them at the JH7110 interconnect.
        Self::device_barrier();
        self.write_reg(
            CMD,
            CMD_START | Self::command_flags(index, response) | extra_flags,
        );
        Self::device_barrier();
        let start = crate::timer::get_time_us();
        loop {
            let status = self.read_reg(RINTSTS);
            if status & COMMAND_ERRORS != 0 {
                self.write_reg(RINTSTS, status);
                return Err(status);
            }
            if status & INT_CMD_DONE != 0 {
                self.write_reg(RINTSTS, INT_CMD_DONE);
                return Ok([
                    self.read_reg(RESP0),
                    self.read_reg(RESP1),
                    self.read_reg(RESP2),
                    self.read_reg(RESP3),
                ]);
            }
            if crate::timer::get_time_us().saturating_sub(start) >= IO_TIMEOUT_US {
                return Err(self.read_reg(RINTSTS) | INT_RTO);
            }
            core::hint::spin_loop();
        }
    }

    fn send_command(&self, index: u32, argument: u32, response: ResponseKind) -> [u32; 4] {
        self.send_command_raw(index, argument, response, 0)
            .unwrap_or_else(|status| {
                panic!("JH7110 MMC CMD{index} failed: arg={argument:#x}, status={status:#x}")
            })
    }

    fn initialize(&self, state: &mut MmcState) {
        assert_eq!(
            self.read_reg(CDETECT) & 1,
            0,
            "JH7110 removable SD card is not present"
        );
        self.write_reg(PWREN, 1);
        self.reset_bits(CTRL_ALL_RESET);
        self.write_reg(INTMASK, 0);
        self.write_reg(RINTSTS, u32::MAX);
        self.write_reg(BMOD, 0);
        self.write_reg(TMOUT, u32::MAX);
        self.write_reg(FIFOTH, (2 << 28) | (15 << 16) | 16);
        self.write_reg(CTYPE, 0);
        self.write_reg(UHS_REG, 0);
        self.set_clock(IDENT_CLOCK_HZ);

        self.send_command_raw(0, 0, ResponseKind::None, CMD_SEND_INIT | CMD_STOP)
            .unwrap_or_else(|status| panic!("JH7110 MMC CMD0 failed: status={status:#x}"));
        let cmd8_ok = self
            .send_command_raw(8, 0x1aa, ResponseKind::Short, 0)
            .is_ok_and(|response| response[0] & 0xfff == 0x1aa);
        let start = crate::timer::get_time_us();
        let ocr = loop {
            let _ = self.send_command(55, 0, ResponseKind::Short);
            let argument = 0x00ff_8000 | if cmd8_ok { 1 << 30 } else { 0 };
            let response = self.send_command(41, argument, ResponseKind::ShortNoCrc)[0];
            if response & (1 << 31) != 0 {
                break response;
            }
            assert!(
                crate::timer::get_time_us().saturating_sub(start) < INIT_TIMEOUT_US,
                "JH7110 SD card initialization timed out: OCR={response:#x}"
            );
        };
        state.high_capacity = ocr & (1 << 30) != 0;
        let _cid = self.send_command(2, 0, ResponseKind::Long);
        state.rca = self.send_command(3, 0, ResponseKind::Short)[0] & 0xffff_0000;
        let csd = self.send_command(9, state.rca, ResponseKind::Long);
        state.capacity_blocks = Self::capacity_from_csd(csd);
        assert_ne!(
            state.capacity_blocks, 0,
            "JH7110 SD card reported zero capacity"
        );
        let _ = self.send_command(7, state.rca, ResponseKind::Short);
        assert!(
            self.wait_reg_clear(STATUS, STATUS_DATA_BUSY, 500_000),
            "JH7110 SD card remained busy after selection"
        );
        if !state.high_capacity {
            let _ = self.send_command(16, SECTOR_SIZE as u32, ResponseKind::Short);
        }
        let _ = self.send_command(55, state.rca, ResponseKind::Short);
        let _ = self.send_command(6, 2, ResponseKind::Short);
        self.write_reg(CTYPE, 1);
        self.set_clock(DEFAULT_CLOCK_HZ);
        // UNFINISHED: 50 MHz SD high-speed mode also requires programming
        // the JH7110 receive/sample phase. Until that board-specific tuning
        // is implemented, retain the standard 25 MHz mode selected above.
    }

    fn capacity_from_csd(csd: [u32; 4]) -> usize {
        let structure = (csd[3] >> 30) & 0x3;
        if structure == 1 {
            let c_size = ((csd[2] & 0x3f) << 16) | (csd[1] >> 16);
            return (c_size as usize + 1) * 1024;
        }
        let read_bl_len = (csd[2] >> 16) & 0xf;
        let c_size = ((csd[2] & 0x3ff) << 2) | (csd[1] >> 30);
        let c_size_mult = (csd[1] >> 15) & 0x7;
        let bytes = (c_size as usize + 1)
            .checked_mul(1usize << (c_size_mult + 2))
            .and_then(|blocks| blocks.checked_mul(1usize << read_bl_len))
            .expect("JH7110 SD capacity overflow");
        bytes / SECTOR_SIZE
    }

    fn block_argument(state: &MmcState, block_id: usize) -> u32 {
        if state.high_capacity {
            block_id as u32
        } else {
            block_id
                .checked_mul(SECTOR_SIZE)
                .expect("JH7110 byte address overflow") as u32
        }
    }

    fn prepare_multiblock_read(&self, block_count: usize) -> Result<(), u32> {
        const R1_ILLEGAL_COMMAND: u32 = 1 << 22;

        let response = self.send_command_raw(
            23,
            block_count as u32,
            ResponseKind::Short,
            CMD_PREV_DATA_WAIT,
        )?;
        if response[0] & R1_ILLEGAL_COMMAND != 0 {
            return Err(response[0]);
        }
        Ok(())
    }

    fn recover_multiblock_read(&self) {
        let _ = self.send_command_raw(12, 0, ResponseKind::Short, CMD_STOP);
        self.reset_bits(CTRL_FIFO_RESET | CTRL_DMA_RESET);
        self.write_reg(RINTSTS, u32::MAX);
        let _ = self.wait_reg_clear(STATUS, STATUS_DATA_BUSY, 500_000);
    }

    fn transfer_with_retry(
        &self,
        command: u32,
        argument: u32,
        write: bool,
        buffer: &mut [u8],
    ) -> (Result<(), u32>, usize) {
        let mut retries = 0;
        let mut result = self.transfer_data(command, argument, write, SECTOR_SIZE, buffer);
        if result.is_err() && command != 18 {
            retries = 1;
            self.reset_bits(CTRL_FIFO_RESET | CTRL_DMA_RESET);
            result = self.transfer_data(command, argument, write, SECTOR_SIZE, buffer);
        }
        (result, retries)
    }

    /// Proves CMD23 + CMD18 against the existing CMD17 path before enabling it
    /// for filesystem reads. The probe is read-only and leaves writes on CMD24.
    fn probe_multiblock_read(&self, state: &mut MmcState) {
        if state.capacity_blocks < MULTIBLOCK_PROBE_BLOCKS {
            warn!(
                "JH7110 MMC CMD18 probe skipped: sectors={} probe_blocks={MULTIBLOCK_PROBE_BLOCKS}",
                state.capacity_blocks
            );
            return;
        }

        let mut reference = alloc::vec![0u8; MULTIBLOCK_PROBE_BLOCKS * SECTOR_SIZE];
        for (block, sector) in reference.chunks_exact_mut(SECTOR_SIZE).enumerate() {
            let argument = Self::block_argument(state, block);
            let (result, _) = self.transfer_with_retry(17, argument, false, sector);
            if let Err(status) = result {
                warn!("JH7110 MMC CMD18 probe skipped: CMD17 block={block} status={status:#x}");
                return;
            }
        }

        let mut multiple = alloc::vec![0u8; MULTIBLOCK_PROBE_BLOCKS * SECTOR_SIZE];
        if let Err(status) = self.prepare_multiblock_read(MULTIBLOCK_PROBE_BLOCKS) {
            warn!("JH7110 MMC CMD18 probe failed: CMD23 status={status:#x}");
            return;
        }
        let argument = Self::block_argument(state, 0);
        let (result, retries) = self.transfer_with_retry(18, argument, false, &mut multiple);
        if let Err(status) = result {
            self.recover_multiblock_read();
            warn!(
                "JH7110 MMC CMD18 probe failed: blocks={MULTIBLOCK_PROBE_BLOCKS} retries={retries} status={status:#x}; using CMD17"
            );
            return;
        }
        if multiple != reference {
            warn!(
                "JH7110 MMC CMD18 probe failed: blocks={MULTIBLOCK_PROBE_BLOCKS} data_mismatch=true; using CMD17"
            );
            return;
        }

        state.max_read_blocks = MAX_READ_BLOCKS_PER_TRANSFER;
        info!(
            "JH7110 MMC CMD18 probe passed: blocks={MULTIBLOCK_PROBE_BLOCKS}, max_read_blocks={MAX_READ_BLOCKS_PER_TRANSFER}"
        );
    }

    fn transfer_data(
        &self,
        command: u32,
        argument: u32,
        write: bool,
        block_size: usize,
        buffer: &mut [u8],
    ) -> Result<(), u32> {
        let block_size = if block_size == 0 {
            buffer.len()
        } else {
            block_size
        };
        self.reset_bits(CTRL_FIFO_RESET | CTRL_DMA_RESET);
        self.write_reg(BMOD, 0);
        self.write_reg(RINTSTS, u32::MAX);
        self.write_reg(BLKSIZ, block_size as u32);
        self.write_reg(BYTCNT, buffer.len() as u32);
        self.write_reg(
            CTRL,
            (self.read_reg(CTRL) | CTRL_INT_ENABLE) & !(CTRL_DMA_ENABLE | CTRL_USE_IDMAC),
        );
        Self::device_barrier();
        let flags = CMD_DATA_EXPECTED | CMD_PREV_DATA_WAIT | if write { CMD_WRITE } else { 0 };
        self.send_command_raw(command, argument, ResponseKind::Short, flags)?;

        let mut transferred = 0usize;
        let start = crate::timer::get_time_us();
        loop {
            let interrupt = self.read_reg(RINTSTS);
            if interrupt & DATA_ERRORS != 0 {
                self.write_reg(RINTSTS, interrupt);
                return Err(interrupt);
            }

            if !write && interrupt & (INT_RXDR | INT_DATA_OVER) != 0 {
                while transferred < buffer.len() && self.read_reg(STATUS) & STATUS_FIFO_EMPTY == 0 {
                    let bytes = self.read_reg(DATA).to_le_bytes();
                    let count = (buffer.len() - transferred).min(bytes.len());
                    buffer[transferred..transferred + count].copy_from_slice(&bytes[..count]);
                    transferred += count;
                }
                self.write_reg(RINTSTS, INT_RXDR);
            } else if write && interrupt & INT_TXDR != 0 {
                while transferred < buffer.len() && self.read_reg(STATUS) & STATUS_FIFO_FULL == 0 {
                    let count = (buffer.len() - transferred).min(core::mem::size_of::<u32>());
                    let mut bytes = [0u8; core::mem::size_of::<u32>()];
                    bytes[..count].copy_from_slice(&buffer[transferred..transferred + count]);
                    self.write_reg(DATA, u32::from_le_bytes(bytes));
                    transferred += count;
                }
                self.write_reg(RINTSTS, INT_TXDR);
            }

            if interrupt & INT_DATA_OVER != 0 && transferred == buffer.len() {
                self.write_reg(RINTSTS, interrupt);
                return Ok(());
            }
            if crate::timer::get_time_us().saturating_sub(start) >= IO_TIMEOUT_US {
                let fifo_count =
                    self.read_reg(STATUS) >> STATUS_FIFO_COUNT_SHIFT & STATUS_FIFO_COUNT_MASK;
                return Err(interrupt | INT_DRTO | (fifo_count << 16));
            }
            core::hint::spin_loop();
        }
    }

    fn transfer_blocks_uncached(
        &self,
        state: &mut MmcState,
        block_id: usize,
        buffer: &mut [u8],
        write: bool,
    ) {
        assert_eq!(
            buffer.len() % SECTOR_SIZE,
            0,
            "JH7110 MMC I/O is not sector aligned"
        );
        let block_count = buffer.len() / SECTOR_SIZE;
        assert!(
            block_id
                .checked_add(block_count)
                .is_some_and(|end| end <= state.capacity_blocks),
            "JH7110 MMC I/O exceeds card capacity"
        );
        let mut completed = 0usize;
        while completed < block_count {
            let max_blocks = if write {
                MAX_WRITE_BLOCKS_PER_TRANSFER
            } else {
                state.max_read_blocks
            };
            let chunk_blocks = (block_count - completed).min(max_blocks);
            let chunk_len = chunk_blocks * SECTOR_SIZE;
            let first_block = block_id + completed;
            let argument = Self::block_argument(state, first_block);
            let command = match (write, chunk_blocks == 1) {
                (false, true) => 17,
                (false, false) => 18,
                (true, true) => 24,
                (true, false) => 25,
            };
            let chunk = &mut buffer[completed * SECTOR_SIZE..completed * SECTOR_SIZE + chunk_len];
            if command == 18 {
                if let Err(status) = self.prepare_multiblock_read(chunk_blocks) {
                    warn!(
                        "JH7110 MMC CMD23 failed: block={first_block}, blocks={chunk_blocks}, status={status:#x}; disabling CMD18"
                    );
                    state.max_read_blocks = 1;
                    continue;
                }
            }
            let transfer_start_us = crate::timer::get_time_us();
            let (result, retries) = self.transfer_with_retry(command, argument, write, chunk);
            crate::perf::record_jh7110_mmc_transfer(
                command as usize,
                chunk_blocks,
                crate::timer::get_time_us().saturating_sub(transfer_start_us),
                retries,
                result.is_ok(),
            );
            if let Err(status) = result {
                if command == 18 {
                    self.recover_multiblock_read();
                    warn!(
                        "JH7110 MMC CMD18 failed: block={first_block}, blocks={chunk_blocks}, status={status:#x}; falling back to CMD17"
                    );
                    state.max_read_blocks = 1;
                    continue;
                }
                panic!(
                    "JH7110 MMC data transfer failed: cmd={command}, block={first_block}, blocks={chunk_blocks}, write={write}, status={status:#x}"
                );
            }
            completed += chunk_blocks;
        }
    }

    fn read_blocks_uncached(&self, block_id: usize, buf: &mut [u8]) {
        let mut state = self.state.lock();
        self.transfer_blocks_uncached(&mut state, block_id, buf, false);
    }

    fn write_blocks_uncached(&self, block_id: usize, buf: &[u8]) {
        let mut bounce = Vec::from(buf);
        let mut state = self.state.lock();
        self.transfer_blocks_uncached(&mut state, block_id, &mut bounce, true);
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

    pub fn read_blocks_versioned_fill_for_file_plan(
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

    pub fn write_blocks_for_file_plan(
        &self,
        block_id: usize,
        buf: &[u8],
    ) -> block_cache::WriteStats {
        block_cache::write_with_cache(self.cache_key, block_id, buf, |block_id, buf| {
            self.write_blocks_uncached(block_id, buf)
        })
    }

    pub fn num_blocks(&self) -> u64 {
        self.state.lock().capacity_blocks as u64
    }

    pub fn base_addr(&self) -> usize {
        self.base_addr
    }
}
