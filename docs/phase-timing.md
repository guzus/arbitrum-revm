# Core transaction phase timing

Offline diagnostic feature `phase-timing`, off by default. This does not implement
parallel replay or change legacy histogram sampling. It does not measure node
receipt/state commit, retry derivation/scheduling, sender recovery, witness setup
or root verification.

## Entrypoint

```rust,ignore
let (execution, report) = arb_revm::phase_timing::with_recording(4096, || {
    evm.transact(tx)
})?;
// Inspect report.complete and execution; serialize only after replay/timing ends.
```

`with_recording` preallocates a bounded record vector before invoking the closure.
Capacities must be 1 through 65,536. Nested recording scopes return an error without
invoking their closure. Recording is thread-local; new threads do not inherit it.
Every supported attempt increments `attempted`; capacity overflow increments
`dropped`, preserves execution, and marks `complete=false`. Consumers must reject
incomplete reports. An empty report does not prove the requested execution path
was measured: require expected attempt counts and outcomes in the node harness.

Supported entrypoints are `ExecuteEvm::transact` and `replay` on the existing
EthInstructions ArbEvm implementation. Direct `transact_one`, inspector/system-call
APIs and other instruction providers do not independently create records. The
scope alone does not redirect those APIs or bypass inspector semantics. A future
node harness must explicitly identify unsupported paths rather than label missing
records as zero-duration work.

An attempt contains a monotonic local ID, optional nested-core parent ID (not retry
provenance), entrypoint, transaction type/nonce, outcome, terminal/error phase and
total duration. No hash is computed for diagnostics. The outer node harness must
supply block attempt, input and included indices and scheduler provenance.

`phases` is indexed by the accompanying `phase_order`. `None` means a phase was
not entered; present values carry call count and integer nanoseconds. Phases are
inclusive: `Vm` is inside `Execution`. Do not sum both. Protocol shortcuts may have
Execution but no Vm. Mainnet execution includes initial-frame setup, frame loop,
precompiles/Stylus and final-frame gas handling, not only opcodes.

The phases are validation/precharge, transaction-gas setup, pre-execution,
execution envelope, VM subregion, runtime-OOG result construction, full Arb
settlement, result/journal close, error cleanup and journal finalization.
Outcome distinguishes success, revert, halt, invalid transaction, header error,
database error, custom error and interruption. Error phase identifies the failing top-level orchestration boundary (an Execution
error may originate inside Vm), and is preserved across cleanup/finalization.

## Preserved semantics and drift check

Inactive scopes continue through the existing `h.run` path. Feature-off builds do
not compile the recorder or its hooks. Active attempts call a diagnostic helper
that mirrors revm-handler 42.0.1 `Handler::run` and `run_without_catch_error`,
invoking the same Arb handler methods in the same order, including all `?` and
runtime-OOG branches. It never calls mainnet validation or settlement instead of
Arb overrides. VM guards surround only the two existing mainnet execution calls.

The `transact` override follows upstream `ExecuteEvm::transact`: call
`transact_one`, always finalize, then propagate the result/error. The attempt spans
all of these operations. `replay` deliberately retains its existing success-only
finalization behavior. Do not "fix" these different API contracts in a timing
change.

Before upgrading the pinned handler, run this against the resolved crate directory:

```sh
python3 scripts/check_phase_timing_upstream.py /path/to/revm-handler-42.0.1
```

Fingerprints in `phase-timing-upstream.json` cover the three mirrored upstream
methods. A failure requires reviewing the helper and API override, not blindly
refreshing fingerprints. The drift check is source evidence; differential tests
remain necessary.

## Verification and measurement controls

Relevant test target:

```sh
cargo test -p arb-revm --lib --features phase-timing phase_timing::tests
```

Run the ordinary feature-off checks and the node's compiled/Stylus combination
checks separately. Tests compare complete results/state for success, revert,
halt, invalid nonce and nested CALL, and cover database errors, finalization after
errors, bounded overflow, nested-scope rejection and unwind cleanup. These tests
are not benchmark results or node scheduler coverage.

Compare feature-off, feature-on/inactive and feature-on/recording with matched
execution features, fixtures and legacy metric settings. Serialize outside replay
and report observer/serialization cost. The existing validation histogram selects
its sampling flag before pre_execution changes it; metrics sample rate zero is
not a complete observer-off control. This diagnostic intentionally leaves that
separate issue unchanged.

Canonical accepted-block fixtures omit invalid dropped inputs and dynamic retry
construction. A complete serial-overhead study needs original input-message replay
with node-level attempt/commit/scheduler instrumentation. Timing alone does not
establish dependency independence. For a partition leaving fraction s serial, the
optimistic eight-worker bound is `1 / (s + (1-s)/8)`; greater than 5x needs s below
3/35 (about 8.6%), before parallel overhead.

## Combined-feature instruction guard regression

The first combined `phase-timing,compiled-frame,stylus` Linux library run at
`86f6ec6` had 159 passes and one failure in
`evm::instruction_table_diff::arb_overrides_are_exactly_number_and_blockhash`.
That test compared function addresses from independently initialized tables;
codegen-unit duplication can give the same function different addresses.
This failure is retained as failed qualification, not treated as a parity pass.

The guard now snapshots one table before and after the canonical override helper.
It checks every instruction address and gas entry, requires exactly NUMBER and
BLOCKHASH mutations, and checks their costs. A reviewed full-constructor source
snapshot rejects bypassing that helper. Updating the snapshot requires explicit
review of all table mutations and existing compiled/interpreter opcode parity.
No compiler eligibility rule or opcode behavior changed. Rerun the combined suite
and feature-off checks before claiming this regression resolved.
