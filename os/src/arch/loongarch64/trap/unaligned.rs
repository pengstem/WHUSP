//! Bounded user-mode scalar load/store emulation for LoongArch ALE traps.

use super::context::TrapContext;
use super::unaligned_decode::{
    DecodedUnalignedInstruction, UnalignedOperation, decode_unaligned_instruction,
};
use crate::syscall::user_ptr::{read_user_value_with_mmap_fault, write_user_value_with_mmap_fault};
use crate::task::current_user_token;
use core::sync::atomic::{AtomicBool, Ordering};

static REPORTED_FIRST_USER_ALE: AtomicBool = AtomicBool::new(false);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum UserUnalignedOutcome {
    Emulated {
        register_write: Option<(usize, usize)>,
    },
    Segv,
    Bus,
}

fn read_unaligned_user_value(
    token: usize,
    address: usize,
    decoded: DecodedUnalignedInstruction,
) -> Option<usize> {
    let raw = match decoded.size {
        2 => read_user_value_with_mmap_fault(token, address as *const u16)
            .ok()
            .map(u64::from),
        4 => read_user_value_with_mmap_fault(token, address as *const u32)
            .ok()
            .map(u64::from),
        8 => read_user_value_with_mmap_fault(token, address as *const u64).ok(),
        _ => None,
    }?;
    Some(decoded.loaded_value(raw))
}

fn write_unaligned_user_value(token: usize, address: usize, size: usize, value: usize) -> bool {
    match size {
        2 => write_user_value_with_mmap_fault(token, address as *mut u16, &(value as u16)),
        4 => write_user_value_with_mmap_fault(token, address as *mut u32, &(value as u32)),
        8 => write_user_value_with_mmap_fault(token, address as *mut u64, &(value as u64)),
        _ => return false,
    }
    .is_ok()
}

/// Emulate one scalar integer user ALE without retaining a TrapContext borrow
/// across user-copy fault handling. The caller applies any register write and
/// advances ERA only after this function reports success.
pub(super) fn emulate_user_unaligned(
    era: usize,
    badv: usize,
    registers: &[usize; 32],
) -> UserUnalignedOutcome {
    let token = current_user_token();
    let instruction = match read_user_value_with_mmap_fault(token, era as *const u32) {
        Ok(instruction) => instruction,
        Err(_) => {
            println!(
                "KERN: LoongArch user ALE instruction fetch failed era={:#x} badv={:#x}",
                era, badv
            );
            return UserUnalignedOutcome::Segv;
        }
    };
    let Some(decoded) = decode_unaligned_instruction(instruction) else {
        println!(
            "KERN: unsupported LoongArch user ALE era={:#x} badv={:#x} instruction={:#010x}",
            era, badv, instruction
        );
        return UserUnalignedOutcome::Bus;
    };

    if !REPORTED_FIRST_USER_ALE.swap(true, Ordering::Relaxed) {
        println!(
            "KERN: LoongArch user ALE era={:#x} badv={:#x} instruction={:#010x} operation={:?} size={} rd={} rd_value={:#x}",
            era,
            badv,
            instruction,
            decoded.operation,
            decoded.size,
            decoded.rd,
            if decoded.rd == 0 {
                0
            } else {
                registers[decoded.rd]
            }
        );
    }

    match decoded.operation {
        UnalignedOperation::LoadSigned | UnalignedOperation::LoadUnsigned => {
            let Some(value) = read_unaligned_user_value(token, badv, decoded) else {
                return UserUnalignedOutcome::Segv;
            };
            UserUnalignedOutcome::Emulated {
                // LoongArch r0 remains hard-wired to zero.
                register_write: (decoded.rd != 0).then_some((decoded.rd, value)),
            }
        }
        UnalignedOperation::Store => {
            let value = if decoded.rd == 0 {
                0
            } else {
                registers[decoded.rd]
            };
            if write_unaligned_user_value(token, badv, decoded.size, value) {
                UserUnalignedOutcome::Emulated {
                    register_write: None,
                }
            } else {
                UserUnalignedOutcome::Segv
            }
        }
    }
}

/// Apply a successful emulation result after all potentially blocking user
/// copies have completed.
pub(super) fn finish_user_unaligned(
    cx: &mut TrapContext,
    era: usize,
    register_write: Option<(usize, usize)>,
) {
    if let Some((register, value)) = register_write {
        debug_assert_ne!(register, 0);
        cx.x[register] = value;
    }
    cx.era = era.wrapping_add(4);
}
