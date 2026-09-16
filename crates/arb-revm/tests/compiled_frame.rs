//! Compiled vs interpreted parity for the default-off compiled-frame PoC.
//!
//! These tests need LLVM 22 (`/opt/homebrew/opt/llvm@22`) and
//! `--features compiled-frame`. Compile is warm (outside `transact`); `frame_run`
//! only looks up resident programs.

#![cfg(feature = "compiled-frame")]
#![allow(clippy::field_reassign_with_default)]

use arb_revm::transaction::arb_envelope_to_tx_env;
use arb_revm::{
    ARB_INSTRUCTION_OVERRIDES, ArbBuilder, ArbChainContext, ArbContext, ArbSpecId, ArbTransaction,
    COMPILED_HOST_BRIDGED_OVERRIDES, CompiledFrameRegistry, DefaultArb, bytecode_ineligible,
};
use arbitrum_alloy_consensus::transactions::{ArbTxEnvelope, TxUnsigned};
use revm::{
    ExecuteEvm,
    context::{BlockEnv, CfgEnv, TxEnv, result::ExecutionResult},
    database::{CacheDB, EmptyDB},
    interpreter::Host,
    primitives::{Address, Bytes, TxKind, U256, keccak256},
    state::{AccountInfo, Bytecode, EvmState},
};
use std::sync::Arc;

const CHAIN_ID: u64 = 412_346;
const CALLER: Address = Address::new([0x11; 20]);
const CONTRACT: Address = Address::new([0xcc; 20]);
const CALLEE: Address = Address::new([0xdd; 20]);

/// PUSH1 2 PUSH1 3 ADD PUSH1 0 MSTORE PUSH1 32 PUSH1 0 RETURN
fn arithmetic_code() -> Bytes {
    Bytes::from(vec![
        0x60, 0x02, 0x60, 0x03, 0x01, 0x60, 0x00, 0x52, 0x60, 0x20, 0x60, 0x00, 0xf3,
    ])
}

/// Infinite JUMPDEST/PUSH1 0/JUMP loop.
fn gas_burn_code() -> Bytes {
    Bytes::from(vec![0x5b, 0x60, 0x00, 0x56])
}

/// PUSH1 0x2a PUSH1 0 SSTORE STOP
fn sstore_code() -> Bytes {
    Bytes::from(vec![0x60, 0x2a, 0x60, 0x00, 0x55, 0x00])
}

/// Callee returns 0x2a as a 32-byte word.
fn return_2a_code() -> Bytes {
    Bytes::from(vec![
        0x60, 0x2a, 0x60, 0x00, 0x52, 0x60, 0x20, 0x60, 0x00, 0xf3,
    ])
}

fn call_callee_code(callee: Address) -> Bytes {
    let mut code = vec![
        0x60, 0x20, // retSize
        0x60, 0x00, // retOffset
        0x60, 0x00, // argSize
        0x60, 0x00, // argOffset
        0x60, 0x00, // value
        0x73, // PUSH20
    ];
    code.extend_from_slice(callee.as_slice());
    code.extend_from_slice(&[
        0x61, 0x40, 0x00, // PUSH2 gas
        0xf1, // CALL
        0x60, 0x20, 0x60, 0x00, 0xf3, // RETURN mem[0..32]
    ]);
    Bytes::from(code)
}

fn insert_code(db: &mut CacheDB<EmptyDB>, address: Address, code: Bytes) {
    let code_hash = keccak256(&code);
    db.insert_account_info(
        address,
        AccountInfo {
            balance: U256::ZERO,
            nonce: 1,
            code_hash,
            code: Some(Bytecode::new_raw(code)),
            ..AccountInfo::default()
        },
    );
}

fn unsigned_call(to: Address, gas_limit: u64) -> ArbTransaction<TxEnv> {
    let tx = TxUnsigned {
        chain_id: U256::from(CHAIN_ID),
        from: CALLER,
        nonce: 0,
        gas_fee_cap: U256::ZERO,
        gas_limit,
        to: TxKind::Call(to),
        value: U256::ZERO,
        input: Bytes::new(),
    };
    arb_envelope_to_tx_env(&ArbTxEnvelope::from(tx)).expect("convert tx")
}

fn unsigned_create(initcode: Bytes, gas_limit: u64) -> ArbTransaction<TxEnv> {
    let tx = TxUnsigned {
        chain_id: U256::from(CHAIN_ID),
        from: CALLER,
        nonce: 0,
        gas_fee_cap: U256::ZERO,
        gas_limit,
        to: TxKind::Create,
        value: U256::ZERO,
        input: initcode,
    };
    arb_envelope_to_tx_env(&ArbTxEnvelope::from(tx)).expect("convert tx")
}

