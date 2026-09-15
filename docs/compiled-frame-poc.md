# Compiled-frame proof of concept

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
interpreter baseline.

## Eligibility / coverage

Compiled only when **all** of these hold:

- runtime call frame (not `CREATE` / initcode, not empty frame)
- bytecode does not start with `0xEF` (EOF / Stylus)
- bytecode does not contain `NUMBER` or `BLOCKHASH` after skipping PUSH
  immediates (ArbOS L1 overrides differ from revmc builtins)
- code hash is already in the registry
- live `ArbSpecId` and `GasParams` match the instance that compiled the program

Cache key is code hash plus the registry's immutable instance context (ArbOS
spec, eth spec, target arch/os, compiler/runtime identity, gas table).

Inspect/tracing: `InspectorEvmTr::inspect_frame_run` runs `inspect_instructions`
on the interpreter table and does **not** call `frame_run`. Compiled dispatch
is skipped under `inspect_*`; traces always show the interpreter.

`ArbContext` does not implement `Host`. Stock `Context` `Host` methods are L2
(`block_number`, `block_hash`). NUMBER/BLOCKHASH are **instruction-table**
overrides, which compiled code never sees. revmc builtins go through `Host`,
so those two opcodes are denylisted. Other env opcodes (COINBASE, DIFFICULTY,
GASPRICE, …) share `Host` with the interpreter and are not overridden.

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
   Stylus, NUMBER/BLOCKHASH on non-compiled frames).
3. Coverage high enough that interpreter fallback + handler/precompile/Stylus
   time do not dominate. Frames with `NUMBER`/`BLOCKHASH`, initcode, Stylus, and
   unknown hashes still interpret.
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
- revmc `NUMBER`/`BLOCKHASH` builtins are L2; we refuse those opcodes rather
  than teaching revmc ArbOS L1 semantics.
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
  mismatch, PUSH-immediate exclusion, and the 256-opcode instruction-table
  diff. They are not a replay corpus. No whole-block speed claim.
