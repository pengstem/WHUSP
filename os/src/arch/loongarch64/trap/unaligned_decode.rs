//! Pure LoongArch scalar load/store decoding for user ALE emulation.
//!
//! Keep this module free of kernel dependencies so the decoder can be tested
//! directly on the host as well as compiled into the LoongArch kernel.

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum UnalignedOperation {
    LoadSigned,
    LoadUnsigned,
    Store,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct DecodedUnalignedInstruction {
    pub(crate) operation: UnalignedOperation,
    pub(crate) size: usize,
    pub(crate) rd: usize,
}

impl DecodedUnalignedInstruction {
    pub(crate) const fn loaded_value(self, raw: u64) -> usize {
        match (self.operation, self.size) {
            (UnalignedOperation::LoadSigned, 2) => (raw as i16 as i64) as usize,
            (UnalignedOperation::LoadSigned, 4) => (raw as i32 as i64) as usize,
            (UnalignedOperation::LoadSigned, 8) | (UnalignedOperation::LoadUnsigned, 8) => {
                raw as usize
            }
            (UnalignedOperation::LoadUnsigned, 2) => (raw as u16) as usize,
            (UnalignedOperation::LoadUnsigned, 4) => (raw as u32) as usize,
            _ => raw as usize,
        }
    }
}

const fn decoded(
    word: u32,
    operation: UnalignedOperation,
    size: usize,
) -> Option<DecodedUnalignedInstruction> {
    Some(DecodedUnalignedInstruction {
        operation,
        size,
        rd: (word & 0x1f) as usize,
    })
}

/// Decode only scalar integer accesses that Linux's LoongArch ALE handler
/// emulates. Byte accesses cannot cause alignment exceptions; atomics and FP
/// accesses deliberately remain unsupported.
pub(crate) const fn decode_unaligned_instruction(word: u32) -> Option<DecodedUnalignedInstruction> {
    // reg2i12: rd[4:0], rj[9:5], si12[21:10], opcode[31:22].
    match word >> 22 {
        0xa1 => return decoded(word, UnalignedOperation::LoadSigned, 2), // ld.h
        0xa2 => return decoded(word, UnalignedOperation::LoadSigned, 4), // ld.w
        0xa3 => return decoded(word, UnalignedOperation::LoadSigned, 8), // ld.d
        0xa5 => return decoded(word, UnalignedOperation::Store, 2),      // st.h
        0xa6 => return decoded(word, UnalignedOperation::Store, 4),      // st.w
        0xa7 => return decoded(word, UnalignedOperation::Store, 8),      // st.d
        0xa9 => return decoded(word, UnalignedOperation::LoadUnsigned, 2), // ld.hu
        0xaa => return decoded(word, UnalignedOperation::LoadUnsigned, 4), // ld.wu
        _ => {}
    }

    // reg2i14: rd[4:0], rj[9:5], si14[23:10], opcode[31:24].
    match word >> 24 {
        0x24 => return decoded(word, UnalignedOperation::LoadSigned, 4), // ldptr.w
        0x25 => return decoded(word, UnalignedOperation::Store, 4),      // stptr.w
        0x26 => return decoded(word, UnalignedOperation::LoadSigned, 8), // ldptr.d
        0x27 => return decoded(word, UnalignedOperation::Store, 8),      // stptr.d
        _ => {}
    }

    // reg3: rd[4:0], rj[9:5], rk[14:10], opcode[31:15].
    match word >> 15 {
        0x7008 => decoded(word, UnalignedOperation::LoadSigned, 2), // ldx.h
        0x7010 => decoded(word, UnalignedOperation::LoadSigned, 4), // ldx.w
        0x7018 => decoded(word, UnalignedOperation::LoadSigned, 8), // ldx.d
        0x7028 => decoded(word, UnalignedOperation::Store, 2),      // stx.h
        0x7030 => decoded(word, UnalignedOperation::Store, 4),      // stx.w
        0x7038 => decoded(word, UnalignedOperation::Store, 8),      // stx.d
        0x7048 => decoded(word, UnalignedOperation::LoadUnsigned, 2), // ldx.hu
        0x7050 => decoded(word, UnalignedOperation::LoadUnsigned, 4), // ldx.wu
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const fn reg2i12(opcode: u32, rd: u32) -> u32 {
        (opcode << 22) | (0xfff << 10) | (7 << 5) | rd
    }

    const fn reg2i14(opcode: u32, rd: u32) -> u32 {
        (opcode << 24) | (0x3fff << 10) | (8 << 5) | rd
    }

    const fn reg3(opcode: u32, rd: u32) -> u32 {
        (opcode << 15) | (9 << 10) | (8 << 5) | rd
    }

    #[test]
    fn decodes_linux_integer_ale_instruction_set() {
        let cases = [
            (reg2i12(0xa1, 12), UnalignedOperation::LoadSigned, 2),
            (reg2i12(0xa2, 13), UnalignedOperation::LoadSigned, 4),
            (reg2i12(0xa3, 14), UnalignedOperation::LoadSigned, 8),
            (reg2i12(0xa5, 15), UnalignedOperation::Store, 2),
            (reg2i12(0xa6, 16), UnalignedOperation::Store, 4),
            (reg2i12(0xa7, 17), UnalignedOperation::Store, 8),
            (reg2i12(0xa9, 18), UnalignedOperation::LoadUnsigned, 2),
            (reg2i12(0xaa, 19), UnalignedOperation::LoadUnsigned, 4),
            (reg2i14(0x24, 20), UnalignedOperation::LoadSigned, 4),
            (reg2i14(0x25, 21), UnalignedOperation::Store, 4),
            (reg2i14(0x26, 22), UnalignedOperation::LoadSigned, 8),
            (reg2i14(0x27, 23), UnalignedOperation::Store, 8),
            (reg3(0x7008, 24), UnalignedOperation::LoadSigned, 2),
            (reg3(0x7010, 25), UnalignedOperation::LoadSigned, 4),
            (reg3(0x7018, 26), UnalignedOperation::LoadSigned, 8),
            (reg3(0x7028, 27), UnalignedOperation::Store, 2),
            (reg3(0x7030, 28), UnalignedOperation::Store, 4),
            (reg3(0x7038, 29), UnalignedOperation::Store, 8),
            (reg3(0x7048, 30), UnalignedOperation::LoadUnsigned, 2),
            (reg3(0x7050, 31), UnalignedOperation::LoadUnsigned, 4),
        ];
        for (word, operation, size) in cases {
            let decoded = decode_unaligned_instruction(word).unwrap();
            assert_eq!(decoded.operation, operation);
            assert_eq!(decoded.size, size);
            assert_eq!(decoded.rd, (word & 0x1f) as usize);
        }
    }

    #[test]
    fn rejects_byte_fp_atomic_and_unknown_accesses() {
        for word in [
            reg2i12(0xa0, 1), // ld.b
            reg2i12(0xa4, 2), // st.b
            reg2i12(0xac, 3), // fld.s
            reg2i14(0x20, 4), // ll.w
            reg3(0x70c2, 5),  // amadd.w
            0,
        ] {
            assert_eq!(decode_unaligned_instruction(word), None);
        }
    }

    #[test]
    fn applies_load_extension_and_preserves_register_zero_rule_at_caller() {
        let signed_half = decode_unaligned_instruction(reg2i12(0xa1, 0)).unwrap();
        let unsigned_half = decode_unaligned_instruction(reg2i12(0xa9, 1)).unwrap();
        let signed_word = decode_unaligned_instruction(reg2i12(0xa2, 2)).unwrap();
        let unsigned_word = decode_unaligned_instruction(reg2i12(0xaa, 3)).unwrap();
        assert_eq!(signed_half.loaded_value(0x8001), usize::MAX - 0x7ffe);
        assert_eq!(unsigned_half.loaded_value(0xffff), 0xffff);
        assert_eq!(
            signed_word.loaded_value(0x8000_0001),
            usize::MAX - 0x7fff_fffe
        );
        assert_eq!(unsigned_word.loaded_value(0xffff_ffff), 0xffff_ffff);
        assert_eq!(signed_half.rd, 0);
    }
}
