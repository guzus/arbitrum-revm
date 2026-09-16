//! Default-off canonical-table stack dispatch experiment. No mutable table escape.
use crate::{ArbEvm, ArbSpecId, chain::ArbChainContext, precompiles::ArbPrecompiles};
use revm::{
    context_interface::ContextTr,
    handler::instructions::{EthInstructions, InstructionProvider},
    interpreter::{
        Host, InstructionContext, InstructionResult, Interpreter, InterpreterAction,
        instructions::{GasTable, InstructionTable, stack},
        interpreter::EthInterpreter,
        interpreter_types::{Jumps, LoopControl},
    },
};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum StackDispatchMode {
    #[default]
    Indirect,
    Direct,
    /// Same kernel without per-opcode counting; direct_steps is unavailable (stored as zero).
    DirectUncounted,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StackDispatchStats {
    pub direct_frames: u64,
    /// Hot opcode calls after static gas was charged, including calls returning errors.
    /// Always zero in DirectUncounted: unavailable, not evidence of no direct execution.
    pub direct_steps: u64,
}

/// Immutable canonical Arb table. No API accepts custom tables or exposes mutable entries.
/// Create the EVM with `ArbEvm::new_canonical`; ordinary `from_inner` stays indirect.
#[derive(Debug)]
pub struct CanonicalStackInstructions<CTX> {
    inner: EthInstructions<EthInterpreter, CTX>,
    mode: StackDispatchMode,
    stats: StackDispatchStats,
}

impl<CTX: Host> Clone for CanonicalStackInstructions<CTX> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
            mode: self.mode,
            // A clone begins a new evidence scope; it must not inherit dispatch hits.
            stats: StackDispatchStats::default(),
        }
    }
}
impl<CTX: Host + ContextTr<Chain = ArbChainContext>> CanonicalStackInstructions<CTX> {
    pub(crate) fn new(spec: ArbSpecId, mode: StackDispatchMode) -> Self {
        Self {
            inner: crate::evm::canonical_arb_instructions(spec),
            mode,
            stats: StackDispatchStats::default(),
        }
    }
}
impl<CTX> CanonicalStackInstructions<CTX> {
    pub fn stats(&self) -> StackDispatchStats {
        self.stats
    }
    pub fn mode(&self) -> StackDispatchMode {
        self.mode
    }
}
impl<CTX: Host> InstructionProvider for CanonicalStackInstructions<CTX> {
    type Context = CTX;
    type InterpreterTypes = EthInterpreter;
    fn instruction_table(&self) -> &InstructionTable<EthInterpreter, CTX> {
        self.inner.instruction_table()
    }
    fn gas_table(&self) -> &GasTable {
        self.inner.gas_table()
    }
}

pub type CanonicalArbEvm<CTX, INSP = ()> =
    ArbEvm<CTX, INSP, CanonicalStackInstructions<CTX>, ArbPrecompiles>;
pub(crate) type FrameRunner<CTX, I> = fn(&mut Interpreter, &mut I, &mut CTX) -> InterpreterAction;

pub(crate) fn run_frame<CTX: Host>(
    interpreter: &mut Interpreter,
    instructions: &mut CanonicalStackInstructions<CTX>,
    host: &mut CTX,
) -> InterpreterAction {
    if instructions.mode == StackDispatchMode::Indirect {
        return interpreter.run_plain(
            instructions.instruction_table(),
            instructions.gas_table(),
            host,
        );
    }
    let (action, hits) = match instructions.mode {
        StackDispatchMode::Direct => run_direct::<true, _>(
            interpreter,
            instructions.instruction_table(),
            instructions.gas_table(),
            host,
        ),
        StackDispatchMode::DirectUncounted => run_direct::<false, _>(
            interpreter,
            instructions.instruction_table(),
            instructions.gas_table(),
            host,
        ),
        StackDispatchMode::Indirect => unreachable!("indirect mode returned above"),
    };
    instructions.stats.direct_frames += 1;
    instructions.stats.direct_steps += hits;
    action
}