fn cfg() -> CfgEnv<ArbSpecId> {
    let mut cfg = CfgEnv::new_with_spec(ArbSpecId::NITRO)
        .with_chain_id(CHAIN_ID)
        .with_disable_priority_fee_check(true);
    cfg.disable_balance_check = true;
    cfg
}

fn word32(last: u8) -> Bytes {
    let mut word = vec![0_u8; 32];
    word[31] = last;
    Bytes::from(word)
}

fn word_u256(value: u64) -> Bytes {
    Bytes::from(U256::from(value).to_be_bytes::<32>().to_vec())
}

/// NUMBER PUSH1 0 SSTORE STOP
fn number_sstore_code() -> Bytes {
    Bytes::from(vec![0x43, 0x60, 0x00, 0x55, 0x00])
}

/// NUMBER PUSH1 0 MSTORE PUSH1 32 PUSH1 0 RETURN
fn number_return_code() -> Bytes {
    Bytes::from(vec![0x43, 0x60, 0x00, 0x52, 0x60, 0x20, 0x60, 0x00, 0xf3])
}

fn slot0(state: &EvmState, address: Address) -> U256 {
    state
        .get(&address)
        .and_then(|acct| acct.storage.get(&U256::ZERO))
        .map(|slot| slot.present_value())
        .unwrap_or_default()
}

fn transact_call(
    db: &mut CacheDB<EmptyDB>,
    to: Address,
    gas_limit: u64,
    registry: Option<Arc<CompiledFrameRegistry>>,
) -> ExecutionResult {
    transact(db, unsigned_call(to, gas_limit), registry).result
}

fn transact(
    db: &mut CacheDB<EmptyDB>,
    tx: ArbTransaction<TxEnv>,
    registry: Option<Arc<CompiledFrameRegistry>>,
) -> revm::context::result::ExecResultAndState<ExecutionResult, EvmState> {
    let chain = ArbChainContext::new(None).with_l1_block_number(777);
    let mut block = BlockEnv::default();
    block.number = U256::from(1000);
    let ctx: ArbContext<&mut _> = ArbContext::arb_with_chain_context(chain)
        .with_db(db)
        .with_cfg(cfg())
        .with_block(block)
        .with_tx(ArbTransaction::<TxEnv>::default());
    let mut evm = ctx.build_arb();
    evm.set_compiled_programs(registry);
    evm.transact(tx).expect("execution")
}

fn warm_compile(spec: ArbSpecId, codes: &[&[u8]]) -> Arc<CompiledFrameRegistry> {
    let mut registry = CompiledFrameRegistry::new(spec).expect("llvm compiler");
    for code in codes {
        registry.compile(code).expect("warm compile");
    }
    registry.into_shared()
}

fn assert_same_outcome(interpreted: &ExecutionResult, compiled: &ExecutionResult) {
    match (interpreted, compiled) {
        (
            ExecutionResult::Success {
                reason: ra,
                gas: ga,
                logs: la,
                output: oa,
            },
            ExecutionResult::Success {
                reason: rb,
                gas: gb,
                logs: lb,
                output: ob,
            },
        ) => {
            assert_eq!(ra, rb, "success reason");
            assert_eq!(ga, gb, "success gas");
            assert_eq!(la, lb, "success logs");
            assert_eq!(oa, ob, "success output");
        }
        (
            ExecutionResult::Revert {
                gas: ga,
                logs: la,
                output: oa,
            },
            ExecutionResult::Revert {
                gas: gb,
                logs: lb,
                output: ob,
            },
        ) => {
            assert_eq!(ga, gb, "revert gas");
            assert_eq!(la, lb, "revert logs");
            assert_eq!(oa, ob, "revert output");
        }
        (
            ExecutionResult::Halt {
                reason: ra,
                gas: ga,
                logs: la,
            },
            ExecutionResult::Halt {
                reason: rb,
                gas: gb,
                logs: lb,
            },
        ) => {
            assert_eq!(ra, rb, "halt reason");
            assert_eq!(ga, gb, "halt gas");
            assert_eq!(la, lb, "halt logs");
        }
        _ => panic!("result variant mismatch\ninterpreted={interpreted:?}\ncompiled={compiled:?}"),
    }
}

#[test]
fn empty_registry_uses_interpreter() {
    let mut db = CacheDB::new(EmptyDB::default());
    insert_code(&mut db, CONTRACT, arithmetic_code());
    let empty = CompiledFrameRegistry::new(ArbSpecId::NITRO)
        .expect("llvm")
        .into_shared();
    let with_empty = transact_call(&mut db, CONTRACT, 100_000, Some(empty.clone()));
    let mut db2 = CacheDB::new(EmptyDB::default());
    insert_code(&mut db2, CONTRACT, arithmetic_code());
    let without = transact_call(&mut db2, CONTRACT, 100_000, None);
    assert!(without.is_success());
    assert_same_outcome(&without, &with_empty);
    assert_eq!(empty.dispatch_hits(), 0);
}

