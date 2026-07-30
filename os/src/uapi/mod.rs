//! Kernel-wide Linux ABI definitions.
//!
//! Domain subsystems may depend on these value types and constants. ABI
//! adapters under `syscall` must not own definitions shared with filesystem,
//! memory-management, task, or networking code.

pub(crate) mod errno;
pub(crate) mod linux;
pub(crate) mod syscall_nr;
