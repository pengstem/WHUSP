use super::{FrameTracker, PhysAddr, PhysPageNum, VirtAddr, VirtPageNum, frame_alloc};
use crate::arch::mm as arch_mm;
use crate::perf;
use alloc::vec;
use alloc::vec::Vec;
use bitflags::*;
use core::ops::{Deref, DerefMut};

const PAGE_TABLE_LEVELS: usize = 3;
const PAGE_TABLE_INDEX_BITS: usize = 9;

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
        // Abstract software COW marker. `arch::mm` maps it to an
        // architecture-safe PTE bit; do not treat this value as a portable
        // hardware encoding outside this flag set.
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
    fn new_leaf(ppn: PhysPageNum, flags: PTEFlags, level: usize) -> Self {
        PageTableEntry {
            bits: arch_mm::pte_new_leaf_bits(ppn.0, flags, level),
        }
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
    fn is_leaf(&self) -> bool {
        arch_mm::pte_is_leaf(self.bits)
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
    asid: usize,
    frames: Vec<FrameTracker>,
    leaves_4k: usize,
    leaves_2m: usize,
    leaves_1g: usize,
}

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct PageTableStats {
    pub(crate) frames: usize,
    pub(crate) leaves_4k: usize,
    pub(crate) leaves_2m: usize,
    pub(crate) leaves_1g: usize,
}

// CONTEXT: `try_new`/`try_map` are the recoverable allocation API for user
// memory paths. The panic-style helpers below are used only where the caller
// has already committed to a kernel mapping invariant; failure there is a
// kernel bug or an unrecoverable boot-path allocation failure.
impl PageTable {
    pub fn new() -> Self {
        let _profile_scope = perf::time_scope(perf::ProfilePoint::FrameAllocPageTable);
        let frame = frame_alloc().expect("page table root allocation requires a free frame");
        PageTable {
            root_ppn: frame.ppn,
            asid: arch_mm::alloc_page_table_asid(),
            frames: vec![frame],
            leaves_4k: 0,
            leaves_2m: 0,
            leaves_1g: 0,
        }
    }
    pub fn try_new() -> Option<Self> {
        let _profile_scope = perf::time_scope(perf::ProfilePoint::FrameAllocPageTable);
        let frame = frame_alloc()?;
        Some(PageTable {
            root_ppn: frame.ppn,
            asid: arch_mm::alloc_page_table_asid(),
            frames: vec![frame],
            leaves_4k: 0,
            leaves_2m: 0,
            leaves_1g: 0,
        })
    }
    /// Builds a non-owning view over an existing page-table token.
    ///
    /// `frames` stays empty so dropping this wrapper never frees page-table
    /// pages; syscall copy helpers use it only to translate user addresses.
    pub fn from_token(satp: usize) -> Self {
        Self {
            root_ppn: PhysPageNum::from(arch_mm::page_table_root_ppn(satp)),
            asid: arch_mm::page_table_asid(satp),
            frames: Vec::new(),
            leaves_4k: 0,
            leaves_2m: 0,
            leaves_1g: 0,
        }
    }
    fn find_pte_create_at_level(
        &mut self,
        vpn: VirtPageNum,
        target_level: usize,
    ) -> Option<&mut PageTableEntry> {
        assert!(target_level < PAGE_TABLE_LEVELS);
        let idxs = vpn.indexes();
        let mut ppn = self.root_ppn;
        let target_depth = PAGE_TABLE_LEVELS - 1 - target_level;
        for (depth, idx) in idxs.iter().enumerate() {
            let pte = &mut ppn.get_pte_array_mut()[*idx];
            if depth == target_depth {
                return Some(pte);
            }
            if pte.is_leaf() {
                return None;
            }
            if !pte.is_valid() {
                let _profile_scope = perf::time_scope(perf::ProfilePoint::FrameAllocPageTable);
                let frame = frame_alloc()?;
                *pte = PageTableEntry::new(frame.ppn, PTEFlags::V);
                self.frames.push(frame);
            }
            ppn = pte.ppn();
        }
        None
    }
    fn find_pte_create(&mut self, vpn: VirtPageNum) -> Option<&mut PageTableEntry> {
        self.find_pte_create_at_level(vpn, 0)
    }
    fn find_leaf(&self, vpn: VirtPageNum) -> Option<(&PageTableEntry, usize)> {
        let idxs = vpn.indexes();
        let mut ppn = self.root_ppn;
        for (depth, idx) in idxs.iter().enumerate() {
            let pte = &ppn.get_pte_array()[*idx];
            let level = PAGE_TABLE_LEVELS - 1 - depth;
            if level == 0 || pte.is_leaf() {
                return Some((pte, level));
            }
            if !pte.is_valid() {
                return None;
            }
            ppn = pte.ppn();
        }
        None
    }
    fn find_pte_at_level_mut(
        &mut self,
        vpn: VirtPageNum,
        target_level: usize,
    ) -> Option<&mut PageTableEntry> {
        assert!(target_level < PAGE_TABLE_LEVELS);
        let idxs = vpn.indexes();
        let mut ppn = self.root_ppn;
        let target_depth = PAGE_TABLE_LEVELS - 1 - target_level;
        for (depth, idx) in idxs.iter().enumerate() {
            let pte = &mut ppn.get_pte_array_mut()[*idx];
            if depth == target_depth {
                return Some(pte);
            }
            if !pte.is_valid() || pte.is_leaf() {
                return None;
            }
            ppn = pte.ppn();
        }
        None
    }
    fn find_pte_mut(&mut self, vpn: VirtPageNum) -> Option<&mut PageTableEntry> {
        self.find_pte_at_level_mut(vpn, 0)
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
        *pte = PageTableEntry::new_leaf(ppn, flags, 0);
        self.leaves_4k += 1;
        true
    }
    /// Allocates the page-table path for `vpn` without publishing a leaf.
    ///
    /// Bulk mapping code uses this as a preflight step so allocation failure
    /// cannot occur after some leaf PTEs have become visible to another CPU.
    pub(super) fn prepare_empty_leaf_path(&mut self, vpn: VirtPageNum) -> bool {
        self.find_pte_create(vpn).is_some_and(|pte| pte.bits == 0)
    }
    pub(super) fn try_map_kernel_identical_range(
        &mut self,
        start_vpn: VirtPageNum,
        end_vpn: VirtPageNum,
        flags: PTEFlags,
    ) -> bool {
        assert!(start_vpn <= end_vpn);

        // Preflight allocates every required intermediate table and checks
        // every destination before publishing any leaf. A failed preflight
        // can leave empty table pages owned by this PageTable, but cannot
        // expose a partial mapping to another CPU.
        let mut vpn = start_vpn;
        while vpn < end_vpn {
            let ppn = identical_ppn(vpn);
            let level = largest_fit_kernel_leaf_level(vpn, ppn, end_vpn.0 - vpn.0);
            let Some(pte) = self.find_pte_create_at_level(vpn, level) else {
                return false;
            };
            if pte.bits != 0 {
                return false;
            }
            vpn.0 += pages_per_leaf(level);
        }

        let flags = normalized_leaf_flags(flags);
        vpn = start_vpn;
        while vpn < end_vpn {
            let ppn = identical_ppn(vpn);
            let level = largest_fit_kernel_leaf_level(vpn, ppn, end_vpn.0 - vpn.0);
            let pte = self
                .find_pte_at_level_mut(vpn, level)
                .expect("preflighted kernel leaf path disappeared");
            assert_eq!(
                pte.bits, 0,
                "preflighted kernel leaf became occupied: vpn={vpn:?} level={level}"
            );
            *pte = PageTableEntry::new_leaf(ppn, flags, level);
            self.increment_leaf_count(level);
            vpn.0 += pages_per_leaf(level);
        }

        self.verify_kernel_identical_range(start_vpn, end_vpn, flags);
        true
    }
    pub(super) fn unmap_kernel_identical_range(
        &mut self,
        start_vpn: VirtPageNum,
        end_vpn: VirtPageNum,
    ) -> bool {
        assert!(start_vpn <= end_vpn);
        let mut vpn = start_vpn;
        while vpn < end_vpn {
            let Some((pte, level)) = self.find_leaf(vpn) else {
                return false;
            };
            if pte.bits == 0 {
                return false;
            }
            let pages = pages_per_leaf(level);
            if vpn.0 & (pages - 1) != 0 || pages > end_vpn.0 - vpn.0 {
                return false;
            }
            let pte = self
                .find_pte_at_level_mut(vpn, level)
                .expect("located kernel leaf path disappeared");
            *pte = PageTableEntry::empty();
            self.decrement_leaf_count(level);
            vpn.0 += pages;
        }
        true
    }
    pub fn unmap(&mut self, vpn: VirtPageNum) {
        let pte = self
            .find_pte_mut(vpn)
            .expect("unmap requires an existing page-table path");
        assert!(
            pte.is_valid() || pte.bits != 0,
            "vpn {vpn:?} is invalid before unmapping"
        );
        *pte = PageTableEntry::empty();
    }
    pub fn remap_flags(&mut self, vpn: VirtPageNum, flags: PTEFlags) -> bool {
        let Some(pte) = self.find_pte_mut(vpn) else {
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
    pub fn clear_leaf(&mut self, vpn: VirtPageNum) -> bool {
        let Some(pte) = self.find_pte_mut(vpn) else {
            return false;
        };
        *pte = PageTableEntry::empty();
        true
    }
    pub fn clear_leaf_create_path(&mut self, vpn: VirtPageNum) -> bool {
        let Some(pte) = self.find_pte_create(vpn) else {
            return false;
        };
        *pte = PageTableEntry::empty();
        true
    }
    pub fn mark_cow_readonly(&mut self, vpn: VirtPageNum) -> bool {
        let Some(pte) = self.find_pte_mut(vpn) else {
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
        let Some(pte) = self.find_pte_mut(vpn) else {
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
        let Some(pte) = self.find_pte_mut(vpn) else {
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
        self.find_leaf(vpn).map(|(pte, level)| {
            if level == 0 || pte.bits == 0 {
                return *pte;
            }
            let offset_mask = pages_per_leaf(level) - 1;
            let base_ppn = pte.ppn().0;
            assert_eq!(
                base_ppn & offset_mask,
                0,
                "unaligned level-{level} leaf PPN {base_ppn:#x}"
            );
            PageTableEntry::new(
                PhysPageNum::from(base_ppn | (vpn.0 & offset_mask)),
                pte.flags(),
            )
        })
    }
    pub fn translate_va(&self, va: VirtAddr) -> Option<PhysAddr> {
        self.translate(va.clone().floor()).map(|pte| {
            let aligned_pa: PhysAddr = pte.ppn().into();
            let offset = va.page_offset();
            let aligned_pa_usize: usize = aligned_pa.into();
            (aligned_pa_usize + offset).into()
        })
    }
    pub fn token(&self) -> usize {
        arch_mm::page_table_token_with_asid(self.root_ppn.0, self.asid)
    }

    pub(crate) fn stats(&self) -> PageTableStats {
        PageTableStats {
            frames: self.frames.len(),
            leaves_4k: self.leaves_4k,
            leaves_2m: self.leaves_2m,
            leaves_1g: self.leaves_1g,
        }
    }

    fn increment_leaf_count(&mut self, level: usize) {
        match level {
            0 => self.leaves_4k += 1,
            1 => self.leaves_2m += 1,
            2 => self.leaves_1g += 1,
            _ => unreachable!("unsupported page-table leaf level {level}"),
        }
    }

    fn decrement_leaf_count(&mut self, level: usize) {
        let counter = match level {
            0 => &mut self.leaves_4k,
            1 => &mut self.leaves_2m,
            2 => &mut self.leaves_1g,
            _ => unreachable!("unsupported page-table leaf level {level}"),
        };
        *counter = counter
            .checked_sub(1)
            .expect("page-table leaf counter underflow");
    }

    fn verify_kernel_identical_range(
        &self,
        start_vpn: VirtPageNum,
        end_vpn: VirtPageNum,
        expected_flags: PTEFlags,
    ) {
        let mut vpn = start_vpn;
        while vpn < end_vpn {
            let expected_ppn = identical_ppn(vpn);
            let remaining = end_vpn.0 - vpn.0;
            let expected_level = largest_fit_kernel_leaf_level(vpn, expected_ppn, remaining);
            let (pte, actual_level) = self
                .find_leaf(vpn)
                .expect("published kernel identity leaf is not walkable");
            assert_ne!(pte.bits, 0, "published kernel identity leaf is empty");
            assert_eq!(
                actual_level, expected_level,
                "kernel identity leaf level mismatch at {vpn:?}"
            );
            assert_eq!(
                pte.flags(),
                expected_flags,
                "kernel identity leaf permissions mismatch at {vpn:?}"
            );
            assert_eq!(
                self.translate(vpn).map(|entry| entry.ppn()),
                Some(expected_ppn),
                "kernel identity translation mismatch at {vpn:?}"
            );

            let pages = pages_per_leaf(expected_level);
            let last_vpn = VirtPageNum(vpn.0 + pages - 1);
            let expected_last_ppn = PhysPageNum(expected_ppn.0 + pages - 1);
            assert_eq!(
                self.translate(last_vpn).map(|entry| entry.ppn()),
                Some(expected_last_ppn),
                "kernel identity end translation mismatch at {last_vpn:?}"
            );
            vpn.0 += pages;
        }
    }
}

fn normalized_leaf_flags(flags: PTEFlags) -> PTEFlags {
    let leaf_flags = PTEFlags::R | PTEFlags::W | PTEFlags::X;
    if flags.intersects(leaf_flags) {
        flags | PTEFlags::V
    } else {
        flags
    }
}

fn pages_per_leaf(level: usize) -> usize {
    1usize << (PAGE_TABLE_INDEX_BITS * level)
}

fn identical_ppn(vpn: VirtPageNum) -> PhysPageNum {
    let va: VirtAddr = vpn.into();
    PhysAddr::from(usize::from(va)).floor()
}

fn largest_fit_kernel_leaf_level(
    vpn: VirtPageNum,
    ppn: PhysPageNum,
    remaining_pages: usize,
) -> usize {
    for level in (1..=arch_mm::MAX_KERNEL_LEAF_LEVEL).rev() {
        let pages = pages_per_leaf(level);
        if remaining_pages >= pages && vpn.0 & (pages - 1) == 0 && ppn.0 & (pages - 1) == 0 {
            return level;
        }
    }
    0
}

/// A checked user translation together with allocator references for its pages.
///
/// The slices and pins must move together until the copy or File operation is
/// complete. Keeping this intermediate type separate prevents truncation and
/// iovec assembly from accidentally dropping a pin before its slice.
pub(crate) struct TranslatedUserBuffer {
    buffers: Vec<&'static mut [u8]>,
    pins: Vec<FrameTracker>,
}

impl TranslatedUserBuffer {
    pub(crate) fn new(buffers: Vec<&'static mut [u8]>, pins: Vec<FrameTracker>) -> Self {
        assert_eq!(
            buffers.len(),
            pins.len(),
            "translated user segments and pins diverged"
        );
        Self { buffers, pins }
    }

    pub(crate) fn empty() -> Self {
        Self {
            buffers: Vec::new(),
            pins: Vec::new(),
        }
    }

    pub(crate) fn append(&mut self, mut other: Self) {
        self.buffers.append(&mut other.buffers);
        self.pins.append(&mut other.pins);
    }

    pub(crate) fn truncate(mut self, mut limit: usize) -> Self {
        let keep = self
            .buffers
            .iter()
            .position(|buffer| {
                if limit == 0 {
                    true
                } else if buffer.len() <= limit {
                    limit -= buffer.len();
                    false
                } else {
                    true
                }
            })
            .unwrap_or(self.buffers.len());
        if keep < self.buffers.len() && limit > 0 {
            let buffer = &mut self.buffers[keep];
            let ptr = buffer.as_mut_ptr();
            *buffer = unsafe { core::slice::from_raw_parts_mut(ptr, limit) };
            self.buffers.truncate(keep + 1);
            self.pins.truncate(keep + 1);
        } else {
            self.buffers.truncate(keep);
            self.pins.truncate(keep);
        }
        self
    }

    fn into_parts(self) -> (Vec<&'static mut [u8]>, Vec<FrameTracker>) {
        (self.buffers, self.pins)
    }
}

impl Deref for TranslatedUserBuffer {
    type Target = Vec<&'static mut [u8]>;

    fn deref(&self) -> &Self::Target {
        &self.buffers
    }
}

impl DerefMut for TranslatedUserBuffer {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.buffers
    }
}

pub(crate) struct TranslatedUserBufferIntoIter {
    buffers: alloc::vec::IntoIter<&'static mut [u8]>,
    _pins: Vec<FrameTracker>,
}

impl Iterator for TranslatedUserBufferIntoIter {
    type Item = &'static mut [u8];

    fn next(&mut self) -> Option<Self::Item> {
        self.buffers.next()
    }
}

impl IntoIterator for TranslatedUserBuffer {
    type Item = &'static mut [u8];
    type IntoIter = TranslatedUserBufferIntoIter;

    fn into_iter(self) -> Self::IntoIter {
        TranslatedUserBufferIntoIter {
            buffers: self.buffers.into_iter(),
            _pins: self.pins,
        }
    }
}

impl<'a> IntoIterator for &'a TranslatedUserBuffer {
    type Item = &'a &'static mut [u8];
    type IntoIter = core::slice::Iter<'a, &'static mut [u8]>;

    fn into_iter(self) -> Self::IntoIter {
        self.buffers.iter()
    }
}

// CONTEXT: most syscall copy paths use checked byte-buffer helpers now. Keep
// this segmented buffer type for legacy in-kernel adapters that still iterate
// translated slices directly.
pub struct UserBuffer {
    pub buffers: Vec<&'static mut [u8]>,
    // Keep every translated user page allocated for the entire possibly
    // sleeping File operation. Mapping removal may clear its PTE meanwhile,
    // but the physical frame cannot be recycled until this buffer is dropped.
    _pins: Vec<FrameTracker>,
}

impl UserBuffer {
    pub(crate) fn new(translated: TranslatedUserBuffer) -> Self {
        let (buffers, pins) = translated.into_parts();
        Self {
            buffers,
            _pins: pins,
        }
    }
    /// Wraps a kernel-owned slice for synchronous in-kernel File trait I/O.
    ///
    /// The returned buffer must be consumed immediately and must not be stored
    /// by the callee. It exists for legacy File::read/write adapters that still
    /// use UserBuffer as their byte carrier even when the source is kernel memory.
    pub fn from_kernel_slice_for_sync_io(buf: &mut [u8]) -> Self {
        let slice = unsafe { core::mem::transmute::<&mut [u8], &'static mut [u8]>(buf) };
        Self {
            buffers: vec![slice],
            _pins: Vec::new(),
        }
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

// An explicit destructor prevents callers from partially moving `buffers`
// out and dropping the page pins before the raw slices have been consumed.
impl Drop for UserBuffer {
    fn drop(&mut self) {}
}
