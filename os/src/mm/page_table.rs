use super::{FrameTracker, PhysAddr, PhysPageNum, VirtAddr, VirtPageNum, frame_alloc};
use crate::arch::mm as arch_mm;
use alloc::vec;
use alloc::vec::Vec;
use bitflags::*;

bitflags! {
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct PTEFlags: usize {
        const V = 1 << 0;
        const R = 1 << 1;
        const W = 1 << 2;
        const X = 1 << 3;
        const U = 1 << 4;
        const G = 1 << 5;
        const A = 1 << 6;
        const D = 1 << 7;
        const COW = 1 << 8;
    }
}

#[derive(Copy, Clone)]
#[repr(C)]
pub struct PageTableEntry {
    pub bits: usize,
}

impl PageTableEntry {
    pub fn new(ppn: PhysPageNum, flags: PTEFlags) -> Self {
        PageTableEntry {
            bits: arch_mm::pte_new_bits(ppn.0, flags),
        }
    }
    pub fn empty() -> Self {
        PageTableEntry { bits: 0 }
    }
    pub fn ppn(&self) -> PhysPageNum {
        arch_mm::pte_ppn(self.bits).into()
    }
    pub fn flags(&self) -> PTEFlags {
        arch_mm::pte_flags(self.bits)
    }
    pub fn is_valid(&self) -> bool {
        arch_mm::pte_is_valid(self.bits)
    }
    pub fn readable(&self) -> bool {
        self.flags().contains(PTEFlags::R)
    }
    pub fn writable(&self) -> bool {
        self.flags().contains(PTEFlags::W)
    }
    pub fn executable(&self) -> bool {
        self.flags().contains(PTEFlags::X)
    }
    pub fn cow(&self) -> bool {
        self.flags().contains(PTEFlags::COW)
    }
}

pub struct PageTable {
    root_ppn: PhysPageNum,
    frames: Vec<FrameTracker>,
}

// CONTEXT: `try_new`/`try_map` are the recoverable allocation API for user
// memory paths. The panic-style helpers below are used only where the caller
// has already committed to a kernel mapping invariant; failure there is a
// kernel bug or an unrecoverable boot-path allocation failure.
impl PageTable {
    pub fn new() -> Self {
        let frame = frame_alloc().expect("page table root allocation requires a free frame");
        PageTable {
            root_ppn: frame.ppn,
            frames: vec![frame],
        }
    }
    pub fn try_new() -> Option<Self> {
        let frame = frame_alloc()?;
        Some(PageTable {
            root_ppn: frame.ppn,
            frames: vec![frame],
        })
    }
    /// Builds a non-owning view over an existing page-table token.
    ///
    /// `frames` stays empty so dropping this wrapper never frees page-table
    /// pages; syscall copy helpers use it only to translate user addresses.
    pub fn from_token(satp: usize) -> Self {
        Self {
            root_ppn: PhysPageNum::from(arch_mm::page_table_root_ppn(satp)),
            frames: Vec::new(),
        }
    }
    fn find_pte_create(&mut self, vpn: VirtPageNum) -> Option<&mut PageTableEntry> {
        let idxs = vpn.indexes();
        let mut ppn = self.root_ppn;
        let mut result: Option<&mut PageTableEntry> = None;
        for (i, idx) in idxs.iter().enumerate() {
            let pte = &mut ppn.get_pte_array()[*idx];
            if i == 2 {
                result = Some(pte);
                break;
            }
            if !pte.is_valid() {
                let frame = frame_alloc()?;
                *pte = PageTableEntry::new(frame.ppn, PTEFlags::V);
                self.frames.push(frame);
            }
            ppn = pte.ppn();
        }
        result
    }
    fn find_pte(&self, vpn: VirtPageNum) -> Option<&mut PageTableEntry> {
        let idxs = vpn.indexes();
        let mut ppn = self.root_ppn;
        let mut result: Option<&mut PageTableEntry> = None;
        for (i, idx) in idxs.iter().enumerate() {
            let pte = &mut ppn.get_pte_array()[*idx];
            if i == 2 {
                result = Some(pte);
                break;
            }
            if !pte.is_valid() {
                return None;
            }
            ppn = pte.ppn();
        }
        result
    }
    #[allow(unused)]
    pub fn map(&mut self, vpn: VirtPageNum, ppn: PhysPageNum, flags: PTEFlags) {
        let pte = self
            .find_pte_create(vpn)
            .expect("map requires intermediate page-table allocation to succeed");
        assert!(pte.bits == 0, "vpn {vpn:?} is mapped before mapping");
        let leaf_flags = PTEFlags::R | PTEFlags::W | PTEFlags::X;
        let flags = if flags.intersects(leaf_flags) {
            flags | PTEFlags::V
        } else {
            flags
        };
        *pte = PageTableEntry::new(ppn, flags);
    }
    pub fn try_map(&mut self, vpn: VirtPageNum, ppn: PhysPageNum, flags: PTEFlags) -> bool {
        let Some(pte) = self.find_pte_create(vpn) else {
            return false;
        };
        if pte.bits != 0 {
            return false;
        }
        let leaf_flags = PTEFlags::R | PTEFlags::W | PTEFlags::X;
        let flags = if flags.intersects(leaf_flags) {
            flags | PTEFlags::V
        } else {
            flags
        };
        *pte = PageTableEntry::new(ppn, flags);
        true
    }
    #[allow(unused)]
    pub fn unmap(&mut self, vpn: VirtPageNum) {
        let pte = self
            .find_pte(vpn)
            .expect("unmap requires an existing page-table path");
        assert!(
            pte.is_valid() || pte.bits != 0,
            "vpn {vpn:?} is invalid before unmapping"
        );
        *pte = PageTableEntry::empty();
    }
    pub fn remap_flags(&mut self, vpn: VirtPageNum, flags: PTEFlags) -> bool {
        let Some(pte) = self.find_pte(vpn) else {
            return false;
        };
        if !pte.is_valid() && pte.bits == 0 {
            return false;
        }
        let leaf_flags = PTEFlags::R | PTEFlags::W | PTEFlags::X;
        let flags = if flags.intersects(leaf_flags) {
            flags | PTEFlags::V
        } else {
            flags
        };
        *pte = PageTableEntry::new(pte.ppn(), flags);
        true
    }
    pub fn mark_cow_readonly(&mut self, vpn: VirtPageNum) -> bool {
        let Some(pte) = self.find_pte(vpn) else {
            return false;
        };
        if !pte.is_valid() || pte.bits == 0 {
            return false;
        }
        if pte.cow() && !pte.writable() {
            return true;
        }
        if !pte.writable() {
            return false;
        }
        let mut flags = pte.flags();
        flags.remove(PTEFlags::W);
        flags.insert(PTEFlags::COW);
        *pte = PageTableEntry::new(pte.ppn(), flags);
        true
    }
    pub fn restore_write_clear_cow(&mut self, vpn: VirtPageNum) -> bool {
        let Some(pte) = self.find_pte(vpn) else {
            return false;
        };
        if !pte.is_valid() || pte.bits == 0 || !pte.cow() {
            return false;
        }
        let mut flags = pte.flags();
        flags.remove(PTEFlags::COW);
        flags.insert(PTEFlags::W);
        *pte = PageTableEntry::new(pte.ppn(), flags);
        true
    }
    pub fn replace_leaf(&mut self, vpn: VirtPageNum, ppn: PhysPageNum, flags: PTEFlags) -> bool {
        let Some(pte) = self.find_pte(vpn) else {
            return false;
        };
        if !pte.is_valid() || pte.bits == 0 {
            return false;
        }
        let leaf_flags = PTEFlags::R | PTEFlags::W | PTEFlags::X;
        let flags = if flags.intersects(leaf_flags) {
            flags | PTEFlags::V
        } else {
            flags
        };
        *pte = PageTableEntry::new(ppn, flags);
        true
    }
    pub fn translate(&self, vpn: VirtPageNum) -> Option<PageTableEntry> {
        self.find_pte(vpn).map(|pte| *pte)
    }
    pub fn translate_va(&self, va: VirtAddr) -> Option<PhysAddr> {
        self.find_pte(va.clone().floor()).map(|pte| {
            let aligned_pa: PhysAddr = pte.ppn().into();
            let offset = va.page_offset();
            let aligned_pa_usize: usize = aligned_pa.into();
            (aligned_pa_usize + offset).into()
        })
    }
    pub fn token(&self) -> usize {
        arch_mm::page_table_token(self.root_ppn.0)
    }
}

