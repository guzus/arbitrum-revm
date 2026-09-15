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

Out of coverage for this PoC: inspect/tracing (`inspect_frame_run` is not
overridden), sync compile on miss, AOT artifacts, substituting `JitEvm`,
disabling gas metering, collapsing halt reasons (`single_error` is off).

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

- Inspect/trace path still interprets.
- revmc `NUMBER`/`BLOCKHASH` builtins are L2; we refuse those opcodes rather
  than teaching revmc ArbOS L1 semantics.
- Default opt-level / debug-assertion compiler settings are whatever revmc
  LLVM uses; they affect compile time, not the “did we disable gas” question
  (gas metering stays on).
- Optional path dependency on the sibling revmc checkout: `cargo` resolves the
  path even with the feature off; LLVM is not compiled unless the feature is on.
- Parity tests cover arithmetic, OOG, SSTORE, nested CALL, initcode skip, miss,
  spec mismatch, and PUSH-immediate exclusion. They are not a replay corpus.
