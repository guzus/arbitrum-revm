# Prepared poster compression primitive

This is an opt-in primitive only. No handler caller, node scheduler, worker thread,
mutable/global cache, runtime flag or deployment is wired by this change. No speedup
is claimed. Existing `compute_poster_info` uses the same synchronous compression
and fee behavior, without allocating or copying preparation inputs.

`PreparedPosterCompression::prepare(bytes, level, window, dictionary)` computes a
successful compressed length and retains an owned copy of the exact encoded bytes
and all requested settings. Fields are private; no state/fee values or unsuccessful
fallback results are retained. The artifact has no interior mutation and may be
transferred between workers using ordinary Rust ownership.

Use `encode_tx_bytes` to preserve the current canonical EIP-2718-versus-fallback
selection, including empty output for internal/deposit/submit-retryable/retry
transactions. Workers should skip empty inputs. At consumption, first read the
actual compression level and all current fee/state inputs in serial transaction
order, then call `compute_poster_info_with_prepared(..., prepared.as_ref())`.
An artifact is accepted only for exact bytes/level plus the existing default
window and empty dictionary. A level change, different bytes, other settings,
missing result or failed preparation retains the synchronous path. Empty inputs
and non-batch-poster blocks still return zero; zero prices still retain calldata
units, and gas-price-zero behavior remains unchanged.

Future scheduling must bound workers, queued bytes and artifact retention. Do not
wait for unrelated work or reuse a previous execution's state. Preparation errors
should leave no artifact so the current synchronous compression/error policy runs.
No compiler/algorithm version cache invalidation is needed here because artifacts
are process-local typed values, not serialized cache entries.

A future benchmark must include scheduling, preparation and join costs from block
availability through execution completion. Clear prepared inputs per replay
attempt; warming a repeated fixture's compression outside the timer is not a live
block acceleration result. This primitive alone neither moves work outside the
execution timer nor changes production behavior.

Validation: eight focused `l1_cost::tests` passed on the cached local test target,
covering settings/bytes mismatches, current fee recomputation, error/nonposter/empty
cases, canonical/fallback/system encoding and zero-price units. Full block parity
and scheduler concurrency validation remain required when a caller is integrated.

```sh
cargo test --locked -p arb-revm --lib l1_cost::tests -- --test-threads=1
```
