//! Ownership-safe compiler artifact. The LLVM module outlives every function pointer.

use super::eligibility::{IneligibleReason, bytecode_ineligible_with_ring};
use crate::ArbSpecId;
use revm::{
    context_interface::cfg::GasParams,
    primitives::{B256, keccak256},
};
use revmc::{BlockHashSemantics, CompileTimings, EvmCompiler, EvmCompilerFn, EvmLlvmBackend};
use std::{
    collections::HashMap,
    fmt,
    sync::atomic::{AtomicU64, Ordering},
};

/// Bound compiler/runtime identity stored with every registry instance.
///
/// Lookup also requires the live frame's `ArbSpecId` and `GasParams` to match the
/// values this instance compiled against. That is the cache key: code hash plus this
/// immutable instance context (ArbOS spec, eth spec, target, compiler, runtime, gas).
pub const COMPILER_IDENTITY: &str = "revmc-6f8854dc/arbos-l1-ring-blockhash-v1/in-process-owned-jit/gas-metered/single-error-off/stack-checks-on";

/// Failure from constructing a registry or compiling a program.
#[derive(Debug)]
pub enum CompiledFrameError {
    /// Bytecode is refused before any LLVM work.
    Ineligible(IneligibleReason),
    /// LLVM / revmc failure. The string is the underlying `eyre` report.
    Compiler(String),
}

impl fmt::Display for CompiledFrameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Ineligible(reason) => {
                write!(f, "bytecode ineligible for compilation: {reason:?}")
            }
            Self::Compiler(err) => write!(f, "revmc compiler error: {err}"),
        }
    }
}

impl std::error::Error for CompiledFrameError {}

/// Immutable compiler/runtime context bound to one registry.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CompiledFrameIdentity {
    /// ArbOS spec this instance compiles and will dispatch for.
    pub spec: ArbSpecId,
    /// Ethereum spec revmc receives (`ArbSpecId::into_eth_spec`).
    pub eth_spec: revm::primitives::hardfork::SpecId,
    /// Host architecture compiled for.
    pub target_arch: &'static str,
    /// Host OS compiled for.
    pub target_os: &'static str,
    /// Immutable compiler/host BLOCKHASH contract; part of the cache identity.
    pub block_hash_semantics: &'static str,
    /// Compiler/runtime build identity. See [`COMPILER_IDENTITY`].
    pub compiler_runtime: &'static str,
    /// Requested simple perf map mode, not proof of map availability.
    pub simple_perf_requested: bool,
}

struct CompiledProgram {
    func: EvmCompilerFn,
    code_len: usize,
    hits: AtomicU64,
}

/// Owns an LLVM JIT module and every `EvmCompilerFn` produced from it.
///
/// Function pointers are never stored without this owner. [`compile`](Self::compile)
/// is the only insertion path. Do not call [`compile`](Self::compile) from
/// `frame_run`; warm-compile outside replay and then share the registry.
///
/// Not `Sync` (LLVM context). `Arc` is for sharing on one thread after warm
/// compile; this crate does not `unsafe impl Send/Sync`.
pub struct CompiledFrameRegistry {
    compiler: EvmCompiler<revmc::EvmLlvmBackend>,
    programs: HashMap<B256, CompiledProgram>,
    identity: CompiledFrameIdentity,
    gas_params: GasParams,
    last_timings: Option<CompileTimings>,
}

impl fmt::Debug for CompiledFrameRegistry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CompiledFrameRegistry")
            .field("identity", &self.identity)
            .field("programs", &self.programs.len())
            .finish_non_exhaustive()
    }
}

impl CompiledFrameRegistry {
    /// Creates a JIT compiler bound to `spec` and that spec's default gas table.
    ///
    /// Gas metering stays on. `single_error` is turned **off** so halt reasons are
    /// not collapsed to `OutOfGas`.
    pub fn new(spec: ArbSpecId) -> Result<Self, CompiledFrameError> {
        Self::new_configured(spec, false)
    }

    /// Diagnostic-only constructor requesting `/tmp/perf-<pid>.map` JIT symbols.
    ///
    /// Must run in a fresh process before any other JIT compilation: LLVM's
    /// process-global first compilation selects this setting. Plugin failures
    /// are warnings upstream, so callers must verify the actual map and symbols.
    /// This request does not establish profiling success or sample quality.
    pub fn new_with_simple_perf(spec: ArbSpecId) -> Result<Self, CompiledFrameError> {
        Self::new_configured(spec, true)
    }

    fn new_configured(spec: ArbSpecId, simple_perf: bool) -> Result<Self, CompiledFrameError> {
        let eth_spec = spec.into_eth_spec();
        let gas_params = GasParams::new_spec(eth_spec);
        let backend = EvmLlvmBackend::new(false)
            .map_err(|err| CompiledFrameError::Compiler(err.to_string()))?;
        let mut compiler =
            EvmCompiler::new_with_block_hash_semantics(backend, BlockHashSemantics::ArbosL1Ring);
        compiler.set_module_name("arb-compiled-frame");
        compiler.set_dump_to(None);
        compiler.dump_assembly(false);
        compiler.set_debug_support(false);
        compiler.set_simple_perf(simple_perf);
        compiler.gas_metering(true);
        compiler.single_error(false);
        // SAFETY: `true` is the revmc default; set explicitly so COMPILER_IDENTITY
        // (`stack-checks-on`) matches the instance rather than an implicit default.
        unsafe { compiler.stack_bound_checks(true) };
        compiler.set_gas_params(gas_params.clone());
        Ok(Self {
            compiler,
            programs: HashMap::new(),
            identity: CompiledFrameIdentity {
                spec,
                eth_spec,
                target_arch: std::env::consts::ARCH,
                target_os: std::env::consts::OS,
                compiler_runtime: COMPILER_IDENTITY,
                simple_perf_requested: simple_perf,
                block_hash_semantics: BlockHashSemantics::ArbosL1Ring.cache_tag(),
            },
            gas_params,
            last_timings: None,
        })
    }