#[test]
fn compiled_arithmetic_matches_interpreter() {
    let code = arithmetic_code();
    let registry = warm_compile(ArbSpecId::NITRO, &[code.as_ref()]);
    assert!(registry.last_compile_timings().is_some());

    let mut db_i = CacheDB::new(EmptyDB::default());
    insert_code(&mut db_i, CONTRACT, code.clone());
    let interpreted = transact_call(&mut db_i, CONTRACT, 100_000, None);

    let mut db_c = CacheDB::new(EmptyDB::default());
    insert_code(&mut db_c, CONTRACT, code);
    let compiled = transact_call(&mut db_c, CONTRACT, 100_000, Some(registry.clone()));

    assert!(interpreted.is_success());
    assert_same_outcome(&interpreted, &compiled);
    assert_eq!(compiled.output(), Some(&word32(5)));
    assert!(registry.dispatch_hits() >= 1);
}

#[test]
fn compiled_gas_exhaustion_matches_interpreter() {
    let code = gas_burn_code();
    let registry = warm_compile(ArbSpecId::NITRO, &[code.as_ref()]);

    let mut db_i = CacheDB::new(EmptyDB::default());
    insert_code(&mut db_i, CONTRACT, code.clone());
    let interpreted = transact_call(&mut db_i, CONTRACT, 25_000, None);

    let mut db_c = CacheDB::new(EmptyDB::default());
    insert_code(&mut db_c, CONTRACT, code);
    let compiled = transact_call(&mut db_c, CONTRACT, 25_000, Some(registry.clone()));

    assert!(
        interpreted.is_halt(),
        "expected OOG halt, got {interpreted:?}"
    );
    assert_same_outcome(&interpreted, &compiled);
    assert!(registry.dispatch_hits() >= 1);
}

#[test]
fn compiled_sstore_matches_interpreter() {
    let code = sstore_code();
    let registry = warm_compile(ArbSpecId::NITRO, &[code.as_ref()]);

    let mut db_i = CacheDB::new(EmptyDB::default());
    insert_code(&mut db_i, CONTRACT, code.clone());
    let interpreted = transact(&mut db_i, unsigned_call(CONTRACT, 100_000), None);
    let slot_i = slot0(&interpreted.state, CONTRACT);

    let mut db_c = CacheDB::new(EmptyDB::default());
    insert_code(&mut db_c, CONTRACT, code);
    let compiled = transact(
        &mut db_c,
        unsigned_call(CONTRACT, 100_000),
        Some(registry.clone()),
    );
    let slot_c = slot0(&compiled.state, CONTRACT);

    assert!(interpreted.result.is_success(), "{:?}", interpreted.result);
    assert_same_outcome(&interpreted.result, &compiled.result);
    assert_eq!(slot_i, U256::from(0x2a));
    assert_eq!(slot_i, slot_c);
    assert!(registry.dispatch_hits() >= 1);
}

#[test]
fn compiled_nested_call_matches_interpreter() {
    let callee_code = return_2a_code();
    let caller_code = call_callee_code(CALLEE);
    let registry = warm_compile(
        ArbSpecId::NITRO,
        &[callee_code.as_ref(), caller_code.as_ref()],
    );

    let mut db_i = CacheDB::new(EmptyDB::default());
    insert_code(&mut db_i, CALLEE, callee_code.clone());
    insert_code(&mut db_i, CONTRACT, caller_code.clone());
    let interpreted = transact_call(&mut db_i, CONTRACT, 200_000, None);

    let mut db_c = CacheDB::new(EmptyDB::default());
    insert_code(&mut db_c, CALLEE, callee_code.clone());
    insert_code(&mut db_c, CONTRACT, caller_code.clone());
    let compiled = transact_call(&mut db_c, CONTRACT, 200_000, Some(registry.clone()));

    assert!(interpreted.is_success(), "{interpreted:?}");
    assert_same_outcome(&interpreted, &compiled);
    assert_eq!(compiled.output(), Some(&word32(0x2a)));
    let caller_hash = keccak256(&caller_code);
    let callee_hash = keccak256(&callee_code);
    assert!(
        registry.dispatch_hits_for(caller_hash) >= 1,
        "caller must dispatch, hits={}",
        registry.dispatch_hits_for(caller_hash)
    );
    assert!(
        registry.dispatch_hits_for(callee_hash) >= 1,
        "callee must dispatch compiled, hits={}",
        registry.dispatch_hits_for(callee_hash)
    );
}