// CONTEXT: most syscall copy paths use checked byte-buffer helpers now. Keep
// this segmented buffer type for legacy in-kernel adapters that still iterate
// translated slices directly.
pub struct UserBuffer {
    pub buffers: Vec<&'static mut [u8]>,
}

impl UserBuffer {
    pub fn new(buffers: Vec<&'static mut [u8]>) -> Self {
        Self { buffers }
    }
    /// Wraps a kernel-owned slice for synchronous in-kernel File trait I/O.
    ///
    /// The returned buffer must be consumed immediately and must not be stored
    /// by the callee. It exists for legacy File::read/write adapters that still
    /// use UserBuffer as their byte carrier even when the source is kernel memory.
    pub fn from_kernel_slice_for_sync_io(buf: &mut [u8]) -> Self {
        let slice = unsafe { core::mem::transmute::<&mut [u8], &'static mut [u8]>(buf) };
        Self::new(vec![slice])
    }
    pub fn len(&self) -> usize {
        let mut total: usize = 0;
        for b in self.buffers.iter() {
            total += b.len();
        }
        total
    }
    pub fn copy_from_slice(&mut self, src: &[u8]) -> usize {
        let mut copied = 0usize;
        for buffer in self.buffers.iter_mut() {
            if copied == src.len() {
                break;
            }
            let dst = &mut **buffer;
            let len = dst.len().min(src.len() - copied);
            dst[..len].copy_from_slice(&src[copied..copied + len]);
            copied += len;
        }
        copied
    }
    pub fn to_vec(&self) -> Vec<u8> {
        let mut data = Vec::with_capacity(self.len());
        for buffer in self.buffers.iter() {
            data.extend_from_slice(buffer);
        }
        data
    }
}

impl IntoIterator for UserBuffer {
    type Item = *mut u8;
    type IntoIter = UserBufferIterator;
    fn into_iter(self) -> Self::IntoIter {
        UserBufferIterator {
            buffers: self.buffers,
            current_buffer: 0,
            current_idx: 0,
        }
    }
}

pub struct UserBufferIterator {
    buffers: Vec<&'static mut [u8]>,
    current_buffer: usize,
    current_idx: usize,
}

impl Iterator for UserBufferIterator {
    type Item = *mut u8;
    fn next(&mut self) -> Option<Self::Item> {
        if self.current_buffer >= self.buffers.len() {
            None
        } else {
            let r = &mut self.buffers[self.current_buffer][self.current_idx] as *mut _;
            if self.current_idx + 1 == self.buffers[self.current_buffer].len() {
                self.current_idx = 0;
                self.current_buffer += 1;
            } else {
                self.current_idx += 1;
            }
            Some(r)
        }
    }
}
