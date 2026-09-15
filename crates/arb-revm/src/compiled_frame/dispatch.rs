//! Runtime lookup used by `ArbEvm::frame_run`. Never compiles.

use super::CompiledFrameRegistry;
use revm::{
    handler::EthFrame,
    interpreter::{FrameInput, Host, InterpreterAction, interpreter::EthInterpreter},
};

/// Runs the current frame with a resident compiled program, or `None` to interpret.
///
/// Fallback (caller uses `self.0.frame_run`):
/// - no registry / empty registry (handled by the caller)
/// - create/initcode frames
/// - unknown code hash
/// - spec / gas-table mismatch (handled by the caller before this)
///
/// Stylus frames are filtered in `frame_run` before this is called.
/// Nested frames are checked because the handler loop calls `frame_run` for each.
pub(crate) fn try_execute<H: Host>(
    registry: &CompiledFrameRegistry,
    frame: &mut EthFrame<EthInterpreter>,
    host: &mut H,
) -> Option<InterpreterAction> {
    if matches!(frame.input, FrameInput::Create(_) | FrameInput::Empty) {
        return None;
    }

    let code_hash = frame.interpreter.bytecode.get_or_calculate_hash();
    // `Bytecode::len` is original_len (no JUMPDEST padding). `bytes().len()` includes padding
    // and would miss every compiled program.
    let live_len = frame.interpreter.bytecode.len();
    let func = registry.lookup(code_hash, live_len)?;

    // SAFETY: `func` was produced by `registry.compiler.jit` and the compiler
    // artifact is borrowed here through `registry`. `call_with_interpreter`
    // copies gas into the compiled context, writes it back, and stores the
    // CALL/CREATE resume PC on the interpreter so the next `frame_run` of this
    // frame continues the same compiled function.
    Some(unsafe { func.call_with_interpreter(&mut frame.interpreter, host) })
}