#[test]
fn initcode_create_does_not_dispatch_compiled_program() {
    let initcode = arithmetic_code();
    let registry = warm_compile(ArbSpecId::NITRO, &[initcode.as_ref()]);

    let mut db = CacheDB::new(EmptyDB::default());
    let result = transact(
        &mut db,
        unsigned_create(initcode, 200_000),
        Some(registry.clone()),
    )
    .result;
    assert!(result.is_success(), "{result:?}");
    assert_eq!(
        registry.dispatch_hits(),
        0,
        "CREATE/initcode must fall back to the interpreter"
    );
}

#[test]
fn unknown_codehash_does_not_dispatch() {
    let registry = warm_compile(ArbSpecId::NITRO, &[arithmetic_code().as_ref()]);
    let other = sstore_code();
    let mut db = CacheDB::new(EmptyDB::default());
    insert_code(&mut db, CONTRACT, other);
    let result = transact_call(&mut db, CONTRACT, 100_000, Some(registry.clone()));
    assert!(result.is_success(), "{result:?}");
    assert_eq!(registry.dispatch_hits(), 0);
}

#[test]
fn spec_mismatch_does_not_dispatch() {
    let code = arithmetic_code();
    let registry = warm_compile(ArbSpecId::ARBOS_20, &[code.as_ref()]);
    let mut db = CacheDB::new(EmptyDB::default());
    insert_code(&mut db, CONTRACT, code);
    // Live EVM is NITRO (ArbOS 40); registry is bound to ArbOS 20.
    let result = transact_call(&mut db, CONTRACT, 100_000, Some(registry.clone()));
    assert!(result.is_success(), "{result:?}");
    assert_eq!(registry.dispatch_hits(), 0);
}

#[test]
fn bridged_opcodes_admitted_at_compile() {
    let mut registry = CompiledFrameRegistry::new(ArbSpecId::NITRO).expect("llvm");
    let number = number_sstore_code();
    let blockhash = vec![0x60, 0x01, 0x40, 0x00];
    assert!(bytecode_ineligible(number.as_ref()).is_none());
    assert!(bytecode_ineligible(&blockhash).is_none());
    assert!(registry.compile(number.as_ref()).is_ok());
    assert!(registry.compile(&blockhash).is_ok());
}

#[test]
fn push_immediate_with_number_byte_compiles_and_matches() {
    // PUSH1 0x43 PUSH1 0 MSTORE PUSH1 32 PUSH1 0 RETURN
    let code = Bytes::from(vec![
        0x60, 0x43, 0x60, 0x00, 0x52, 0x60, 0x20, 0x60, 0x00, 0xf3,
    ]);
    assert!(bytecode_ineligible(code.as_ref()).is_none());
    let registry = warm_compile(ArbSpecId::NITRO, &[code.as_ref()]);

    let mut db_i = CacheDB::new(EmptyDB::default());
    insert_code(&mut db_i, CONTRACT, code.clone());
    let interpreted = transact_call(&mut db_i, CONTRACT, 100_000, None);

    let mut db_c = CacheDB::new(EmptyDB::default());
    insert_code(&mut db_c, CONTRACT, code);
    let compiled = transact_call(&mut db_c, CONTRACT, 100_000, Some(registry.clone()));

    assert_same_outcome(&interpreted, &compiled);
    assert_eq!(compiled.output(), Some(&word32(0x43)));
    assert!(registry.dispatch_hits() >= 1);
}

#[test]
fn denylist_covers_arb_instruction_overrides() {
    assert_eq!(
        ARB_INSTRUCTION_OVERRIDES,
        &[0x40, 0x43],
        "must stay aligned with evm.rs insert_instruction + table-diff test"
    );
    assert_eq!(COMPILED_HOST_BRIDGED_OVERRIDES, &[0x40, 0x43]);
    for &op in ARB_INSTRUCTION_OVERRIDES {
        let bridged = COMPILED_HOST_BRIDGED_OVERRIDES.contains(&op);
        let ineligible = bytecode_ineligible(&[op, 0x00]).is_some();
        assert_ne!(
            bridged, ineligible,
            "override 0x{op:02x} must be host-bridged or refused, not both/neither"
        );
    }
}

