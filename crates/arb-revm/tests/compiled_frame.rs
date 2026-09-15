//! Compiled vs interpreted parity for the default-off compiled-frame PoC.
//!
//! These tests need LLVM 22 (`/opt/homebrew/opt/llvm@22`) and
//! `--features compiled-frame`. Compile is warm (outside `transact`); `frame_run`
//! only looks up resident programs.

#![cfg(feature = "compiled-frame")]
#![allow(clippy::field_reassign_with_default)]

use arb_revm::transaction::arb_envelope_to_tx_env;
use arb_revm::{
    ArbBuilder, ArbChainContext, ArbContext, ArbSpecId, ArbTransaction, CompiledFrameRegistry,
    DefaultArb, bytecode_ineligible,
};
use arbitrum_alloy_consensus::transactions::{ArbTxEnvelope, TxUnsigned};
use revm::{
    ExecuteEvm,
    context::{BlockEnv, CfgEnv, TxEnv, result::ExecutionResult},
    database::{CacheDB, EmptyDB},
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
    assert_eq!(
        interpreted.tx_gas_used(),
        compiled.tx_gas_used(),
        "gas_used mismatch\ninterpreted={interpreted:?}\ncompiled={compiled:?}"
    );
    assert_eq!(
        interpreted.output(),
        compiled.output(),
        "output mismatch\ninterpreted={interpreted:?}\ncompiled={compiled:?}"
    );
    assert_eq!(
        interpreted.is_success(),
        compiled.is_success(),
        "success mismatch\ninterpreted={interpreted:?}\ncompiled={compiled:?}"
    );
    assert_eq!(
        interpreted.is_halt(),
        compiled.is_halt(),
        "halt mismatch\ninterpreted={interpreted:?}\ncompiled={compiled:?}"
    );
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
    insert_code(&mut db_c, CALLEE, callee_code);
    insert_code(&mut db_c, CONTRACT, caller_code);
    let compiled = transact_call(&mut db_c, CONTRACT, 200_000, Some(registry.clone()));

    assert!(interpreted.is_success(), "{interpreted:?}");
    assert_same_outcome(&interpreted, &compiled);
    assert_eq!(compiled.output(), Some(&word32(0x2a)));
    // Caller frame + callee frame.
    assert!(
        registry.dispatch_hits() >= 2,
        "nested frames should both dispatch, hits={}",
        registry.dispatch_hits()
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
fn excluded_opcodes_rejected_at_compile() {
    let mut registry = CompiledFrameRegistry::new(ArbSpecId::NITRO).expect("llvm");
    let number = vec![0x43, 0x60, 0x00, 0x55, 0x00];
    let blockhash = vec![0x60, 0x01, 0x40, 0x00];
    assert!(bytecode_ineligible(&number).is_some());
    assert!(bytecode_ineligible(&blockhash).is_some());
    assert!(registry.compile(&number).is_err());
    assert!(registry.compile(&blockhash).is_err());
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
