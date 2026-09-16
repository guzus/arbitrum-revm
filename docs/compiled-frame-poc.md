# Compiled-frame proof of concept

## Current BLOCKHASH experiment (2026-09-16)

The `compiled-frame` feature remains off by default. The owned registry now pins
both `revmc` and `revmc-build` to `guzus/revmc` commit
`6f8854dc7e5265f238ada30e16ea8629bafd95d7`. Its only constructor selects immutable
`BlockHashSemantics::ArbosL1Ring`; public eligibility therefore admits BLOCKHASH.
The ordinary Ethereum compiler default is unchanged.

The compiled host reads `ArbosState::block_hashes` through the live journal,
matching the interpreter's storage warmth, range checks and error-to-zero policy.
The builtin saturates the U256 request to u64 before this lookup. NUMBER still
returns the supplied L1 number; neither method rewrites the L2 block environment.
The forwarding-only host constructor remains private and is never used by dispatch.

Registry identity includes `arbos-l1-ring-blockhash-v1` and the compiler pin;
context acceptance and function lookup require that semantic binding. Registry
entries cannot be imported from an external cache or another compiler owner.
Any future external object cache must include this tag and the complete build,
specification, target and gas configuration identity.

Added qualification tests cover generated Ethereum/ArbOS symbol coexistence in one
process, cursor distinct from NUMBER, current/future/window boundary/MAX/missing
hashes, repeated warm reads, journal read failure, nested calls and resumed parent
frames. Transaction comparisons include the entire execution result and state.
These tests still require Linux/LLVM execution; source formatting is not parity
qualification and no speedup or deployment is claimed.

```sh
cargo test -p arb-revm --features compiled-frame --lib compiled_frame
cargo test -p arb-revm --features compiled-frame --test compiled_frame
```

## Opt-in JIT symbol diagnostic

`CompiledFrameRegistry::new(spec)` still requests simple perf maps **off**.
A standalone diagnostic may use
`CompiledFrameRegistry::new_with_simple_perf(spec)` before its first compile.
There are no environment-variable switches or runtime setters. Both constructors
retain the same immutable ArbOS BLOCKHASH semantics and normal execution controls.
`identity().simple_perf_requested` records the request, not successful activation.

LLVM initializes its shared ORC state on the process's first JIT compilation;
that first compiler's setting wins for all subsequent compilers. Start a fresh
process with the diagnostic constructor before any other JIT. Upstream plugin
setup failure is logged as a warning and does not make construction fail. Verify
`/tmp/perf-<target-pid>.map` exists and contains the expected `arb_<code_hash>`
entries, and preserve that map with the perf artifact before the process exits.
The map does not remove freed entries: keep this a short standalone diagnostic,
not a long-lived production profiling service. Do not use profiled runs for
performance comparisons; map existence alone does not prove useful samples.

The focused `profile_configuration_tests` test checks explicit request wiring
without JIT; actual map generation and symbol attribution require Linux evidence.

## Historical NUMBER-only prototype notes

The remainder records the earlier 79e3c8ca prototype, including its historical
BLOCKHASH refusal. The current semantic binding above supersedes that refusal.


Isolated, **default-off** research path. It does not replace `ArbHandler`, ArbOS
precompiles, poster fees, retryables, or frame init/return/span accounting. It
does not wrap the EVM in upstream `JitEvm` (those entrypoints use
`MainnetHandler`).

Parent process owns whole-replay benchmarks and delivery. This document is the
worker-side contract for the experimental boundary.

## How to enable

Default `cargo check` / `cargo test` must not compile LLVM and must keep the
original interpreter path.

```bash
export CARGO_TARGET_DIR=.local/target
export CARGO_BUILD_JOBS=2
export PATH="/opt/homebrew/opt/llvm@22/bin:$PATH"
export LLVM_SYS_221_PREFIX=/opt/homebrew/opt/llvm@22

# Default graph (feature off)
cargo check -p arb-revm -j2

# PoC graph
cargo test -p arb-revm --features compiled-frame --test compiled_frame \
  -- --test-threads=1
```

Local revmc is `../revmc-arb-prototype` at `79e3c8ca1980d98c856034558ec7c7a9f3f3dd7d`.
LLVM is 22.1.8 at `/opt/homebrew/opt/llvm@22`. Use this worktree's `.local/target`
only; do not reuse a native replay probe target from another checkout.

## Dispatch