/// GAS followed by MSTORE/RETURN in the same basic block. If a gas section
/// precharged those later static costs, the GAS value (and output) would diverge.
#[test]
fn compiled_gas_opcode_matches_interpreter_with_trailing_static_ops() {
    let code = Bytes::from(vec![
        0x60, 0x01, 0x60, 0x02, 0x01, // PUSH1 1 PUSH1 2 ADD
        0x5a, // GAS
        0x60, 0x00, 0x52, 0x60, 0x20, 0x60, 0x00, 0xf3,
    ]);
    let registry = warm_compile(ArbSpecId::NITRO, &[code.as_ref()]);
    let mut db_i = CacheDB::new(EmptyDB::default());
    insert_code(&mut db_i, CONTRACT, code.clone());
    let interpreted = transact_call(&mut db_i, CONTRACT, 100_000, None);
    let mut db_c = CacheDB::new(EmptyDB::default());
    insert_code(&mut db_c, CONTRACT, code);
    let compiled = transact_call(&mut db_c, CONTRACT, 100_000, Some(registry.clone()));
    assert!(interpreted.is_success(), "{interpreted:?}");
    assert_same_outcome(&interpreted, &compiled);
    assert!(registry.dispatch_hits() >= 1);
}

/// EIP-2200 sentry: SSTORE fails if gas left ≤ 2300. Trailing PUSH/POP after
/// SSTORE must not be precharged into that remaining-gas check.
#[test]
fn compiled_sstore_sentry_matches_interpreter() {
    // PUSH1 1 PUSH1 0 SSTORE PUSH1 0xff POP STOP — slot already 1 (no-op SSTORE).
    let code = Bytes::from(vec![0x60, 0x01, 0x60, 0x00, 0x55, 0x60, 0xff, 0x50, 0x00]);
    let registry = warm_compile(ArbSpecId::NITRO, &[code.as_ref()]);
    // 21_000 intrinsic + 2×PUSH1 (6) + 2301 remaining at SSTORE.
    let just_above_sentry = 21_000 + 6 + 2_301;

    for gas_limit in [just_above_sentry, just_above_sentry - 1, 100_000_u64] {
        let mut db_i = CacheDB::new(EmptyDB::default());
        insert_code(&mut db_i, CONTRACT, code.clone());
        db_i.insert_account_storage(CONTRACT, U256::ZERO, U256::from(1))
            .unwrap();
        let interpreted = transact(&mut db_i, unsigned_call(CONTRACT, gas_limit), None);

        let mut db_c = CacheDB::new(EmptyDB::default());
        insert_code(&mut db_c, CONTRACT, code.clone());
        db_c.insert_account_storage(CONTRACT, U256::ZERO, U256::from(1))
            .unwrap();
        let compiled = transact(
            &mut db_c,
            unsigned_call(CONTRACT, gas_limit),
            Some(registry.clone()),
        );

        assert_same_outcome(&interpreted.result, &compiled.result);
        assert_eq!(
            slot0(&interpreted.state, CONTRACT),
            slot0(&compiled.state, CONTRACT),
            "storage diverged at gas_limit={gas_limit}"
        );
    }
    assert!(registry.dispatch_hits() >= 1);
}

/// CALL stipend is `GAS` (exact remaining), so 63/64 forwarding is observable
/// via the callee's returned GAS value.
#[test]
fn compiled_call_gas_stipend_matches_interpreter() {
    let callee_code = Bytes::from(vec![
        0x5a, 0x60, 0x00, 0x52, 0x60, 0x20, 0x60, 0x00, 0xf3, // GAS, return it
    ]);
    let mut caller_code = vec![
        0x60, 0x20, 0x60, 0x00, 0x60, 0x00, 0x60, 0x00, 0x60, 0x00, 0x73,
    ];
    caller_code.extend_from_slice(CALLEE.as_slice());
    caller_code.extend_from_slice(&[
        0x5a, // GAS as stipend
        0xf1, // CALL
        0x60, 0x20, 0x60, 0x00, 0xf3,
    ]);
    let caller_code = Bytes::from(caller_code);
    let registry = warm_compile(
        ArbSpecId::NITRO,
        &[callee_code.as_ref(), caller_code.as_ref()],
    );

    let mut db_i = CacheDB::new(EmptyDB::default());
    insert_code(&mut db_i, CALLEE, callee_code.clone());
    insert_code(&mut db_i, CONTRACT, caller_code.clone());
    let interpreted = transact_call(&mut db_i, CONTRACT, 200_000, None);

    let mut db_c = CacheDB::new(EmptyDB::default());
    insert_code(&mut db_c, CALLEE, callee_code.clone());
    insert_code(&mut db_c, CONTRACT, caller_code.clone());
    let compiled = transact_call(&mut db_c, CONTRACT, 200_000, Some(registry.clone()));

    assert!(interpreted.is_success(), "{interpreted:?}");
    assert_same_outcome(&interpreted, &compiled);
    assert!(
        registry.dispatch_hits_for(keccak256(&callee_code)) >= 1,
        "callee must run compiled"
    );
    assert!(
        registry.dispatch_hits_for(keccak256(&caller_code)) >= 1,
        "caller must run compiled"
    );
}

