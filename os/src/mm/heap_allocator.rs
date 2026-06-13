use crate::config::KERNEL_HEAP_SIZE;
use buddy_system_allocator::LockedHeap;
use core::alloc::{GlobalAlloc, Layout};
use core::ptr::addr_of_mut;

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
        let _guard = InterruptGuard::new();
        unsafe { GlobalAlloc::alloc(&self.inner, layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
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

static mut HEAP_SPACE: [u8; KERNEL_HEAP_SIZE] = [0; KERNEL_HEAP_SIZE];

/// what the hack did this init do
pub fn init_heap() {
    unsafe {
        HEAP_ALLOCATOR.init(addr_of_mut!(HEAP_SPACE) as usize, KERNEL_HEAP_SIZE);
    }
}
