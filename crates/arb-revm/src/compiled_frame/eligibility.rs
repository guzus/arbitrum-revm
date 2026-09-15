//! Conservative compile eligibility. Decoding skips PUSH (and other) immediates.

use crate::evm::{ARB_INSTRUCTION_OVERRIDES, COMPILED_HOST_BRIDGED_OVERRIDES};
use revm::bytecode::opcode::OPCODE_INFO;

/// Why a bytecode image is refused for compilation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IneligibleReason {
    /// Empty runtime code. There is nothing to compile.
    Empty,
    /// `0xEF` prefix: EOF or a Stylus discriminant. Stylus is handled before this
    /// path; EOF is out of scope for the prototype.
    EofOrStylusPrefix,
    /// Opcode whose ArbEvm instruction-table entry differs from mainnet and is
    /// **not** listed in [`COMPILED_HOST_BRIDGED_OVERRIDES`].
    ///
    /// Compiled code skips the instruction table. Unclassified overrides are
    /// refused. NUMBER is bridged (compiled Host `block_number` returns L1).
    /// BLOCKHASH stays refused: revmc's builtin adds an L2-style 256-block range
    /// check against `Host::block_number()` then `Host::block_hash()`, which is
    /// not the ArbOS L1 ring used by the interpreter override.
    InstructionOverride(u8),
}

/// Returns `Some` when `code` must not be compiled.
///
/// Immediate bytes of `PUSH1..=PUSH32` (and any other opcode with an immediate) are
/// skipped so a `PUSH1 0x43` does **not** count as `NUMBER`. Table overrides are
/// refused unless they are explicitly host-bridged; adding an `insert_instruction`
/// without classifying it fails closed (ineligible), not open.
pub fn bytecode_ineligible(code: &[u8]) -> Option<IneligibleReason> {
    if code.is_empty() {
        return Some(IneligibleReason::Empty);
    }
    if code.first() == Some(&0xef) {
        return Some(IneligibleReason::EofOrStylusPrefix);
    }

    let mut i = 0;
    while i < code.len() {
        let op = code[i];
        if ARB_INSTRUCTION_OVERRIDES.contains(&op) && !COMPILED_HOST_BRIDGED_OVERRIDES.contains(&op)
        {
            return Some(IneligibleReason::InstructionOverride(op));
        }
        let immediate = OPCODE_INFO[op as usize]
            .map(|info| info.immediate_size() as usize)
            .unwrap_or(0);
        i = i.saturating_add(1).saturating_add(immediate);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::{IneligibleReason, bytecode_ineligible};
    use revm::bytecode::opcode;

    #[test]
    fn empty_and_ef_prefix_are_ineligible() {
        assert_eq!(bytecode_ineligible(&[]), Some(IneligibleReason::Empty));
        assert_eq!(
            bytecode_ineligible(&[0xef, 0xf0, 0x00]),
            Some(IneligibleReason::EofOrStylusPrefix)
        );
    }

    #[test]
    fn number_is_eligible_blockhash_is_not() {
        assert_eq!(
            bytecode_ineligible(&[opcode::NUMBER, opcode::STOP]),
            None,
            "NUMBER is host-bridged and must compile"
        );
        assert_eq!(
            bytecode_ineligible(&[opcode::BLOCKHASH, opcode::STOP]),
            Some(IneligibleReason::InstructionOverride(opcode::BLOCKHASH))
        );
        assert_eq!(
            bytecode_ineligible(&[opcode::NUMBER, opcode::BLOCKHASH, opcode::STOP]),
            Some(IneligibleReason::InstructionOverride(opcode::BLOCKHASH)),
            "NUMBER must not waive a co-occurring refused override"
        );
    }

    #[test]
    fn every_instruction_override_is_bridged_or_refused() {
        use crate::evm::{ARB_INSTRUCTION_OVERRIDES, COMPILED_HOST_BRIDGED_OVERRIDES};
        for &op in ARB_INSTRUCTION_OVERRIDES {
            let bridged = COMPILED_HOST_BRIDGED_OVERRIDES.contains(&op);
            let refused = bytecode_ineligible(&[op, opcode::STOP])
                == Some(IneligibleReason::InstructionOverride(op));
            assert_ne!(
                bridged, refused,
                "opcode 0x{op:02x} must be exactly one of host-bridged or refused"
            );
        }
        for &op in COMPILED_HOST_BRIDGED_OVERRIDES {
            assert!(
                ARB_INSTRUCTION_OVERRIDES.contains(&op),
                "bridged opcode 0x{op:02x} is not an instruction-table override"
            );
        }
    }

    #[test]
    fn push_immediates_are_not_decoded_as_opcodes() {
        // PUSH1 0x43 STOP — 0x43 is NUMBER if decoded as an opcode, but it is immediate data.
        assert_eq!(
            bytecode_ineligible(&[opcode::PUSH1, opcode::NUMBER, opcode::STOP]),
            None
        );
        // PUSH1 0x40 STOP — 0x40 is BLOCKHASH if decoded as an opcode.
        assert_eq!(
            bytecode_ineligible(&[opcode::PUSH1, opcode::BLOCKHASH, opcode::STOP]),
            None
        );
        // PUSH32 whose payload contains NUMBER and BLOCKHASH bytes, then STOP.
        let mut code = vec![opcode::PUSH32];
        code.extend(std::iter::repeat_n(0x11, 10));
        code.push(opcode::NUMBER);
        code.push(opcode::BLOCKHASH);
        code.extend(std::iter::repeat_n(0x22, 20));
        code.push(opcode::STOP);
        assert_eq!(code.len(), 1 + 32 + 1);
        assert_eq!(bytecode_ineligible(&code), None);
    }

    #[test]
    fn truncated_push_does_not_scan_payload_as_opcodes() {
        // PUSH2 with only one immediate byte 0x43. That byte is still immediate, not NUMBER.
        assert_eq!(bytecode_ineligible(&[opcode::PUSH2, opcode::NUMBER]), None);
    }

    #[test]
    fn number_after_push_is_eligible() {
        assert_eq!(
            bytecode_ineligible(&[opcode::PUSH1, 0x01, opcode::NUMBER, opcode::STOP]),
            None
        );
    }

    #[test]
    fn blockhash_after_push_is_still_detected() {
        assert_eq!(
            bytecode_ineligible(&[opcode::PUSH1, 0x01, opcode::BLOCKHASH, opcode::STOP]),
            Some(IneligibleReason::InstructionOverride(opcode::BLOCKHASH))
        );
    }
}