#[test]
fn compiled_revert_keeps_remaining_gas() {
    let code = Bytes::from(vec![0x60, 0x00, 0x60, 0x00, 0xfd]); // PUSH1 0 PUSH1 0 REVERT
    let registry = warm_compile(ArbSpecId::NITRO, &[code.as_ref()]);
    let mut db_i = CacheDB::new(EmptyDB::default());
    insert_code(&mut db_i, CONTRACT, code.clone());
    let interpreted = transact_call(&mut db_i, CONTRACT, 100_000, None);
    let mut db_c = CacheDB::new(EmptyDB::default());
    insert_code(&mut db_c, CONTRACT, code);
    let compiled = transact_call(&mut db_c, CONTRACT, 100_000, Some(registry));
    assert!(
        matches!(interpreted, ExecutionResult::Revert { .. }),
        "{interpreted:?}"
    );
    assert!(interpreted.tx_gas_used() < 100_000);
    assert_same_outcome(&interpreted, &compiled);
}

/// Reverted child CALL must resume the compiled caller with the same success flag.
#[test]
fn compiled_caller_resumes_after_reverted_child() {
    let callee_code = Bytes::from(vec![0x60, 0x00, 0x60, 0x00, 0xfd]);
    let mut caller_code = vec![
        0x60, 0x00, 0x60, 0x00, 0x60, 0x00, 0x60, 0x00, 0x60, 0x00, 0x73,
    ];
    caller_code.extend_from_slice(CALLEE.as_slice());
    caller_code.extend_from_slice(&[
        0x61, 0x40, 0x00, 0xf1, // PUSH2 gas, CALL
        0x15, // ISZERO
        0x60, 0x00, 0x52, 0x60, 0x20, 0x60, 0x00, 0xf3,
    ]);
    let caller_code = Bytes::from(caller_code);
    let registry = warm_compile(
        ArbSpecId::NITRO,
        &[callee_code.as_ref(), caller_code.as_ref()],
    );

    let mut db_i = CacheDB::new(EmptyDB::default());
    insert_code(&mut db_i, CALLEE, callee_code.clone());
    insert_code(&mut db_i, CONTRACT, caller_code.clone());
    let interpreted = transact_call(&mut db_i, CONTRACT, 200_000, None);

    let mut db_c = CacheDB::new(EmptyDB::default());
    insert_code(&mut db_c, CALLEE, callee_code.clone());
    insert_code(&mut db_c, CONTRACT, caller_code.clone());
    let compiled = transact_call(&mut db_c, CONTRACT, 200_000, Some(registry.clone()));

    assert!(interpreted.is_success(), "{interpreted:?}");
    assert_eq!(interpreted.output(), Some(&word32(1)));
    assert_same_outcome(&interpreted, &compiled);
    assert!(registry.dispatch_hits_for(keccak256(&caller_code)) >= 2);
    assert!(registry.dispatch_hits_for(keccak256(&callee_code)) >= 1);
}

#[test]
fn compiled_number_returns_l1_not_l2_and_matches_interpreter() {
    let code = number_sstore_code();
    let registry = warm_compile(ArbSpecId::NITRO, &[code.as_ref()]);

    let mut db_i = CacheDB::new(EmptyDB::default());
    insert_code(&mut db_i, CONTRACT, code.clone());
    let interpreted = transact(&mut db_i, unsigned_call(CONTRACT, 100_000), None);

    let mut db_c = CacheDB::new(EmptyDB::default());
    insert_code(&mut db_c, CONTRACT, code);
    let compiled = transact(
        &mut db_c,
        unsigned_call(CONTRACT, 100_000),
        Some(registry.clone()),
    );

    assert!(interpreted.result.is_success(), "{:?}", interpreted.result);
    assert_same_outcome(&interpreted.result, &compiled.result);
    assert_eq!(slot0(&interpreted.state, CONTRACT), U256::from(777_u64));
    assert_eq!(slot0(&compiled.state, CONTRACT), U256::from(777_u64));
    assert_ne!(slot0(&compiled.state, CONTRACT), U256::from(1000_u64));
    assert_eq!(interpreted.state, compiled.state);
    assert!(registry.dispatch_hits() >= 1);
}

