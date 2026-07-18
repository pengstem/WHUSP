use crate::arch::interrupt;
use core::marker::PhantomData;

/// Saves and disables the current CPU's supervisor interrupt state.
///
/// The state is owned by this guard instead of a global nesting counter, so
/// nested critical sections and independent CPUs cannot corrupt one another's
/// restore decision.
#[must_use = "dropping the guard restores the saved local interrupt state"]
pub struct LocalIrqGuard {
    was_enabled: bool,
    // A local IRQ guard must never migrate to another CPU before Drop.
    _not_send: PhantomData<*mut ()>,
}

impl LocalIrqGuard {
    pub fn disable() -> Self {
        let was_enabled = interrupt::supervisor_interrupt_enabled();
        interrupt::disable_supervisor_interrupt();
        Self {
            was_enabled,
            _not_send: PhantomData,
        }
    }

    pub fn was_enabled(&self) -> bool {
        self.was_enabled
    }
}

impl Drop for LocalIrqGuard {
    fn drop(&mut self) {
        if self.was_enabled {
            interrupt::enable_supervisor_interrupt();
        }
    }
}