`ArbEvm::frame_run`:

1. Stylus programs (feature `stylus`) run as WASM.
2. If a `CompiledFrameRegistry` is attached, is non-empty, matches the live
   ArbOS spec + gas table, and has a function for this code hash, call
   `EvmCompilerFn::call_with_interpreter` and feed the `InterpreterAction` to
   `frame.process_next_action` / `set_finished` (same as the Stylus path).
3. Otherwise `self.0.frame_run()` (interpreter).

The handler loop calls `frame_run` for every frame, so nested CALLs are checked.
`frame_run` never compiles.

## Warm vs cold compile cost

| Phase | When | What you pay |
| --- | --- | --- |
| Cold compile | `CompiledFrameRegistry::compile` before replay | LLVM parse + translate + optimize + codegen. `last_compile_timings()` records that. |
| Warm lookup | `frame_run` during replay | Hash lookup + `call_with_interpreter`. No LLVM. |
| Interpreter fallback | empty/missing registry, miss, ineligible frame | Unchanged revm interpreter. |

A whole-replay speedup claim **must not** count cold compile. Warm the eligible
set first, then measure ArbOS message/block replay wall time against the
interpreter baseline. Enabling NUMBER increases the eligible set (parent
witness notes on the order of 10 NUMBER-refused images); whether that moves
whole-replay time is a parent measurement, not a result of this adapter.

## Eligibility / coverage

Compiled only when **all** of these hold:

- runtime call frame (not `CREATE` / initcode, not empty frame)
- bytecode does not start with `0xEF` (EOF / Stylus)
- bytecode does not contain a refused instruction-table override after
  skipping PUSH immediates. `BLOCKHASH` is refused. `NUMBER` is allowed:
  a compiled-only Host adapter makes revmc's `Host::block_number()` return
  `chain().l1_block_number` without writing `BlockEnv`. Any new
  `insert_instruction` must be classified as refused or host-bridged;
  unclassified overrides fail closed (ineligible).
- code hash is already in the registry
- live `ArbSpecId` and `GasParams` match the instance that compiled the program

Cache key is code hash plus the registry's immutable instance context (ArbOS
spec, eth spec, target arch/os, compiler/runtime identity, gas table).

Inspect/tracing: `InspectorEvmTr::inspect_frame_run` runs `inspect_instructions`
on the interpreter table and does **not** call `frame_run`. Compiled dispatch
is skipped under `inspect_*`; traces always show the interpreter.

`ArbContext` does not implement `Host`. Stock `Context` `Host` methods are L2
(`block_number`, `block_hash`). NUMBER and BLOCKHASH are **instruction-table**
overrides, which compiled code never sees. revmc builtins go through `Host`.

NUMBER is served by a stack-local compiled Host adapter used only inside
`call_with_interpreter`. It overrides `block_number()` to the L1 value copied
from `chain().l1_block_number` and forwards every other Host method to the
original context, including trait-default methods (`sstore`, `sload`,
`balance`, `load_account_delegated`, `load_account_code`,
`load_account_code_hash`) so an inner override is not replaced by the default.
The real `BlockEnv` is not written.

BLOCKHASH stays ineligible. `__revmc_builtin_blockhash` (pinned revmc
`79e3c8ca`) does `host.block_number().checked_sub(requested)`, accepts only
diff in `(0, 256]`, then `host.block_hash` (L2 header DB). ArbOS
`arb_block_hash` reads the L1 ring (`>= current || current > number+256` →
zero, unmetered). Bridging NUMBER must not be treated as bridging BLOCKHASH.

Other env opcodes (COINBASE, DIFFICULTY, GASPRICE, …) share `Host` with the
interpreter and are not overridden.