#[test]
fn compiled_nested_number_matches_interpreter() {
    let callee_code = number_sstore_code();
    let mut caller_code = vec![0x43, 0x60, 0x00, 0x55]; // NUMBER PUSH1 0 SSTORE
    caller_code.extend_from_slice(&[
        0x60, 0x00, 0x60, 0x00, 0x60, 0x00, 0x60, 0x00, 0x60, 0x00, 0x73,
    ]);
    caller_code.extend_from_slice(CALLEE.as_slice());
    caller_code.extend_from_slice(&[0x5a, 0xf1, 0x50, 0x00]); // GAS CALL POP STOP
    let caller_code = Bytes::from(caller_code);
    let registry = warm_compile(
        ArbSpecId::NITRO,
        &[callee_code.as_ref(), caller_code.as_ref()],
    );

    let mut db_i = CacheDB::new(EmptyDB::default());
    insert_code(&mut db_i, CALLEE, callee_code.clone());
    insert_code(&mut db_i, CONTRACT, caller_code.clone());
    let interpreted = transact(&mut db_i, unsigned_call(CONTRACT, 200_000), None);

    let mut db_c = CacheDB::new(EmptyDB::default());
    insert_code(&mut db_c, CALLEE, callee_code.clone());
    insert_code(&mut db_c, CONTRACT, caller_code.clone());
    let compiled = transact(
        &mut db_c,
        unsigned_call(CONTRACT, 200_000),
        Some(registry.clone()),
    );

    assert!(interpreted.result.is_success(), "{:?}", interpreted.result);
    assert_same_outcome(&interpreted.result, &compiled.result);
    assert_eq!(slot0(&interpreted.state, CONTRACT), U256::from(777_u64));
    assert_eq!(slot0(&interpreted.state, CALLEE), U256::from(777_u64));
    assert_eq!(slot0(&compiled.state, CONTRACT), U256::from(777_u64));
    assert_eq!(slot0(&compiled.state, CALLEE), U256::from(777_u64));
    assert_eq!(interpreted.state, compiled.state);
    assert!(registry.dispatch_hits_for(keccak256(&caller_code)) >= 1);
    assert!(registry.dispatch_hits_for(keccak256(&callee_code)) >= 1);
}

#[test]
fn compiled_number_leaves_block_env_unchanged() {
    let code = number_return_code();
    let registry = warm_compile(ArbSpecId::NITRO, &[code.as_ref()]);
    let mut db = CacheDB::new(EmptyDB::default());
    insert_code(&mut db, CONTRACT, code);

    let chain = ArbChainContext::new(None).with_l1_block_number(777);
    let mut block = BlockEnv::default();
    block.number = U256::from(1000);
    let ctx: ArbContext<&mut _> = ArbContext::arb_with_chain_context(chain)
        .with_db(&mut db)
        .with_cfg(cfg())
        .with_block(block)
        .with_tx(ArbTransaction::<TxEnv>::default());
    let mut evm = ctx.build_arb();
    evm.set_compiled_programs(Some(registry.clone()));
    let out = evm
        .transact(unsigned_call(CONTRACT, 100_000))
        .expect("execution");

    assert!(out.result.is_success(), "{:?}", out.result);
    assert_eq!(out.result.output(), Some(&word_u256(777)));
    assert_eq!(evm.0.ctx.block.number, U256::from(1000_u64));
    assert_eq!(Host::block_number(&evm.0.ctx), U256::from(1000_u64));
    assert_eq!(evm.0.ctx.chain.l1_block_number, 777);
    assert!(registry.dispatch_hits() >= 1);
}

#[test]
fn compiled_caller_number_after_reverted_child() {
    let callee_code = Bytes::from(vec![0x60, 0x00, 0x60, 0x00, 0xfd]);
    let mut caller_code = vec![
        0x60, 0x00, 0x60, 0x00, 0x60, 0x00, 0x60, 0x00, 0x60, 0x00, 0x73,
    ];
    caller_code.extend_from_slice(CALLEE.as_slice());
    caller_code.extend_from_slice(&[
        0x61, 0x40, 0x00, 0xf1, // CALL
        0x50, // POP
        0x43, 0x60, 0x00, 0x52, 0x60, 0x20, 0x60, 0x00, 0xf3, // NUMBER, RETURN it
    ]);
    let caller_code = Bytes::from(caller_code);
    let registry = warm_compile(
        ArbSpecId::NITRO,
        &[callee_code.as_ref(), caller_code.as_ref()],
    );

    let mut db_i = CacheDB::new(EmptyDB::default());
    insert_code(&mut db_i, CALLEE, callee_code.clone());
    insert_code(&mut db_i, CONTRACT, caller_code.clone());
    let interpreted = transact_call(&mut db_i, CONTRACT, 200_000, None);

    let mut db_c = CacheDB::new(EmptyDB::default());
    insert_code(&mut db_c, CALLEE, callee_code.clone());
    insert_code(&mut db_c, CONTRACT, caller_code.clone());
    let compiled = transact_call(&mut db_c, CONTRACT, 200_000, Some(registry.clone()));

    assert!(interpreted.is_success(), "{interpreted:?}");
    assert_eq!(interpreted.output(), Some(&word_u256(777)));
    assert_same_outcome(&interpreted, &compiled);
    assert!(registry.dispatch_hits_for(keccak256(&caller_code)) >= 2);
}

