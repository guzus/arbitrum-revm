use crate::{
    ArbSpecId,
    api::exec::{ArbContextTr, CompiledFrameCtx},
    chain::ArbChainContext,
    precompiles::ArbPrecompiles,
    storage::ArbosState,
};
#[cfg(feature = "compiled-frame")]
use revm::interpreter::InterpreterAction;
use revm::{
    Database, Inspector,
    bytecode::opcode,
    context::{Cfg, ContextError, ContextSetters, Evm, FrameStack},
    context_interface::ContextTr,
    handler::{
        EthFrame, EvmTr, FrameInitOrResult, ItemOrResult, PrecompileProvider,
        evm::FrameTr,
        instructions::{EthInstructions, InstructionProvider},
    },
    inspector::{InspectorEvmTr, JournalExt},
    interpreter::{
        CallScheme, FrameInput, Host, Instruction, InstructionContext, InstructionExecResult,
        InstructionResult, InterpreterResult, interpreter::EthInterpreter,
    },
    primitives::{Address, B256, U256},
};

/// Arbitrum `NUMBER` opcode: returns the L1 block number, not the L2 block number.
///
/// Mirrors Nitro's patched `opNumber` (`go-ethereum/core/vm/instructions.go`), which
/// reads `ProcessingHook.L1BlockNumber` rather than `BlockContext.BlockNumber`. The
/// value is carried block-scoped in [`ArbChainContext::l1_block_number`].
fn arb_block_number<CTX>(ctx: InstructionContext<'_, CTX, EthInterpreter>) -> InstructionExecResult
where
    CTX: ContextTr<Chain = ArbChainContext> + Host,
{
    let l1_block_number = ctx.host.chain().l1_block_number;
    if !ctx.interpreter.stack.push(U256::from(l1_block_number)) {
        return Err(revm::interpreter::InstructionResult::StackOverflow);
    }
    Ok(())
}

/// Arbitrum `BLOCKHASH` opcode: returns the hash of an **L1** block from the ArbOS
/// block-hash ring buffer, not an L2 header hash.
///
/// Mirrors Nitro's patched `GetHashFn` (`go-ethereum`), which reads `state.blockhashes`
/// (the last 256 L1 block hashes recorded by `Blockhashes.RecordNewL1Block`). The stock
/// revm instruction compares the requested number against the **L2** block number and so
/// returns 0 for any L1 number, diverging the first time a tx reads `BLOCKHASH(l1Num)`. The internal
/// ArbOS-storage read is unmetered (matching geth's free `GetHashFn`); only the fixed
/// 20-gas table cost applies.
fn arb_block_hash<CTX>(ctx: InstructionContext<'_, CTX, EthInterpreter>) -> InstructionExecResult
where
    CTX: ContextTr<Chain = ArbChainContext> + Host,
{
    let Some(([], number)) = ctx.interpreter.stack.popn_top::<0>() else {
        return Err(InstructionResult::StackUnderflow);
    };
    let requested = u64::try_from(*number).unwrap_or(u64::MAX);
    let hash = ArbosState::open()
        .block_hashes
        .block_hash(requested, ctx.host.journal_mut())
        .unwrap_or(B256::ZERO);
    *number = U256::from_be_bytes(hash.0);
    Ok(())
}

/// Opcodes whose instruction-table entries `ArbEvm::new` replaces.
///
/// Compiled frames skip the instruction table, so these must be refused by
/// [`crate::compiled_frame::bytecode_ineligible`]. Kept next to the
/// `insert_instruction` calls. Sorted numerically. The table-diff test in this
/// file fails if `ArbEvm::new` grows another override without updating this list.
pub const ARB_INSTRUCTION_OVERRIDES: &[u8] = &[opcode::BLOCKHASH, opcode::NUMBER];

/// Arbitrum EVM wrapper over revm's generic [`Evm`] type.
///
/// The optional second field is the compiled-frame registry. It is present only
/// with `--features compiled-frame` and is `None` unless a caller attaches one.
/// Missing/empty registries execute the original interpreter path.
#[derive(Debug, Clone)]
pub struct ArbEvm<
    CTX,
    INSP,
    I = EthInstructions<EthInterpreter, CTX>,
    P = ArbPrecompiles,
    F = EthFrame<EthInterpreter>,
>(
    pub Evm<CTX, INSP, I, P, F>,
    #[cfg(feature = "compiled-frame")]
    Option<std::sync::Arc<crate::compiled_frame::CompiledFrameRegistry>>,
    #[cfg(feature = "direct-stack-dispatch")] Option<crate::stack_dispatch::FrameRunner<CTX, I>>,
);

impl<CTX, INSP> ArbEvm<CTX, INSP, EthInstructions<EthInterpreter, CTX>, ArbPrecompiles>
where
    CTX: ContextTr<Cfg: Cfg<Spec: Into<ArbSpecId> + Clone>, Chain = ArbChainContext> + Host,
{
    /// Creates a new Arbitrum EVM.
    pub fn new(ctx: CTX, inspector: INSP) -> Self {
        let spec: ArbSpecId = ctx.cfg().spec().into();
        let instruction = canonical_arb_instructions(spec);
        Self::from_inner(Evm {
            ctx,
            inspector,
            instruction,
            precompiles: ArbPrecompiles::new_with_spec(spec),
            frame_stack: FrameStack::new_prealloc(8),
        })
    }

    /// Consumes self and returns the inner context.
    pub fn into_context(self) -> CTX {
        self.0.ctx
    }
}

#[cfg(feature = "direct-stack-dispatch")]
impl<CTX, INSP>
    ArbEvm<CTX, INSP, crate::stack_dispatch::CanonicalStackInstructions<CTX>, ArbPrecompiles>
where
    CTX: ContextTr<Cfg: Cfg<Spec: Into<ArbSpecId> + Clone>, Chain = ArbChainContext> + Host,
{
    /// Explicit experiment constructor. Its immutable provider is built canonically here.
    pub fn new_canonical(
        ctx: CTX,
        inspector: INSP,
        mode: crate::stack_dispatch::StackDispatchMode,
    ) -> Self {
        let spec: ArbSpecId = ctx.cfg().spec().into();
        Self(
            Evm {
                ctx,
                inspector,
                instruction: crate::stack_dispatch::CanonicalStackInstructions::new(spec, mode),
                precompiles: ArbPrecompiles::new_with_spec(spec),
                frame_stack: FrameStack::new_prealloc(8),
            },
            #[cfg(feature = "compiled-frame")]
            None,
            Some(crate::stack_dispatch::run_frame::<CTX>),
        )
    }
}

impl<CTX, INSP, I, P> ArbEvm<CTX, INSP, I, P> {
    /// Wraps an inner revm [`Evm`]. With `compiled-frame` the registry starts empty
    /// (interpreter path). Prefer this over `ArbEvm(inner)` from other crates: enabling
    /// the feature adds a private field and the one-argument tuple constructor stops
    /// compiling outside this crate.
    pub fn from_inner(
        inner: Evm<CTX, INSP, I, P, EthFrame<EthInterpreter>>,
    ) -> ArbEvm<CTX, INSP, I, P> {
        ArbEvm(
            inner,
            #[cfg(feature = "compiled-frame")]
            None,
            #[cfg(feature = "direct-stack-dispatch")]
            None,
        )
    }

    #[cfg(feature = "direct-stack-dispatch")]
    fn stack_runner(&self) -> Option<crate::stack_dispatch::FrameRunner<CTX, I>> {
        #[cfg(feature = "compiled-frame")]
        {
            self.2
        }
        #[cfg(not(feature = "compiled-frame"))]
        {
            self.1
        }
    }

    /// Consumes self and returns a new EVM with a different inspector.
    pub fn with_inspector<OINSP>(self, inspector: OINSP) -> ArbEvm<CTX, OINSP, I, P> {
        #[cfg(feature = "direct-stack-dispatch")]
        let runner = self.stack_runner();
        ArbEvm(
            self.0.with_inspector(inspector),
            #[cfg(feature = "compiled-frame")]
            self.1,
            #[cfg(feature = "direct-stack-dispatch")]
            runner,
        )
    }

    /// Consumes self and returns a new EVM with a different precompile provider.
    pub fn with_precompiles<OP>(self, precompiles: OP) -> ArbEvm<CTX, INSP, I, OP> {
        #[cfg(feature = "direct-stack-dispatch")]
        let runner = self.stack_runner();
        ArbEvm(
            self.0.with_precompiles(precompiles),
            #[cfg(feature = "compiled-frame")]
            self.1,
            #[cfg(feature = "direct-stack-dispatch")]
            runner,
        )
    }

    /// Attaches a warm-compiled program registry. No-op type when the feature is off.
    ///
    /// `None` or an empty registry keeps the original interpreter path. Do not compile
    /// from this method; compile on the registry before sharing it.
    #[cfg(feature = "compiled-frame")]
    pub fn set_compiled_programs(
        &mut self,
        registry: Option<std::sync::Arc<crate::compiled_frame::CompiledFrameRegistry>>,
    ) {
        self.1 = registry;
    }

    /// Returns the attached compiled-frame registry, if any.
    #[cfg(feature = "compiled-frame")]
    pub fn compiled_programs(
        &self,
    ) -> Option<&std::sync::Arc<crate::compiled_frame::CompiledFrameRegistry>> {
        self.1.as_ref()
    }

    /// Consumes self and returns the inner inspector.
    pub fn into_inspector(self) -> INSP {
        self.0.into_inspector()
    }
}

impl<CTX, INSP, I, P> InspectorEvmTr for ArbEvm<CTX, INSP, I, P>
where
    CTX: ArbContextTr<Journal: JournalExt> + ContextSetters,
    I: InstructionProvider<Context = CTX, InterpreterTypes = EthInterpreter>,
    P: PrecompileProvider<CTX, Output = InterpreterResult>,
    INSP: Inspector<CTX, I::InterpreterTypes>,
{
    type Inspector = INSP;

    fn all_inspector(
        &self,
    ) -> (
        &Self::Context,
        &Self::Instructions,
        &Self::Precompiles,
        &FrameStack<Self::Frame>,
        &Self::Inspector,
    ) {
        self.0.all_inspector()
    }

    fn all_mut_inspector(
        &mut self,
    ) -> (
        &mut Self::Context,
        &mut Self::Instructions,
        &mut Self::Precompiles,
        &mut FrameStack<Self::Frame>,
        &mut Self::Inspector,
    ) {
        self.0.all_mut_inspector()
    }
}

#[cfg(feature = "compiled-frame")]
impl<CTX, INSP, I, P> ArbEvm<CTX, INSP, I, P, EthFrame<EthInterpreter>>
where
    CTX: ArbContextTr + Host,
    I: InstructionProvider<Context = CTX, InterpreterTypes = EthInterpreter>,
    P: PrecompileProvider<CTX, Output = InterpreterResult>,
{
    fn try_compiled_frame_action(&mut self) -> Option<InterpreterAction> {
        let registry = self.1.as_ref()?;
        if registry.is_empty() {
            return None;
        }
        let spec = self.0.ctx.cfg().spec();
        if !registry.accepts_context(spec, self.0.ctx.cfg().gas_params()) {
            return None;
        }
        crate::compiled_frame::try_execute(registry, self.0.frame_stack.get(), &mut self.0.ctx)
    }
}

impl<CTX, INSP, I, P> EvmTr for ArbEvm<CTX, INSP, I, P, EthFrame<EthInterpreter>>
where
    CTX: ArbContextTr + CompiledFrameCtx,
    I: InstructionProvider<Context = CTX, InterpreterTypes = EthInterpreter>,
    P: PrecompileProvider<CTX, Output = InterpreterResult>,
{
    type Context = CTX;
    type Instructions = I;
    type Precompiles = P;
    type Frame = EthFrame<EthInterpreter>;

    fn all(
        &self,
    ) -> (
        &Self::Context,
        &Self::Instructions,
        &Self::Precompiles,
        &FrameStack<Self::Frame>,
    ) {
        self.0.all()
    }

    fn all_mut(
        &mut self,
    ) -> (
        &mut Self::Context,
        &mut Self::Instructions,
        &mut Self::Precompiles,
        &mut FrameStack<Self::Frame>,
    ) {
        self.0.all_mut()
    }

    fn frame_init(
        &mut self,
        frame_input: <Self::Frame as FrameTr>::FrameInit,
    ) -> Result<
        ItemOrResult<&mut Self::Frame, <Self::Frame as FrameTr>::FrameResult>,
        ContextError<<<Self::Context as ContextTr>::Db as Database>::Error>,
    > {
        // Track per-address open call-frame spans for the Stylus `reentrant` flag (Nitro
        // `TxProcessor.PushContract`). Only count when a frame is actually pushed (`Item`);
        // fast-path results (precompiles, failed pre-checks) never open a geth contract frame.
        let span = span_address(&frame_input.frame_input);
        match self.0.frame_init(frame_input)? {
            ItemOrResult::Item(_) => {}
            ItemOrResult::Result(result) => return Ok(ItemOrResult::Result(result)),
        }
        if let Some(address) = span {
            *self
                .0
                .ctx
                .chain_mut()
                .stylus_program_spans
                .entry(address)
                .or_insert(0) += 1;
        }
        // The pushed frame is the top of the stack, which is exactly what the inner
        // `frame_init` returned for the `Item` case.
        Ok(ItemOrResult::Item(self.0.frame_stack.get()))
    }

    fn frame_run(
        &mut self,
    ) -> Result<
        FrameInitOrResult<Self::Frame>,
        ContextError<<<Self::Context as ContextTr>::Db as Database>::Error>,
    > {
        // Arbitrum: if the current frame runs a Stylus program (bytecode carries the Stylus
        // discriminant), execute it as WASM instead of dispatching to the EVM interpreter.
        #[cfg(feature = "stylus")]
        if crate::stylus::program::is_stylus_program(
            &self.0.frame_stack.get().interpreter.bytecode.bytes(),
            self.0.ctx.cfg().spec().arbos_version(),
        ) && let Some(action) = self.frame_run_stylus()
        {
            let frame = self.0.frame_stack.get();
            let context = &mut self.0.ctx;
            return frame.process_next_action(context, action).inspect(|next| {
                if next.is_result() {
                    frame.set_finished(true);
                }
            });
        }
        // Optional compiled-program dispatch. Lookup only — never compiles here.
        // Nested frames are checked because the handler loop calls `frame_run` for each.
        #[cfg(feature = "compiled-frame")]
        if let Some(action) = self.try_compiled_frame_action() {
            let frame = self.0.frame_stack.get();
            let context = &mut self.0.ctx;
            return frame.process_next_action(context, action).inspect(|next| {
                if next.is_result() {
                    frame.set_finished(true);
                }
            });
        }
        #[cfg(feature = "direct-stack-dispatch")]
        {
            let runner = self.stack_runner();
            let inner = &mut self.0;
            if let Some(run) = runner {
                let frame = inner.frame_stack.get();
                let action = run(
                    &mut frame.interpreter,
                    &mut inner.instruction,
                    &mut inner.ctx,
                );
                return frame
                    .process_next_action(&mut inner.ctx, action)
                    .inspect(|next| {
                        if next.is_result() {
                            frame.set_finished(true);
                        }
                    });
            }
        }
        self.0.frame_run()
    }

    fn frame_return_result(
        &mut self,
        result: <Self::Frame as FrameTr>::FrameResult,
    ) -> Result<
        Option<<Self::Frame as FrameTr>::FrameResult>,
        ContextError<<<Self::Context as ContextTr>::Db as Database>::Error>,
    > {
        // The inner call pops the top frame iff it is finished (Nitro `PopContract`);
        // close its span first. Fast-path results arriving here belong to inits that never
        // pushed a frame - then the top frame is the still-running parent (not finished).
        let span = {
            let frame = self.0.frame_stack.get();
            if frame.is_finished() {
                span_address(&frame.input)
            } else {
                None
            }
        };
        if let Some(address) = span
            && let Some(count) = self
                .0
                .ctx
                .chain_mut()
                .stylus_program_spans
                .get_mut(&address)
        {
            *count = count.saturating_sub(1);
        }
        self.0.frame_return_result(result)
    }
}

pub(crate) fn canonical_arb_instructions<CTX>(
    spec: ArbSpecId,
) -> EthInstructions<EthInterpreter, CTX>
where
    CTX: ContextTr<Chain = ArbChainContext> + Host,
{
    let mut instruction = EthInstructions::new_mainnet_with_spec(spec.into());
    // Arbitrum overrides NUMBER (returns the L1 block number) and BLOCKHASH (returns the
    // ArbOS-stored L1 block hash).
    instruction.insert_instruction(opcode::NUMBER, Instruction::new(arb_block_number::<CTX>), 2);
    instruction.insert_instruction(
        opcode::BLOCKHASH,
        Instruction::new(arb_block_hash::<CTX>),
        20,
    );
    debug_assert_eq!(
        ARB_INSTRUCTION_OVERRIDES,
        &[opcode::BLOCKHASH, opcode::NUMBER]
    );
    instruction
}

/// The acting address whose context span a new frame opens, per Nitro's `PushContract`:
/// non-delegate call frames act as their target address; DELEGATECALL/CALLCODE frames act as
/// the parent's already-open address and are not counted; create frames are exempt (see
/// [`ArbChainContext::stylus_program_spans`]).
fn span_address(input: &FrameInput) -> Option<Address> {
    match input {
        FrameInput::Call(call)
            if !matches!(call.scheme, CallScheme::DelegateCall | CallScheme::CallCode) =>
        {
            Some(call.target_address)
        }
        _ => None,
    }
}

#[cfg(test)]
mod instruction_table_diff {
    use super::ARB_INSTRUCTION_OVERRIDES;
    use crate::{ArbBuilder, ArbContext, ArbSpecId, DefaultArb};
    use revm::{
        context::CfgEnv,
        database::{CacheDB, EmptyDB},
        handler::instructions::EthInstructions,
        interpreter::{Instruction, interpreter::EthInterpreter},
    };
    use std::mem::{size_of, size_of_val};

    fn instruction_fn_addr<H>(inst: Instruction<EthInterpreter, H>) -> usize {
        assert_eq!(size_of_val(&inst), size_of::<usize>());
        // Instruction is a single function pointer; used only to detect table patches.
        unsafe { std::mem::transmute_copy(&inst) }
    }

    #[test]
    fn arb_overrides_are_exactly_number_and_blockhash() {
        let mut db = CacheDB::new(EmptyDB::default());
        let ctx = ArbContext::arb()
            .with_db(&mut db)
            .with_cfg(CfgEnv::new_with_spec(ArbSpecId::NITRO));
        let evm = ctx.build_arb();
        let spec = ArbSpecId::NITRO.into_eth_spec();
        type Host<'a> = ArbContext<&'a mut CacheDB<EmptyDB>>;
        let baseline = EthInstructions::<EthInterpreter, Host<'_>>::new_mainnet_with_spec(spec);

        let arb_ins = evm.0.instruction.instruction_table();
        let base_ins = baseline.instruction_table();
        let arb_gas = evm.0.instruction.gas_table();
        let base_gas = baseline.gas_table();

        let mut diffs = Vec::new();
        for op in 0u8..=255 {
            let fn_diff = instruction_fn_addr(arb_ins[op as usize])
                != instruction_fn_addr(base_ins[op as usize]);
            let gas_diff = arb_gas[op as usize] != base_gas[op as usize];
            if fn_diff || gas_diff {
                diffs.push(op);
            }
        }
        assert_eq!(
            diffs.as_slice(),
            ARB_INSTRUCTION_OVERRIDES,
            "every instruction-table/gas-table difference must be in ARB_INSTRUCTION_OVERRIDES"
        );
    }
}