revmc `79e3c8ca` ends a gas section at `GAS`, Istanbul+ `SSTORE`, branches, and
CALL/CREATE (after charging that opcode's own base gas). Trailing static costs
are a new section, so they should not be precharged into `gasleft`. Differential
tests cover GAS-then-MSTORE, SSTORE EIP-2200 sentry with trailing PUSH/POP, and
CALL stipend = `GAS`.

## Whole-replay criterion

The research goal is **>5× wall-time on whole ArbOS replay**, not a frame-only
microbench.

That requires:

1. Warm compile of the eligible contracts **outside** replay.
2. A replay that still runs `ArbHandler` (poster fees, retryables, precompiles,
   Stylus, BLOCKHASH and other non-compiled frames).
3. Coverage high enough that interpreter fallback + handler/precompile/Stylus
   time do not dominate. Frames with `BLOCKHASH`, initcode, Stylus, and
   unknown hashes still interpret. `NUMBER`-only images may compile when the
   adapter is in use. This checkout does not measure that coverage shift.
4. Compare full message/block replay time, not `call_with_interpreter` alone.

This checkout does not run that replay. Parent owns the probe.

## Ownership

`CompiledFrameRegistry` owns the LLVM module. `compile` is the only insertion
path. There is no API that accepts an arbitrary function pointer. Dropping the
registry invalidates the pointers; `ArbEvm` holds `Arc<CompiledFrameRegistry>`
so the module outlives dispatch.

## Limits that remain

- Inspect/trace path always interprets (`inspect_frame_run` does not use
  compiled dispatch). Compiled-path bugs will not show up in `debug_trace`.
- revmc `BLOCKHASH` stays refused (extra range check + L2 `Host::block_hash`
  vs ArbOS L1 ring). `NUMBER` is bridged only through the compiled Host
  adapter; the interpreter table override is unchanged.
- Compiled dispatch always installs the NUMBER adapter, including on frames
  that never execute NUMBER. `call_with_interpreter` already takes
  `&mut dyn Host`; the adapter adds a forwarding layer and a `u64` copy of
  `l1_block_number` at frame entry. The generic inner Host may be inlined;
  an extra vtable hop is not established. Any remaining forwarding overhead
  is on the warm path and must be measured, not assumed away.
  Parent owns whole-replay timing of `5c00135` vs this change.
- `Host` is required on `EvmTr`/`ExecuteEvm` **only** with `--features compiled-frame`
  (`CompiledFrameCtx`). Default-off bounds are unchanged.
- Enabling `compiled-frame` in a workspace adds a private registry field to
  `ArbEvm`. External `ArbEvm(inner)` tuple construction then fails to compile;
  use `ArbEvm::from_inner`.
- `CompiledFrameRegistry` is not documented as `Send`/`Sync`. revmc's LLVM
  backend has upstream `unsafe impl Send`; this crate adds none. `Arc` is for
  single-thread sharing after warm compile.
- `clear_ir` at this revmc revision replaces the LLVM IR module and does not
  drop ORC committed machine code. Not a lifetime proof under `clear()`.
- No debug assertion that a resumed frame stays on the compiled path if `cfg`
  were to change mid-transaction (handler is not known to do that).
- Default opt-level / debug-assertion compiler settings are whatever revmc
  LLVM uses; `stack_bound_checks(true)`, `gas_metering(true)`, `single_error(false)`
  are set explicitly.
- revmc is pinned to public revision `79e3c8ca`. LLVM 22 remains a build
  requirement only when the feature is on.
- Parity tests cover arithmetic, exact halt reasons, REVERT leftover gas,
  SSTORE, SSTORE sentry, GAS-then-static-ops, nested CALL (per-hash hits),
  CALL stipend = GAS, reverted-child resume, initcode skip, miss, spec
  mismatch, PUSH-immediate exclusion, the 256-opcode instruction-table
  diff, compiled NUMBER vs L1 (not L2) with matching gas/state, nested
  compiled NUMBER, NUMBER after compiled resume, original `BlockEnv`
  unchanged, and BLOCKHASH still refused. They are not a replay corpus.
  No whole-block speed claim.


### NUMBER review reconciliation

Independent Claude review found no blocking adapter defect. Parent matched all
30 pinned Host method names against the wrapper (including default methods),
confirmed only NUMBER/BLOCKHASH builtins read Host.block_number, and inspected
call_with_interpreter_inner: its EvmContext is local; only gas/action/resume
state is stored back, with no retained host pointer. NUMBER's interpreter reads
chain.l1_block_number without a version branch and costs2 gas. The scope remains
constructor-produced, unmodified instruction tables; public post-construction
customization is unsupported by this compiled prototype. The table audit is a
test gate, not runtime inspection. Documentation now states that accurately.

The eligibility scanner now skips only legacy PUSH payloads, avoiding dependence
on future or EOF multi-byte immediate metadata. A regression test ensures such
prefixes cannot hide BLOCKHASH. STATICCALL/DELEGATECALL and upgrade-block corpus
coverage remain incomplete; these tests do not authorize production enablement.
