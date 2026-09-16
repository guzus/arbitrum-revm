# Canonical stack direct dispatch experiment

Feature `direct-stack-dispatch` is off by default. It specializes only POP,
PUSH1/PUSH2, DUP1–DUP3 and SWAP1/SWAP2. All arithmetic/stack behavior is delegated
to the pinned upstream opcode functions. PC advancement, static-gas charging,
error handling and action extraction follow upstream Interpreter::step/run_plain.
Other opcodes, including NUMBER/BLOCKHASH, use the canonical Arb instruction table.

```rust,ignore
let evm = CanonicalArbEvm::new_canonical(ctx, inspector, StackDispatchMode::Indirect);
let evm = CanonicalArbEvm::new_canonical(ctx, inspector, StackDispatchMode::Direct);
let stats = evm.0.instruction.stats(); // direct_frames and direct_steps
```

Both modes use the same immutable CanonicalStackInstructions provider. It exposes
no mutable instruction table, arbitrary-table constructor or gas setter. Existing
mutable EthInstructions and ArbEvm::from_inner always retain indirect dispatch.
The private frame runner is installed only by new_canonical and retained when
replacing inspectors/precompiles. Normal inspector execution uses its original
loop; Stylus and compiled dispatch still take precedence. No node/runtime switch
or deployment is included here.

Stats use provider-local integers, no atomics or TLS. Hot-op calls are counted
locally and aggregated once per interpreter frame invocation (including resumed
parents). Static-gas failures before dispatch do not count as direct_steps;
stack errors inside a directly called opcode do. Reading stats does not reset
them, but cloning a provider resets its stats to avoid inherited evidence. All
counter overhead belongs inside the execution measurement. A speedup has not
been demonstrated; require same-binary parity and full end-to-end paired data.

Checks:
```sh
cargo test --locked -p arb-revm --features direct-stack-dispatch --lib stack_dispatch::tests
cargo check --locked -p arb-revm --lib --tests
```

Differential tests cover truncated PUSH, exact/insufficient static gas, stack
bounds, mixed direct/indirect code, nested call and parent resume, full EVM
result/state parity, custom opcode override retention, and inspector/from_inner
bypass. Full witness qualification and Stylus/compiled feature builds remain
separate required integration checks.

## Counter ablation

`StackDispatchMode::DirectUncounted` runs the same guarded eight-opcode kernel as
`Direct`, instantiated with a compile-time `COUNT = false`. Only per-opcode hit
increments are removed; PC, gas, opcode calls, errors and frame-level aggregation
retain the same path. `direct_frames` must remain positive for an executed kernel.
`direct_steps == 0` means unavailable in this mode, not unused; consumers must
report it as null alongside the explicit mode. The kernel still returns a zero
hit value, preserving the shared implementation and result structure.

The counter-ablation comparison is limited to same-binary indirect/counted/uncounted
runs with full parity. It does not relax the deployment threshold or make counts
into timings. Differential edge cases and nested full-result/state tests compare
both direct variants against the same indirect reference.