    /// Bound identity (spec, target, compiler, runtime, gas-check flags).
    pub fn identity(&self) -> &CompiledFrameIdentity {
        &self.identity
    }

    /// Gas table compiled into the programs.
    pub fn gas_params(&self) -> &GasParams {
        &self.gas_params
    }

    /// True when a live frame's spec and gas table match this instance.
    pub fn accepts_context(&self, spec: ArbSpecId, gas_params: &GasParams) -> bool {
        self.has_arbos_ring_semantics()
            && spec == self.identity.spec
            && gas_params == &self.gas_params
    }

    fn has_arbos_ring_semantics(&self) -> bool {
        self.compiler.block_hash_semantics() == BlockHashSemantics::ArbosL1Ring
            && self.identity.block_hash_semantics == BlockHashSemantics::ArbosL1Ring.cache_tag()
    }

    /// Whether `code_hash` has a compiled function in this instance.
    pub fn contains(&self, code_hash: B256) -> bool {
        self.programs.contains_key(&code_hash)
    }

    /// Last `jit` phase timings, if any compile has run. Cold compile cost lives here;
    /// `frame_run` lookup does not compile.
    pub fn last_compile_timings(&self) -> Option<CompileTimings> {
        self.last_timings
    }

    /// Number of resident compiled programs.
    pub fn len(&self) -> usize {
        self.programs.len()
    }

    /// True when no programs are resident. Dispatch then takes the interpreter path.
    pub fn is_empty(&self) -> bool {
        self.programs.is_empty()
    }

    /// Warm-compiles `bytecode` with the owned compiler.
    ///
    /// Returns the keccak-256 code hash used as the lookup key. Already-compiled
    /// hashes are no-ops. Never call this from `frame_run`.
    pub fn compile(&mut self, bytecode: &[u8]) -> Result<B256, CompiledFrameError> {
        if let Some(reason) =
            bytecode_ineligible_with_ring(bytecode, self.has_arbos_ring_semantics())
        {
            return Err(CompiledFrameError::Ineligible(reason));
        }
        let code_hash = keccak256(bytecode);
        if self.programs.contains_key(&code_hash) {
            return Ok(code_hash);
        }

        let name = format!("arb_{code_hash}");
        // SAFETY: the returned function is stored only in `self.programs`, and
        // `self.compiler` is never `clear()`ed or dropped while those entries exist.
        // `clear_ir` (ORC) drops IR and keeps committed machine code resident.
        let jit = unsafe { self.compiler.jit(&name, bytecode, self.identity.eth_spec) };
        self.last_timings = Some(self.compiler.take_timings());
        // Always drop leftover IR so a failed jit cannot poison the next compile.
        let clear = self.compiler.clear_ir();
        let func = jit.map_err(|err| CompiledFrameError::Compiler(err.to_string()))?;
        clear.map_err(|err| CompiledFrameError::Compiler(err.to_string()))?;
        self.programs.insert(
            code_hash,
            CompiledProgram {
                func,
                code_len: bytecode.len(),
                hits: AtomicU64::new(0),
            },
        );
        Ok(code_hash)
    }

    /// Shares the registry for attachment to an [`crate::evm::ArbEvm`].
    pub fn into_shared(self) -> std::sync::Arc<Self> {
        std::sync::Arc::new(self)
    }

    /// How many times compiled functions were entered from `frame_run` (all hashes).
    pub fn dispatch_hits(&self) -> u64 {
        self.programs
            .values()
            .map(|p| p.hits.load(Ordering::Relaxed))
            .sum()
    }

    /// Hits for one code hash. Distinguishes caller vs callee in nested CALL.
    pub fn dispatch_hits_for(&self, code_hash: B256) -> u64 {
        self.programs
            .get(&code_hash)
            .map(|p| p.hits.load(Ordering::Relaxed))
            .unwrap_or(0)
    }

    /// Looks up a compiled function. Pointers are valid only while `self` is alive.
    pub(crate) fn lookup(&self, code_hash: B256, live_len: usize) -> Option<EvmCompilerFn> {
        if !self.has_arbos_ring_semantics() {
            return None;
        }
        let program = self.programs.get(&code_hash)?;
        if program.code_len != live_len {
            return None;
        }
        program.hits.fetch_add(1, Ordering::Relaxed);
        Some(program.func)
    }
}

#[cfg(test)]
mod profile_configuration_tests {
    use super::*;

    #[test]
    fn simple_perf_is_explicit_and_bound_before_compilation() {
        let ordinary = CompiledFrameRegistry::new(ArbSpecId::NITRO).unwrap();
        let diagnostic = CompiledFrameRegistry::new_with_simple_perf(ArbSpecId::NITRO).unwrap();
        let ordinary_after = CompiledFrameRegistry::new(ArbSpecId::NITRO).unwrap();
        for registry in [&ordinary, &ordinary_after] {
            assert!(!registry.compiler.simple_perf());
            assert!(!registry.identity().simple_perf_requested);
            assert!(registry.is_empty());
            assert!(registry.has_arbos_ring_semantics());
        }
        assert!(diagnostic.compiler.simple_perf());
        assert!(diagnostic.identity().simple_perf_requested);
        assert!(diagnostic.is_empty());
        assert!(diagnostic.has_arbos_ring_semantics());
        // No JIT here: this proves request plumbing, not process-global map setup.
    }
}
