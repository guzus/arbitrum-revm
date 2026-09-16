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
        instructions::{EthInstructions, InstructionProvider},
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

/// Instruction providers accepted by Arbitrum execution entry points.
/// Existing mutable EthInstructions retains its original indirect execution path.
pub trait ArbExecutionInstructions<CTX>:
    InstructionProvider<Context = CTX, InterpreterTypes = EthInterpreter>
{
}
impl<CTX: revm::interpreter::Host> ArbExecutionInstructions<CTX>
    for EthInstructions<EthInterpreter, CTX>
{
}
#[cfg(feature = "direct-stack-dispatch")]
impl<CTX: revm::interpreter::Host> ArbExecutionInstructions<CTX>
    for crate::stack_dispatch::CanonicalStackInstructions<CTX>
{
}

impl<CTX, INSP, PRECOMPILE, I> ExecuteEvm for ArbEvm<CTX, INSP, I, PRECOMPILE>
where
    CTX: ArbContextTr + ContextSetters + CompiledFrameCtx,
    PRECOMPILE: PrecompileProvider<CTX, Output = InterpreterResult>,
    I: ArbExecutionInstructions<CTX>,
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
        h.run(self)
    }

    fn finalize(&mut self) -> Self::State {
        self.0.ctx.journal_mut().finalize()
    }

    fn replay(
        &mut self,
    ) -> Result<ExecResultAndState<Self::ExecutionResult, Self::State>, Self::Error> {
        let mut h = ArbHandler::<_, _, EthFrame<EthInterpreter>>::new();
        h.run(self).map(|result| {
            let state = self.finalize();
            ExecResultAndState::new(result, state)
        })
    }
}

impl<CTX, INSP, PRECOMPILE, I> ExecuteCommitEvm for ArbEvm<CTX, INSP, I, PRECOMPILE>
where
    CTX: ArbContextTr<Db: DatabaseCommit> + ContextSetters + CompiledFrameCtx,
    PRECOMPILE: PrecompileProvider<CTX, Output = InterpreterResult>,
    I: ArbExecutionInstructions<CTX>,
{
    fn commit(&mut self, state: Self::State) {
        self.0.ctx.db_mut().commit(state);
    }
}

impl<CTX, INSP, PRECOMPILE, I> InspectEvm for ArbEvm<CTX, INSP, I, PRECOMPILE>
where
    CTX: ArbContextTr<Journal: JournalExt> + ContextSetters + CompiledFrameCtx,
    INSP: Inspector<CTX, EthInterpreter>,
    PRECOMPILE: PrecompileProvider<CTX, Output = InterpreterResult>,
    I: ArbExecutionInstructions<CTX>,
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

impl<CTX, INSP, PRECOMPILE, I> InspectCommitEvm for ArbEvm<CTX, INSP, I, PRECOMPILE>
where
    CTX: ArbContextTr<Journal: JournalExt, Db: DatabaseCommit> + ContextSetters + CompiledFrameCtx,
    INSP: Inspector<CTX, EthInterpreter>,
    PRECOMPILE: PrecompileProvider<CTX, Output = InterpreterResult>,
    I: ArbExecutionInstructions<CTX>,
{
}

impl<CTX, INSP, PRECOMPILE, I> SystemCallEvm for ArbEvm<CTX, INSP, I, PRECOMPILE>
where
    CTX: ArbContextTr<Tx: SystemCallTx> + ContextSetters + CompiledFrameCtx,
    PRECOMPILE: PrecompileProvider<CTX, Output = InterpreterResult>,
    I: ArbExecutionInstructions<CTX>,
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

impl<CTX, INSP, PRECOMPILE, I> InspectSystemCallEvm for ArbEvm<CTX, INSP, I, PRECOMPILE>
where
    CTX: ArbContextTr<Journal: JournalExt, Tx: SystemCallTx> + ContextSetters + CompiledFrameCtx,
    INSP: Inspector<CTX, EthInterpreter>,
    PRECOMPILE: PrecompileProvider<CTX, Output = InterpreterResult>,
    I: ArbExecutionInstructions<CTX>,
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

impl<CTX, INSP, PRECOMPILE, I> SystemCallCommitEvm for ArbEvm<CTX, INSP, I, PRECOMPILE>
where
    CTX: ArbContextTr<Db: DatabaseCommit, Tx: SystemCallTx> + ContextSetters + CompiledFrameCtx,
    PRECOMPILE: PrecompileProvider<CTX, Output = InterpreterResult>,
    I: ArbExecutionInstructions<CTX>,
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
