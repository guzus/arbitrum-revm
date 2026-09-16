use crate::{
    ArbSpecId, chain::ArbChainContext, evm::ArbEvm, handler::ArbHandler, transaction::ArbTxTr,
};
use revm::{
    DatabaseCommit, ExecuteCommitEvm, ExecuteEvm,
    context::{ContextSetters, result::ExecResultAndState},
    context_interface::{
        Cfg, ContextTr, Database, JournalTr,
        result::{EVMError, ExecutionResult, InvalidTransaction},
    },
    handler::{
        EthFrame, Handler, PrecompileProvider, SystemCallTx,
        instructions::EthInstructions,
        system_call::{SystemCallCommitEvm, SystemCallEvm},
    },
    inspector::{
        InspectCommitEvm, InspectEvm, InspectSystemCallEvm, Inspector, InspectorHandler, JournalExt,
    },
    interpreter::{InterpreterResult, interpreter::EthInterpreter},
    primitives::{Address, Bytes},
    state::EvmState,
};

/// Context trait bound used by Arbitrum execution APIs.
pub trait ArbContextTr:
    ContextTr<
        Journal: JournalTr<State = EvmState>,
        Tx: ArbTxTr,
        Cfg: Cfg<Spec = ArbSpecId>,
        Chain = ArbChainContext,
    >
{
}

impl<T> ArbContextTr for T where
    T: ContextTr<
            Journal: JournalTr<State = EvmState>,
            Tx: ArbTxTr,
            Cfg: Cfg<Spec = ArbSpecId>,
            Chain = ArbChainContext,
        >
{
}

/// Extra `ArbEvm` bound used only by compiled-frame dispatch.
///
/// Default-off: implemented for every type, so `ExecuteEvm` / `EvmTr` stay as
/// `CTX: ArbContextTr + …` with no `Host` requirement. With `compiled-frame`,
/// this is `Host` because `EvmCompilerFn::call_with_interpreter` needs it.
#[cfg(feature = "compiled-frame")]
pub trait CompiledFrameCtx: revm::interpreter::Host {}
#[cfg(feature = "compiled-frame")]
impl<T: revm::interpreter::Host> CompiledFrameCtx for T {}

#[cfg(not(feature = "compiled-frame"))]
pub trait CompiledFrameCtx {}
#[cfg(not(feature = "compiled-frame"))]
impl<T> CompiledFrameCtx for T {}

/// Error type for Arbitrum EVM execution.
pub type ArbError<CTX> = EVMError<<<CTX as ContextTr>::Db as Database>::Error, InvalidTransaction>;

impl<CTX, INSP, PRECOMPILE> ExecuteEvm
    for ArbEvm<CTX, INSP, EthInstructions<EthInterpreter, CTX>, PRECOMPILE>
where
    CTX: ArbContextTr + ContextSetters + CompiledFrameCtx,
    PRECOMPILE: PrecompileProvider<CTX, Output = InterpreterResult>,
{
    type Tx = <CTX as ContextTr>::Tx;
    type Block = <CTX as ContextTr>::Block;
    type State = EvmState;
    type Error = ArbError<CTX>;
    type ExecutionResult = ExecutionResult<revm::context_interface::result::HaltReason>;

    fn set_block(&mut self, block: Self::Block) {
        self.0.ctx.set_block(block);
    }

    fn transact_one(&mut self, tx: Self::Tx) -> Result<Self::ExecutionResult, Self::Error> {
        self.0.ctx.set_tx(tx);
        let mut h = ArbHandler::<_, _, EthFrame<EthInterpreter>>::new();
        #[cfg(feature = "phase-timing")]
        if crate::phase_timing::active_attempt() {
            return crate::phase_timing::run(&mut h, self);
        }
        h.run(self)
    }

    // Same order as ExecuteEvm::transact in revm-handler 42.0.1: finalize even
    // when transact_one returns Err. The attempt guard spans both operations.
    #[cfg(feature = "phase-timing")]
    fn transact(
        &mut self,
        tx: Self::Tx,
    ) -> Result<ExecResultAndState<Self::ExecutionResult, Self::State>, Self::Error> {
        use revm::context_interface::Transaction;
        let attempt = crate::phase_timing::AttemptGuard::begin(
            crate::phase_timing::Entry::Transact,
            tx.tx_type(),
            tx.nonce(),
        );
        let result = self.transact_one(tx);
        attempt.outcome(&result);
        let state = self.finalize();
        let result = result?;
        Ok(ExecResultAndState::new(result, state))
    }

    fn finalize(&mut self) -> Self::State {
        #[cfg(feature = "phase-timing")]
        let _phase = crate::phase_timing::PhaseGuard::enter(crate::phase_timing::Phase::Finalize);
        self.0.ctx.journal_mut().finalize()
    }

    fn replay(
        &mut self,
    ) -> Result<ExecResultAndState<Self::ExecutionResult, Self::State>, Self::Error> {
        let mut h = ArbHandler::<_, _, EthFrame<EthInterpreter>>::new();
        #[cfg(feature = "phase-timing")]
        let attempt = {
            use revm::context_interface::Transaction;
            crate::phase_timing::AttemptGuard::begin(
                crate::phase_timing::Entry::Replay,
                self.0.ctx.tx().tx_type(),
                self.0.ctx.tx().nonce(),
            )
        };
        #[cfg(feature = "phase-timing")]
        let result = if crate::phase_timing::active_attempt() {
            crate::phase_timing::run(&mut h, self)
        } else {
            h.run(self)
        };
        #[cfg(not(feature = "phase-timing"))]
        let result = h.run(self);
        #[cfg(feature = "phase-timing")]
        attempt.outcome(&result);
        // Preserve replay's existing success-only finalize, unlike transact.
        result.map(|result| {
            let state = self.finalize();
            ExecResultAndState::new(result, state)
        })
    }
}