#[test]
fn blockhash_admitted_by_ring_bound_registry() {
    let mut registry = CompiledFrameRegistry::new(ArbSpecId::NITRO).expect("llvm");
    let blockhash_only = vec![0x60, 0x01, 0x40, 0x00];
    let number_then_blockhash = vec![0x43, 0x60, 0x01, 0x40, 0x00];
    assert!(bytecode_ineligible(&blockhash_only).is_none());
    assert!(bytecode_ineligible(&number_then_blockhash).is_none());
    assert!(registry.compile(&blockhash_only).is_ok());
    assert!(registry.compile(&number_then_blockhash).is_ok());
}

fn seed_l1_ring(db: &mut CacheDB<EmptyDB>) {
    use arb_revm::{
        constants::ARBOS_STATE_ADDRESS,
        storage::{StorageSpace, Subspace},
    };
    let space = StorageSpace::arbos().open_subspace_with_key(Subspace::BlockHashes as u8);
    for (key, value) in [
        (0, U256::from(778)),
        (1 + 777 % 256, U256::from(0x1234)),
        (1 + 522 % 256, U256::from(0x5678)),
    ] {
        db.insert_account_storage(
            ARBOS_STATE_ADDRESS,
            U256::from_be_bytes(space.slot_for_u256(U256::from(key)).0),
            value,
        )
        .unwrap();
    }
}

fn ring_return_code(request: U256) -> Bytes {
    let mut code = vec![0x7f];
    code.extend_from_slice(&request.to_be_bytes::<32>());
    // Repeat the lookup to exercise warm storage, then return the second hash.
    code.extend_from_slice(&[
        0x80, 0x40, 0x50, 0x40, 0x60, 0, 0x52, 0x60, 32, 0x60, 0, 0xf3,
    ]);
    code.into()
}

#[test]
fn compiled_ring_boundaries_and_warm_reads_match_full_state() {
    for (request, expected) in [
        (U256::from(777), 0x1234),
        (U256::from(522), 0x5678),
        (U256::from(521), 0),
        (U256::from(778), 0),
        (U256::from(779), 0),
        (U256::MAX, 0),
        (U256::from(776), 0),
    ] {
        let code = ring_return_code(request);
        let registry = warm_compile(ArbSpecId::NITRO, &[&code]);
        let mut interpreted_db = CacheDB::new(EmptyDB::default());
        seed_l1_ring(&mut interpreted_db);
        insert_code(&mut interpreted_db, CONTRACT, code);
        let mut compiled_db = interpreted_db.clone();
        let interpreted = transact(&mut interpreted_db, unsigned_call(CONTRACT, 200_000), None);
        let compiled = transact(
            &mut compiled_db,
            unsigned_call(CONTRACT, 200_000),
            Some(registry.clone()),
        );
        assert!(interpreted.result.is_success(), "{interpreted:?}");
        assert_eq!(interpreted.result.output(), Some(&word_u256(expected)));
        assert_eq!(interpreted, compiled, "request {request}");
        assert!(registry.dispatch_hits() > 0);
        assert_eq!(
            registry.identity().block_hash_semantics,
            "arbos-l1-ring-blockhash-v1"
        );
    }
}

#[test]
fn compiled_ring_nested_resume_matches_full_state() {
    let callee = ring_return_code(U256::from(522));
    let mut caller = call_callee_code(CALLEE).to_vec();
    caller.truncate(caller.len() - 5); // after CALL, replace return with resumed ring lookup
    caller.push(0x50); // discard CALL result
    caller.extend_from_slice(&ring_return_code(U256::from(777)));
    let caller = Bytes::from(caller);
    let registry = warm_compile(ArbSpecId::NITRO, &[&caller, &callee]);
    let mut db_i = CacheDB::new(EmptyDB::default());
    seed_l1_ring(&mut db_i);
    insert_code(&mut db_i, CONTRACT, caller.clone());
    insert_code(&mut db_i, CALLEE, callee.clone());
    let mut db_c = db_i.clone();
    let interpreted = transact(&mut db_i, unsigned_call(CONTRACT, 200_000), None);
    let compiled = transact(
        &mut db_c,
        unsigned_call(CONTRACT, 200_000),
        Some(registry.clone()),
    );
    assert!(interpreted.result.is_success(), "{interpreted:?}");
    assert_eq!(interpreted.result.output(), Some(&word_u256(0x1234)));
    assert_eq!(interpreted, compiled);
    assert!(registry.dispatch_hits_for(keccak256(&caller)) >= 2);
    assert!(registry.dispatch_hits_for(keccak256(&callee)) >= 1);
}
