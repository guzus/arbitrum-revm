//! Isolated, default-off compiled-frame dispatch for ArbOS replay research.
//!
//! This module is compiled only with `--features compiled-frame`. It does **not**
//! replace [`crate::handler::ArbHandler`] or wrap the EVM in upstream `JitEvm`
//! (those entrypoints use `MainnetHandler` and drop poster fees, retryables, and
//! ArbOS precompiles). Dispatch is an optional lookup in [`crate::evm::ArbEvm::frame_run`]
//! after Stylus handling and before the interpreter.
//!
//! Artifact ownership: [`CompiledFrameRegistry`] owns the LLVM compiler module that
//! produced every function pointer it hands to the frame. There is no API that accepts
//! an arbitrary `EvmCompilerFn` / raw pointer.

mod dispatch;
mod eligibility;
mod host;
mod registry;

pub use eligibility::{IneligibleReason, bytecode_ineligible};
pub use registry::{
    COMPILER_IDENTITY, CompiledFrameError, CompiledFrameIdentity, CompiledFrameRegistry,
};

pub(crate) use dispatch::try_execute;