impl<CTX, INSP, PRECOMPILE> ExecuteCommitEvm
    for ArbEvm<CTX, INSP, EthInstructions<EthInterpreter, CTX>, PRECOMPILE>
where
    CTX: ArbContextTr<Db: DatabaseCommit> + ContextSetters + CompiledFrameCtx,
    PRECOMPILE: PrecompileProvider<CTX, Output = InterpreterResult>,
{
    fn commit(&mut self, state: Self::State) {
        self.0.ctx.db_mut().commit(state);
    }
}

impl<CTX, INSP, PRECOMPILE> InspectEvm
    for ArbEvm<CTX, INSP, EthInstructions<EthInterpreter, CTX>, PRECOMPILE>
where
    CTX: ArbContextTr<Journal: JournalExt> + ContextSetters + CompiledFrameCtx,
    INSP: Inspector<CTX, EthInterpreter>,
    PRECOMPILE: PrecompileProvider<CTX, Output = InterpreterResult>,
{
    type Inspector = INSP;

    fn set_inspector(&mut self, inspector: Self::Inspector) {
        self.0.inspector = inspector;
    }

    fn inspect_one_tx(&mut self, tx: Self::Tx) -> Result<Self::ExecutionResult, Self::Error> {
        self.0.ctx.set_tx(tx);
        let mut h = ArbHandler::<_, _, EthFrame<EthInterpreter>>::new();
        h.inspect_run(self)
    }
}

impl<CTX, INSP, PRECOMPILE> InspectCommitEvm
    for ArbEvm<CTX, INSP, EthInstructions<EthInterpreter, CTX>, PRECOMPILE>
where
    CTX: ArbContextTr<Journal: JournalExt, Db: DatabaseCommit> + ContextSetters + CompiledFrameCtx,
    INSP: Inspector<CTX, EthInterpreter>,
    PRECOMPILE: PrecompileProvider<CTX, Output = InterpreterResult>,
{
}

impl<CTX, INSP, PRECOMPILE> SystemCallEvm
    for ArbEvm<CTX, INSP, EthInstructions<EthInterpreter, CTX>, PRECOMPILE>
where
    CTX: ArbContextTr<Tx: SystemCallTx> + ContextSetters + CompiledFrameCtx,
    PRECOMPILE: PrecompileProvider<CTX, Output = InterpreterResult>,
{
    fn system_call_one_with_caller(
        &mut self,
        caller: Address,
        system_contract_address: Address,
        data: Bytes,
    ) -> Result<Self::ExecutionResult, Self::Error> {
        self.0.ctx.set_tx(CTX::Tx::new_system_tx_with_caller(
            caller,
            system_contract_address,
            data,
        ));
        let mut h = ArbHandler::<_, _, EthFrame<EthInterpreter>>::new();
        h.run_system_call(self)
    }
}

impl<CTX, INSP, PRECOMPILE> InspectSystemCallEvm
    for ArbEvm<CTX, INSP, EthInstructions<EthInterpreter, CTX>, PRECOMPILE>
where
    CTX: ArbContextTr<Journal: JournalExt, Tx: SystemCallTx> + ContextSetters + CompiledFrameCtx,
    INSP: Inspector<CTX, EthInterpreter>,
    PRECOMPILE: PrecompileProvider<CTX, Output = InterpreterResult>,
{
    fn inspect_one_system_call_with_caller(
        &mut self,
        caller: Address,
        system_contract_address: Address,
        data: Bytes,
    ) -> Result<Self::ExecutionResult, Self::Error> {
        self.0.ctx.set_tx(CTX::Tx::new_system_tx_with_caller(
            caller,
            system_contract_address,
            data,
        ));
        let mut h = ArbHandler::<_, _, EthFrame<EthInterpreter>>::new();
        h.inspect_run_system_call(self)
    }
}

impl<CTX, INSP, PRECOMPILE> SystemCallCommitEvm
    for ArbEvm<CTX, INSP, EthInstructions<EthInterpreter, CTX>, PRECOMPILE>
where
    CTX: ArbContextTr<Db: DatabaseCommit, Tx: SystemCallTx> + ContextSetters + CompiledFrameCtx,
    PRECOMPILE: PrecompileProvider<CTX, Output = InterpreterResult>,
{
    fn system_call_with_caller_commit(
        &mut self,
        caller: Address,
        system_contract_address: Address,
        data: Bytes,
    ) -> Result<Self::ExecutionResult, Self::Error> {
        self.system_call_with_caller(caller, system_contract_address, data)
            .map(|output| {
                self.commit(output.state);
                output.result
            })
    }
}