// Matches upstream Interpreter::step/run_plain ordering exactly. Only selected
// canonical stack functions become direct calls; all other slots keep the Arb table.
fn run_direct<const COUNT: bool, CTX: Host>(
    interpreter: &mut Interpreter,
    table: &InstructionTable<EthInterpreter, CTX>,
    gas: &GasTable,
    host: &mut CTX,
) -> (InterpreterAction, u64) {
    let mut hits = 0u64;
    let error = loop {
        let op = interpreter.bytecode.opcode();
        // Upstream bytecode analysis pads truncated PUSH data and the terminal STOP.
        interpreter.bytecode.relative_jump(1);
        if interpreter.gas.record_cost_unsafe(gas[op as usize] as u64) {
            break InstructionResult::OutOfGas;
        }
        let ctx = InstructionContext { interpreter, host };
        let result = match op {
            0x50 => {
                if COUNT {
                    hits += 1;
                }
                stack::pop(ctx)
            }
            0x60 => {
                if COUNT {
                    hits += 1;
                }
                stack::push::<1, _, _>(ctx)
            }
            0x61 => {
                if COUNT {
                    hits += 1;
                }
                stack::push::<2, _, _>(ctx)
            }
            0x80 => {
                if COUNT {
                    hits += 1;
                }
                stack::dup::<1, _, _>(ctx)
            }
            0x81 => {
                if COUNT {
                    hits += 1;
                }
                stack::dup::<2, _, _>(ctx)
            }
            0x82 => {
                if COUNT {
                    hits += 1;
                }
                stack::dup::<3, _, _>(ctx)
            }
            0x90 => {
                if COUNT {
                    hits += 1;
                }
                stack::swap::<1, _, _>(ctx)
            }
            0x91 => {
                if COUNT {
                    hits += 1;
                }
                stack::swap::<2, _, _>(ctx)
            }
            _ => table[op as usize].execute(ctx),
        };
        if let Err(error) = result {
            break error;
        }
    };
    if interpreter.bytecode.action().is_none() {
        interpreter.halt(error);
    }
    debug_assert!(interpreter.bytecode.is_end());
    (interpreter.take_next_action(), hits)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ArbChainContext, ArbTransaction};
    use revm::{
        Context, ExecuteEvm, InspectEvm, MainContext,
        bytecode::Bytecode,
        context::{BlockEnv, CfgEnv},
        database::InMemoryDB,
        inspector::NoOpInspector,
        interpreter::{
            Gas, Instruction, InstructionExecResult,
            host::DummyHost,
            instructions::{gas_table, instruction_table},
            interpreter::ExtBytecode,
        },
        primitives::{Address, Bytes, TxKind, U256},
        state::AccountInfo,
    };

    fn interpreter(code: &[u8], gas: u64, stack: &[U256]) -> Interpreter {
        let mut i = Interpreter::default_ext();
        i.bytecode = ExtBytecode::new(Bytecode::new_raw(Bytes::copy_from_slice(code)));
        i.gas = Gas::new(gas);
        i.stack.data_mut().extend_from_slice(stack);
        i
    }

    fn compare(code: &[u8], gas: u64, stack: &[U256]) {
        let table = instruction_table::<EthInterpreter, DummyHost>();
        let costs = gas_table();
        let mut baseline = interpreter(code, gas, stack);
        let mut direct = interpreter(code, gas, stack);
        let expected = baseline.run_plain(&table, &costs, &mut DummyHost::default());
        let (actual, _) =
            run_direct::<true, _>(&mut direct, &table, &costs, &mut DummyHost::default());
        let mut uncounted = interpreter(code, gas, stack);
        let (without_counts, hits) =
            run_direct::<false, _>(&mut uncounted, &table, &costs, &mut DummyHost::default());
        assert_eq!(without_counts, actual);
        assert_eq!(hits, 0);
        assert_eq!(uncounted.stack.data(), direct.stack.data());
        assert_eq!(uncounted.bytecode.pc(), direct.bytecode.pc());
        assert_eq!(uncounted.gas, direct.gas);
        assert_eq!(actual, expected, "code={code:x?} gas={gas}");
        assert_eq!(direct.stack.data(), baseline.stack.data());
        assert_eq!(direct.bytecode.pc(), baseline.bytecode.pc());
        assert_eq!(direct.gas, baseline.gas);
    }

    #[test]
    fn truncated_push_and_exact_static_gas_match() {
        for code in [
            &[0x60][..],
            &[0x61],
            &[0x61, 0xab],
            &[0x60, 0x42, 0x00],
            &[0x61, 1, 2, 0],
        ] {
            for gas in [0, 1, 2, 3, 4, 100] {
                compare(code, gas, &[]);
            }
        }
        for gas in [0, 1, 2, 3] {
            compare(&[0x50, 0], gas, &[U256::from(1)]);
        }
    }

    #[test]
    fn stack_bounds_and_mixed_dispatch_match() {
        for op in [0x50, 0x60, 0x61, 0x80, 0x81, 0x82, 0x90, 0x91] {
            for count in [0, 1, 2, 3, 1023, 1024] {
                compare(&[op, 0, 0, 0], 1000, &vec![U256::from(7); count]);
            }
        }
        compare(&[0x60, 1, 0x60, 2, 0x01, 0x80, 0x90, 0x50, 0], 100, &[]);
        compare(&[0x60, 0, 0x56], 100, &[]); // invalid JUMP action/halt path
    }

    fn db(code: &[u8], child: Option<&[u8]>) -> InMemoryDB {
        let mut db = InMemoryDB::default();
        for (address, code) in [
            (Address::from([0x11; 20]), Some(code)),
            (Address::from([0x22; 20]), child),
        ] {
            if let Some(code) = code {
                db.insert_account_info(
                    address,
                    AccountInfo {
                        code: Some(Bytecode::new_raw(Bytes::copy_from_slice(code))),
                        ..Default::default()
                    },
                );
            }
        }
        db
    }

    fn tx() -> ArbTransaction<revm::context::TxEnv> {
        let mut tx = ArbTransaction::default();
        tx.base.kind = TxKind::Call(Address::from([0x11; 20]));
        tx.base.gas_limit = 1_000_000;
        tx
    }

    macro_rules! context {
        ($db:expr) => {
            Context::mainnet()
                .with_tx(ArbTransaction::default())
                .with_cfg(CfgEnv::new_with_spec(ArbSpecId::NITRO))
                .with_block(BlockEnv {
                    basefee: 0,
                    ..Default::default()
                })
                .with_chain(ArbChainContext::default())
                .with_db($db)
        };
    }

    #[test]
    fn nested_frames_resume_and_full_results_match() {
        // CALL a child, then resume hot instructions in the parent.
        let mut parent = vec![0x60, 0, 0x60, 0, 0x60, 0, 0x60, 0, 0x60, 0, 0x73];
        parent.extend_from_slice(&[0x22; 20]);
        parent.extend_from_slice(&[0x62, 1, 0x86, 0xa0, 0xf1, 0x50, 0x60, 9, 0x50, 0]);
        let child = [0x60, 1, 0x60, 2, 0x01, 0x50, 0];
        let mut baseline = CanonicalArbEvm::new_canonical(
            context!(db(&parent, Some(&child))),
            (),
            StackDispatchMode::Indirect,
        );
        let mut direct = CanonicalArbEvm::new_canonical(
            context!(db(&parent, Some(&child))),
            (),
            StackDispatchMode::Direct,
        )
        .with_precompiles(ArbPrecompiles::new_with_spec(ArbSpecId::NITRO));
        let expected = baseline.transact(tx()).unwrap();
        let actual = direct.transact(tx()).unwrap();
        assert!(actual.result.is_success(), "{actual:?}");
        assert_eq!(actual, expected);
        let mut uncounted = CanonicalArbEvm::new_canonical(
            context!(db(&parent, Some(&child))),
            (),
            StackDispatchMode::DirectUncounted,
        );
        assert_eq!(uncounted.transact(tx()).unwrap(), actual);
        assert_eq!(
            uncounted.0.instruction.mode(),
            StackDispatchMode::DirectUncounted
        );
        assert_eq!(uncounted.0.instruction.stats().direct_steps, 0);
        assert_eq!(
            uncounted.0.instruction.stats().direct_frames,
            direct.0.instruction.stats().direct_frames
        );
        assert_eq!(
            baseline.0.instruction.stats(),
            StackDispatchStats::default()
        );
        assert!(direct.0.instruction.stats().direct_frames >= 3); // parent, child, resumed parent
        assert!(direct.0.instruction.stats().direct_steps >= 10);
        assert_eq!(
            direct.0.instruction.clone().stats(),
            StackDispatchStats::default()
        );
    }

    #[test]
    fn inspector_and_from_inner_do_not_direct_dispatch() {
        let code = [0x60, 1, 0x50, 0];
        let mut inspected = CanonicalArbEvm::new_canonical(
            context!(db(&code, None)),
            (),
            StackDispatchMode::Direct,
        )
        .with_inspector(NoOpInspector {});
        assert!(inspected.inspect_tx(tx()).unwrap().result.is_success());
        assert_eq!(
            inspected.0.instruction.stats(),
            StackDispatchStats::default()
        );
        let mut uncounted = CanonicalArbEvm::new_canonical(
            context!(db(&code, None)),
            NoOpInspector {},
            StackDispatchMode::DirectUncounted,
        );
        assert!(uncounted.inspect_tx(tx()).unwrap().result.is_success());
        assert_eq!(
            uncounted.0.instruction.stats(),
            StackDispatchStats::default()
        );

        let canonical = CanonicalArbEvm::new_canonical(
            context!(db(&code, None)),
            (),
            StackDispatchMode::Direct,
        );
        let mut wrapped = ArbEvm::from_inner(canonical.0);
        assert!(wrapped.transact(tx()).unwrap().result.is_success());
        assert_eq!(wrapped.0.instruction.stats(), StackDispatchStats::default());
    }

    #[test]
    fn mutable_custom_instruction_table_keeps_its_override() {
        fn custom<CTX: Host + ContextTr<Chain = ArbChainContext>>(
            ctx: InstructionContext<'_, CTX, EthInterpreter>,
        ) -> InstructionExecResult {
            ctx.host.chain_mut().l1_block_number = 4242;
            ctx.interpreter.bytecode.relative_jump(1);
            Ok(())
        }
        let mut evm = ArbEvm::new(context!(db(&[0x60, 1, 0], None)), ());
        evm.0
            .instruction
            .insert_instruction(0x60, Instruction::new(custom), 3);
        assert!(evm.transact(tx()).unwrap().result.is_success());
        assert_eq!(evm.0.ctx.chain.l1_block_number, 4242);
    }
}
