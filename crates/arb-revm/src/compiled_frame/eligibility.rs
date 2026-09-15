//! Conservative compile eligibility. Decoding skips PUSH (and other) immediates.

use revm::bytecode::opcode::{self, OPCODE_INFO};

/// Why a bytecode image is refused for compilation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IneligibleReason {
    /// Empty runtime code. There is nothing to compile.
    Empty,
    /// `0xEF` prefix: EOF or a Stylus discriminant. Stylus is handled before this
    /// path; EOF is out of scope for the prototype.
    EofOrStylusPrefix,
    /// `NUMBER` (0x43). ArbOS returns the L1 block number; revmc's builtin uses L2.
    NumberOpcode,
    /// `BLOCKHASH` (0x40). ArbOS reads the L1 hash ring; revmc's builtin uses L2 hashes.
    BlockhashOpcode,
}

/// Returns `Some` when `code` must not be compiled.
///
/// Immediate bytes of `PUSH1..=PUSH32` (and any other opcode with an immediate) are
/// skipped so a `PUSH1 0x43` does **not** count as `NUMBER`.
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
        match op {
            opcode::NUMBER => return Some(IneligibleReason::NumberOpcode),
            opcode::BLOCKHASH => return Some(IneligibleReason::BlockhashOpcode),
            _ => {}
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
    fn number_and_blockhash_opcodes_are_ineligible() {
        assert_eq!(
            bytecode_ineligible(&[opcode::NUMBER, opcode::STOP]),
            Some(IneligibleReason::NumberOpcode)
        );
        assert_eq!(
            bytecode_ineligible(&[opcode::BLOCKHASH, opcode::STOP]),
            Some(IneligibleReason::BlockhashOpcode)
        );
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
    fn number_after_push_is_still_detected() {
        assert_eq!(
            bytecode_ineligible(&[opcode::PUSH1, 0x01, opcode::NUMBER, opcode::STOP]),
            Some(IneligibleReason::NumberOpcode)
        );
    }
}
