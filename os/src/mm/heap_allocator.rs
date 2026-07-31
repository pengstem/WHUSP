use crate::config::{KERNEL_HEAP_SIZE, MAX_CPUS, PAGE_SIZE};
use crate::sync::SpinNoIrqLock;
use buddy_system_allocator::LockedHeap;
use core::alloc::{GlobalAlloc, Layout};
use core::cell::UnsafeCell;
use core::ptr::addr_of_mut;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

const SLAB_CLASS_SIZES: [usize; 9] = [16, 32, 64, 128, 256, 512, 1024, 2048, 4096];
const SLAB_CLASS_COUNT: usize = SLAB_CLASS_SIZES.len();
const MAGAZINE_CAPACITY: usize = 32;
const MAGAZINE_REFILL: usize = 16;
const BUDDY_HEAP_SIZE: usize = KERNEL_HEAP_SIZE / 2;
const SLAB_ARENA_SIZE: usize = KERNEL_HEAP_SIZE - BUDDY_HEAP_SIZE;

static SLAB_ENABLED: AtomicBool = AtomicBool::new(false);
static SLAB_ARENA_NEXT: AtomicUsize = AtomicUsize::new(0);
static SLAB_CENTRAL: [SpinNoIrqLock<FreeList>; SLAB_CLASS_COUNT] =
    [const { SpinNoIrqLock::new(FreeList::new()) }; SLAB_CLASS_COUNT];
static SLAB_MAGAZINES: PerCpuMagazines = PerCpuMagazines::new();

#[global_allocator]
static HEAP_ALLOCATOR: InterruptFreeLockedHeap<32> = InterruptFreeLockedHeap::empty();

struct InterruptGuard {
    enabled_before: bool,
}

impl InterruptGuard {
    fn new() -> Self {
        let enabled_before = crate::arch::interrupt::supervisor_interrupt_enabled();
        crate::arch::interrupt::disable_supervisor_interrupt();
        Self { enabled_before }
    }
}

impl Drop for InterruptGuard {
    fn drop(&mut self) {
        if self.enabled_before {
            crate::arch::interrupt::enable_supervisor_interrupt();
        }
    }
}

struct InterruptFreeLockedHeap<const ORDER: usize> {
    inner: LockedHeap<ORDER>,
}

#[derive(Clone, Copy)]
struct Magazine {
    slots: [usize; MAGAZINE_CAPACITY],
    len: usize,
}

impl Magazine {
    const fn new() -> Self {
        Self {
            slots: [0; MAGAZINE_CAPACITY],
            len: 0,
        }
    }

    fn pop(&mut self) -> Option<usize> {
        if self.len == 0 {
            return None;
        }
        self.len -= 1;
        Some(self.slots[self.len])
    }

    fn push(&mut self, address: usize) {
        assert!(self.len < MAGAZINE_CAPACITY);
        self.slots[self.len] = address;
        self.len += 1;
    }
}

struct PerCpuMagazines {
    inner: UnsafeCell<[[Magazine; SLAB_CLASS_COUNT]; MAX_CPUS]>,
}

unsafe impl Sync for PerCpuMagazines {}

impl PerCpuMagazines {
    const fn new() -> Self {
        Self {
            inner: UnsafeCell::new(
                [const { [const { Magazine::new() }; SLAB_CLASS_COUNT] }; MAX_CPUS],
            ),
        }
    }

    /// Returns the current CPU's magazine while local interrupts are masked.
    unsafe fn get(&self, cpu: usize, class: usize) -> &mut Magazine {
        unsafe { &mut (*self.inner.get())[cpu][class] }
    }
}

struct FreeList {
    head: usize,
}

impl FreeList {
    const fn new() -> Self {
        Self { head: 0 }
    }

    unsafe fn push(&mut self, address: usize) {
        unsafe {
            (address as *mut usize).write(self.head);
        }
        self.head = address;
    }

    unsafe fn pop(&mut self) -> Option<usize> {
        let address = self.head;
        if address == 0 {
            return None;
        }
        self.head = unsafe { (address as *const usize).read() };
        Some(address)
    }
}

impl<const ORDER: usize> InterruptFreeLockedHeap<ORDER> {
    const fn empty() -> Self {
        Self {
            inner: LockedHeap::empty(),
        }
    }

    unsafe fn init(&self, start: usize, size: usize) {
        let _guard = InterruptGuard::new();
        unsafe {
            self.inner.lock().init(start, size);
        }
    }
}

unsafe impl<const ORDER: usize> GlobalAlloc for InterruptFreeLockedHeap<ORDER> {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if SLAB_ENABLED.load(Ordering::Acquire)
            && let Some(class) = slab_class(layout)
            && let Some(address) = slab_alloc(class)
        {
            return address as *mut u8;
        }
        let _guard = InterruptGuard::new();
        unsafe { GlobalAlloc::alloc(&self.inner, layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        if SLAB_ENABLED.load(Ordering::Acquire)
            && let Some(class) = slab_class(layout)
            && is_slab_arena_address(ptr as usize)
        {
            slab_dealloc(class, ptr as usize);
            return;
        }
        let _guard = InterruptGuard::new();
        unsafe {
            GlobalAlloc::dealloc(&self.inner, ptr, layout);
        }
    }
}

#[alloc_error_handler]
pub fn handle_alloc_error(layout: core::alloc::Layout) -> ! {
    panic!("Heap allocation error, layout = {:?}", layout);
}

#[repr(C, align(4096))]
struct KernelHeapSpace([u8; KERNEL_HEAP_SIZE]);

static mut HEAP_SPACE: KernelHeapSpace = KernelHeapSpace([0; KERNEL_HEAP_SIZE]);

fn slab_class(layout: Layout) -> Option<usize> {
    let required = layout.size().max(layout.align()).max(1);
    if required > PAGE_SIZE {
        return None;
    }
    let slot_size = required.next_power_of_two().max(SLAB_CLASS_SIZES[0]);
    SLAB_CLASS_SIZES.iter().position(|size| *size == slot_size)
}

fn heap_start() -> usize {
    addr_of_mut!(HEAP_SPACE) as usize
}

fn is_slab_arena_address(address: usize) -> bool {
    let start = heap_start() + BUDDY_HEAP_SIZE;
    address >= start && address < start + SLAB_ARENA_SIZE
}

fn slab_alloc(class: usize) -> Option<usize> {
    let _irq = InterruptGuard::new();
    let cpu = crate::cpu::try_current_id()?;
    let magazine = unsafe { SLAB_MAGAZINES.get(cpu, class) };
    if let Some(address) = magazine.pop() {
        return Some(address);
    }

    {
        let mut central = SLAB_CENTRAL[class].lock();
        for _ in 0..MAGAZINE_REFILL {
            let Some(address) = (unsafe { central.pop() }) else {
                break;
            };
            magazine.push(address);
        }
        if let Some(address) = magazine.pop() {
            return Some(address);
        }
    }

    grow_slab_class(class)?;

    let mut central = SLAB_CENTRAL[class].lock();
    for _ in 0..MAGAZINE_REFILL {
        let Some(address) = (unsafe { central.pop() }) else {
            break;
        };
        magazine.push(address);
    }
    magazine.pop()
}

fn slab_dealloc(class: usize, address: usize) {
    let _irq = InterruptGuard::new();
    let Some(cpu) = crate::cpu::try_current_id() else {
        let mut central = SLAB_CENTRAL[class].lock();
        unsafe {
            central.push(address);
        }
        return;
    };

    let magazine = unsafe { SLAB_MAGAZINES.get(cpu, class) };
    if magazine.len == MAGAZINE_CAPACITY {
        let mut central = SLAB_CENTRAL[class].lock();
        for _ in 0..MAGAZINE_REFILL {
            let slot = magazine.pop().expect("full slab magazine became empty");
            unsafe {
                central.push(slot);
            }
        }
    }
    magazine.push(address);
}

fn grow_slab_class(class: usize) -> Option<()> {
    let offset = SLAB_ARENA_NEXT
        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |offset| {
            offset
                .checked_add(PAGE_SIZE)
                .filter(|next| *next <= SLAB_ARENA_SIZE)
        })
        .ok()?;
    let page = heap_start() + BUDDY_HEAP_SIZE + offset;

    let slot_size = SLAB_CLASS_SIZES[class];
    let mut central = SLAB_CENTRAL[class].lock();
    for offset in (0..PAGE_SIZE).step_by(slot_size) {
        unsafe {
            central.push(page + offset);
        }
    }
    Some(())
}

/// Publishes the statically reserved kernel heap to the global allocator.
///
/// Call this once during early boot, before code paths that can allocate while
/// device interrupts are enabled. Allocation itself masks supervisor interrupts
/// so allocator metadata cannot be re-entered from an interrupt handler.
pub fn init_heap() {
    unsafe {
        HEAP_ALLOCATOR.init(heap_start(), BUDDY_HEAP_SIZE);
    }
}

/// Enables the post-boot small-object allocator after physical memory exists.
/// Slab pages come from a disjoint statically reserved arena and remain owned
/// for the kernel lifetime. This prevents allocator metadata operations inside
/// the frame allocator from recursively entering the same lock hierarchy.
pub fn enable_slab_allocator() {
    SLAB_ENABLED.store(true, Ordering::Release);
}
