//! Default-off, bounded diagnostics for the core transaction handler.
//!
//! Records `ExecuteEvm::transact` and `replay`. Node receipt/state commit and retry
//! scheduling are outside this recorder. Phase durations are inclusive: `Vm` is
//! a child of `Execution`, so summing every phase double counts work.
use revm::context_interface::result::{EVMError, ExecutionResult};
use serde::Serialize;
use std::{cell::RefCell, time::Instant};

pub const MAX_ATTEMPTS: usize = 65_536;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub enum Phase {
    Validate,
    TxGas,
    PreExecution,
    Execution,
    Vm,
    RuntimeOog,
    Settlement,
    Result,
    ErrorCleanup,
    Finalize,
}
impl Phase {
    pub const COUNT: usize = 10;
    pub const ALL: [Self; Self::COUNT] = [
        Self::Validate,
        Self::TxGas,
        Self::PreExecution,
        Self::Execution,
        Self::Vm,
        Self::RuntimeOog,
        Self::Settlement,
        Self::Result,
        Self::ErrorCleanup,
        Self::Finalize,
    ];
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub enum Outcome {
    Success,
    Revert,
    Halt,
    InvalidTransaction,
    HeaderError,
    DatabaseError,
    CustomError,
    Interrupted,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub enum Entry {
    Transact,
    Replay,
}
#[derive(Clone, Copy, Debug, Default, Serialize)]
pub struct PhaseSample {
    pub calls: u32,
    pub elapsed_ns: u64,
}
#[derive(Debug, Serialize)]
pub struct Attempt {
    pub id: u64,
    /// Nested core invocation, not a scheduler/retry dependency.
    pub parent_id: Option<u64>,
    pub entry: Entry,
    pub tx_type: u8,
    pub nonce: u64,
    pub outcome: Outcome,
    pub terminal_phase: Option<Phase>,
    pub error_phase: Option<Phase>,
    pub elapsed_ns: u64,
    /// Indexed by Phase discriminant. None means the phase was not entered.
    pub phases: [Option<PhaseSample>; Phase::COUNT],
}
#[derive(Debug, Serialize)]
pub struct Report {
    pub phase_order: [Phase; Phase::COUNT],
    pub attempts: Vec<Attempt>,
    pub attempted: u64,
    pub dropped: u64,
    pub complete: bool,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScopeError {
    NestedScope,
    InvalidCapacity,
    Allocation,
}
struct Recorder {
    report: Report,
    capacity: usize,
    current: Option<usize>,
}
thread_local! { static RECORDER: RefCell<Option<Recorder>> = const { RefCell::new(None) }; }

struct Scope;
impl Drop for Scope {
    fn drop(&mut self) {
        RECORDER.with(|r| {
            r.borrow_mut().take();
        });
    }
}

/// Preallocates before `run`; does not serialize or perform I/O. Rejects nested
/// recording scopes without invoking their closure. Fresh OS threads do not
/// inherit this thread-local recorder. A dropped record makes `complete=false`.
pub fn with_recording<T>(
    capacity: usize,
    run: impl FnOnce() -> T,
) -> Result<(T, Report), ScopeError> {
    if capacity == 0 || capacity > MAX_ATTEMPTS {
        return Err(ScopeError::InvalidCapacity);
    }
    if RECORDER.with(|r| r.borrow().is_some()) {
        return Err(ScopeError::NestedScope);
    }
    let mut attempts = Vec::new();
    attempts
        .try_reserve_exact(capacity)
        .map_err(|_| ScopeError::Allocation)?;
    RECORDER.with(|r| {
        *r.borrow_mut() = Some(Recorder {
            report: Report {
                phase_order: Phase::ALL,
                attempts,
                attempted: 0,
                dropped: 0,
                complete: true,
            },
            capacity,
            current: None,
        })
    });
    let scope = Scope;
    let result = run();
    let report = RECORDER.with(|r| {
        r.borrow_mut()
            .take()
            .expect("recording scope owns recorder")
            .report
    });
    drop(scope);
    Ok((result, report))
}

pub(crate) fn active_attempt() -> bool {
    RECORDER.with(|r| r.borrow().as_ref().is_some_and(|r| r.current.is_some()))
}

pub(crate) struct AttemptGuard {
    id: Option<usize>,
    previous: Option<usize>,
    started: Option<Instant>,
    attached: bool,
}
impl AttemptGuard {
    pub(crate) fn begin(entry: Entry, tx_type: u8, nonce: u64) -> Self {
        RECORDER.with(|r| {
            let mut r = r.borrow_mut();
            let Some(r) = r.as_mut() else {
                return Self {
                    id: None,
                    previous: None,
                    started: None,
                    attached: false,
                };
            };
            let previous = r.current;
            let id = r.report.attempted;
            r.report.attempted += 1;
            if r.report.attempts.len() == r.capacity {
                r.report.dropped += 1;
                r.report.complete = false;
                r.current = None; // An overflowed child must not charge phases to its parent.
                return Self {
                    id: None,
                    previous,
                    started: None,
                    attached: true,
                };
            }
            let parent_id = previous.map(|i| r.report.attempts[i].id);
            let index = r.report.attempts.len();
            r.report.attempts.push(Attempt {
                id,
                parent_id,
                entry,
                tx_type,
                nonce,
                outcome: Outcome::Interrupted,
                terminal_phase: None,
                error_phase: None,
                elapsed_ns: 0,
                phases: [None; Phase::COUNT],
            });
            r.current = Some(index);
            Self {
                id: Some(index),
                previous,
                started: Some(Instant::now()),
                attached: true,
            }
        })
    }
    pub(crate) fn outcome<H, D, E>(&self, result: &Result<ExecutionResult<H>, EVMError<D, E>>) {
        let status = match result {
            Ok(ExecutionResult::Success { .. }) => Outcome::Success,
            Ok(ExecutionResult::Revert { .. }) => Outcome::Revert,
            Ok(ExecutionResult::Halt { .. }) => Outcome::Halt,
            Err(EVMError::Transaction(_)) => Outcome::InvalidTransaction,
            Err(EVMError::Header(_)) => Outcome::HeaderError,
            Err(EVMError::Database(_)) => Outcome::DatabaseError,
            Err(EVMError::Custom(_) | EVMError::CustomAny(_)) => Outcome::CustomError,
        };
        if let Some(id) = self.id {
            RECORDER.with(|r| {
                if let Some(r) = r.borrow_mut().as_mut() {
                    r.report.attempts[id].outcome = status;
                }
            });
        }
    }
}
impl Drop for AttemptGuard {
    fn drop(&mut self) {
        if !self.attached {
            return;
        }
        let elapsed = self.started.map(|s| nanos(s.elapsed().as_nanos()));
        RECORDER.with(|r| {
            if let Some(r) = r.borrow_mut().as_mut() {
                if let (Some(id), Some(elapsed)) = (self.id, elapsed) {
                    let attempt = &mut r.report.attempts[id];
                    attempt.elapsed_ns = elapsed;
                    if std::thread::panicking() {
                        attempt.outcome = Outcome::Interrupted;
                    }
                    if attempt.outcome == Outcome::Interrupted {
                        r.report.complete = false;
                    }
                }
                r.current = self.previous;
            }
        });
    }
}
fn nanos(n: u128) -> u64 {
    n.min(u64::MAX as u128) as u64
}

pub(crate) struct PhaseGuard {
    id: Option<usize>,
    phase: Phase,
    started: Option<Instant>,
}
impl PhaseGuard {
    pub(crate) fn enter(phase: Phase) -> Self {
        let id = RECORDER.with(|r| {
            let mut r = r.borrow_mut();
            let r = r.as_mut()?;
            let id = r.current?;
            r.report.attempts[id].terminal_phase = Some(phase);
            Some(id)
        });
        Self {
            id,
            phase,
            started: id.map(|_| Instant::now()),
        }
    }
}
impl Drop for PhaseGuard {
    fn drop(&mut self) {
        let (Some(id), Some(started)) = (self.id, self.started) else {
            return;
        };
        let elapsed = nanos(started.elapsed().as_nanos());
        RECORDER.with(|r| {
            if let Some(r) = r.borrow_mut().as_mut() {
                let sample = r.report.attempts[id].phases[self.phase as usize]
                    .get_or_insert_with(PhaseSample::default);
                sample.calls = sample.calls.saturating_add(1);
                sample.elapsed_ns = sample.elapsed_ns.saturating_add(elapsed);
            }
        });
    }
}

/// Mirrors revm-handler 42.0.1 Handler::{run,run_without_catch_error}. Keep the
/// call order and `?` placement aligned with that pin. In particular invoke Arb
/// methods on `handler`, NEVER MainnetHandler's validation or settlement.
pub(crate) fn run<H: revm::handler::Handler>(
    handler: &mut H,
    evm: &mut H::Evm,
) -> Result<ExecutionResult<H::HaltReason>, H::Error> {
    use PhaseGuard as Guard;
    let mut error_boundary = Phase::Validate;
    let result = (|| {
        let init_and_floor_gas = {
            let _g = Guard::enter(Phase::Validate);
            handler.validate(evm)?
        };
        let mut gas = {
            let _g = Guard::enter(Phase::TxGas);
            handler.tx_gas(evm, &init_and_floor_gas)
        };
        let pre_execution = {
            error_boundary = Phase::PreExecution;
            let _g = Guard::enter(Phase::PreExecution);
            handler.pre_execution(evm, &mut gas)?
        };
        let refund = pre_execution.map(|pe| pe.eip7702_refund).unwrap_or(0) as i64;
        let mut exec_result = None;
        if let Some(pre_execution) = pre_execution {
            error_boundary = Phase::Execution;
            let _g = Guard::enter(Phase::Execution);
            exec_result = handler.execution(evm, pre_execution.checkpoint, &mut gas)?;
        }
        let mut exec_result = match exec_result {
            Some(exec_result) => exec_result,
            None => {
                error_boundary = Phase::RuntimeOog;
                let _g = Guard::enter(Phase::RuntimeOog);
                handler.runtime_oog_result(evm, &init_and_floor_gas, &mut gas)?
            }
        };
        let result_gas = {
            error_boundary = Phase::Settlement;
            let _g = Guard::enter(Phase::Settlement);
            handler.post_execution(evm, &mut exec_result, init_and_floor_gas, refund)?
        };
        error_boundary = Phase::Result;
        let _g = Guard::enter(Phase::Result);
        handler.execution_result(evm, exec_result, result_gas)
    })();
    match result {
        Ok(output) => Ok(output),
        Err(error) => {
            RECORDER.with(|r| {
                if let Some(r) = r.borrow_mut().as_mut() {
                    if let Some(id) = r.current {
                        let attempt = &mut r.report.attempts[id];
                        attempt.error_phase = Some(error_boundary);
                    }
                }
            });
            let _g = Guard::enter(Phase::ErrorCleanup);
            handler.catch_error(evm, error)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ArbSpecId, ArbTransaction, api::builder::ArbBuilder, chain::ArbChainContext};
    use revm::{
        Context, Database, ExecuteEvm, MainContext,
        context::{BlockEnv, CfgEnv, TxEnv},
        context_interface::result::{InvalidTransaction, ResultAndState},
        database::InMemoryDB,
        primitives::{Address, B256, Bytes, TxKind, U256},
        state::{AccountInfo, Bytecode},
    };
    use std::convert::Infallible;

    fn tx(nonce: u64) -> ArbTransaction<TxEnv> {
        let mut tx = TxEnv::default();
        tx.caller = Address::repeat_byte(0x22);
        tx.kind = TxKind::Call(Address::repeat_byte(0x11));
        tx.chain_id = Some(42161);
        tx.gas_limit = 100_000;
        tx.gas_price = 0;
        tx.nonce = nonce;
        ArbTransaction::new(tx)
    }
    fn cfg() -> CfgEnv<ArbSpecId> {
        CfgEnv::new_with_spec(ArbSpecId::NITRO)
            .with_chain_id(42161)
            .with_disable_priority_fee_check(true)
    }
    fn run_code(code: &[u8], nonce: u64) -> Result<ResultAndState, EVMError<Infallible>> {
        run_entry(code, nonce, false)
    }
    fn run_entry(
        code: &[u8],
        nonce: u64,
        replay: bool,
    ) -> Result<ResultAndState, EVMError<Infallible>> {
        let mut db = InMemoryDB::default();
        db.insert_account_info(
            Address::repeat_byte(0x22),
            AccountInfo::from_balance(U256::from(1_000_000)),
        );
        let bytecode = Bytecode::new_raw(Bytes::copy_from_slice(code));
        db.insert_account_info(
            Address::repeat_byte(0x11),
            AccountInfo {
                code_hash: bytecode.hash_slow(),
                code: Some(bytecode),
                ..Default::default()
            },
        );
        let ctx = Context::mainnet()
            .with_tx(tx(nonce))
            .with_cfg(cfg())
            .with_block(BlockEnv {
                basefee: 0,
                ..Default::default()
            })
            .with_chain(ArbChainContext::default())
            .with_db(db);
        let mut evm = ctx.build_arb();
        if replay {
            evm.replay()
        } else {
            evm.transact(tx(nonce))
        }
    }
    #[test]
    fn actual_success_revert_halt_and_invalid_match_unobserved_results_and_state() {
        for (code, nonce, outcome) in [
            (vec![0x00], 0, Outcome::Success),
            (vec![0x60, 0, 0x60, 0, 0xfd], 0, Outcome::Revert),
            (vec![0xfe], 0, Outcome::Halt),
            (vec![0], 1, Outcome::InvalidTransaction),
            // Nested CALL resumes parent; recorder emits one transaction attempt.
            (
                vec![
                    0x60, 0, 0x60, 0, 0x60, 0, 0x60, 0, 0x60, 0, 0x60, 0x33, 0x61, 0xff, 0xff,
                    0xf1, 0x00,
                ],
                0,
                Outcome::Success,
            ),
        ] {
            let baseline = run_code(&code, nonce);
            let (observed, report) = with_recording(2, || run_code(&code, nonce)).unwrap();
            assert_eq!(baseline, observed);
            assert!(report.complete);
            assert_eq!(report.attempts.len(), 1);
            let a = &report.attempts[0];
            assert_eq!(a.outcome, outcome);
            assert_eq!(a.phases[Phase::Finalize as usize].unwrap().calls, 1);
            assert_eq!(a.terminal_phase, Some(Phase::Finalize));
            if outcome == Outcome::InvalidTransaction {
                assert_eq!(a.error_phase, Some(Phase::Validate));
                assert!(a.phases[Phase::ErrorCleanup as usize].is_some());
                assert!(a.phases[Phase::Vm as usize].is_none());
                assert!(a.phases[Phase::Settlement as usize].is_none());
            } else {
                for p in [
                    Phase::Validate,
                    Phase::PreExecution,
                    Phase::Execution,
                    Phase::Vm,
                    Phase::Settlement,
                    Phase::Result,
                ] {
                    assert_eq!(a.phases[p as usize].unwrap().calls, 1);
                }
                assert!(a.error_phase.is_none());
                assert!(a.phases[Phase::ErrorCleanup as usize].is_none());
            }
        }
    }

    #[test]
    fn replay_retains_success_only_finalization() {
        for nonce in [0, 1] {
            let baseline = run_entry(&[0], nonce, true);
            let (observed, report) = with_recording(1, || run_entry(&[0], nonce, true)).unwrap();
            assert_eq!(baseline, observed);
            let attempt = &report.attempts[0];
            assert_eq!(attempt.entry, Entry::Replay);
            assert_eq!(
                attempt.phases[Phase::Finalize as usize].is_some(),
                nonce == 0
            );
            assert!(report.complete);
        }
    }

    struct FailingDb;
    impl Database for FailingDb {
        type Error = std::io::Error;
        fn basic(&mut self, _: Address) -> Result<Option<AccountInfo>, Self::Error> {
            Err(std::io::Error::other("injected"))
        }
        fn code_by_hash(&mut self, _: B256) -> Result<Bytecode, Self::Error> {
            Err(std::io::Error::other("injected"))
        }
        fn storage(&mut self, _: Address, _: U256) -> Result<U256, Self::Error> {
            Err(std::io::Error::other("injected"))
        }
        fn block_hash(&mut self, _: u64) -> Result<B256, Self::Error> {
            Err(std::io::Error::other("injected"))
        }
    }
    #[test]
    fn database_error_still_finalizes_and_records_cleanup() {
        let (result, report) = with_recording(1, || {
            Context::mainnet()
                .with_tx(ArbTransaction::<TxEnv>::default())
                .with_cfg(cfg())
                .with_block(BlockEnv {
                    basefee: 0,
                    ..Default::default()
                })
                .with_chain(ArbChainContext::default())
                .with_db(FailingDb)
                .build_arb()
                .transact(tx(0))
        })
        .unwrap();
        assert!(matches!(result, Err(EVMError::Database(_))));
        let a = &report.attempts[0];
        assert_eq!(a.outcome, Outcome::DatabaseError);
        assert!(a.phases[Phase::Finalize as usize].is_some());
        assert!(a.phases[Phase::ErrorCleanup as usize].is_some());
        assert!(report.complete);
    }
    #[test]
    fn nested_scope_rejected_overflow_explicit_and_inactive_path_unchanged() {
        assert!(!active_attempt());
        let baseline = run_code(&[0], 0);
        let (_, report) = with_recording(1, || {
            assert!(matches!(
                with_recording(1, || panic!("must not run")),
                Err(ScopeError::NestedScope)
            ));
            assert_eq!(baseline, run_code(&[0], 0));
            assert_eq!(baseline, run_code(&[0], 0));
        })
        .unwrap();
        assert_eq!(report.attempted, 2);
        assert_eq!(report.dropped, 1);
        assert_eq!(report.attempts.len(), 1);
        assert!(!report.complete);
        assert!(!active_attempt());
        assert_eq!(baseline, run_code(&[0], 0));
        assert!(matches!(
            with_recording(0, || ()),
            Err(ScopeError::InvalidCapacity)
        ));
        assert!(matches!(
            with_recording(MAX_ATTEMPTS + 1, || ()),
            Err(ScopeError::InvalidCapacity)
        ));
    }
    #[test]
    fn unwind_restores_scope_and_guards_do_not_hold_borrows() {
        let _ = std::panic::catch_unwind(|| {
            let _ = with_recording(1, || {
                let _attempt = AttemptGuard::begin(Entry::Transact, 0, 0);
                let _phase = PhaseGuard::enter(Phase::Validate);
                panic!("test unwind");
            });
        });
        let (_, report) = with_recording(1, || run_code(&[0], 0)).unwrap();
        assert!(report.complete);
    }
    #[test]
    fn nested_attempt_overflow_does_not_charge_parent() {
        let (_, report) = with_recording(1, || {
            let parent = AttemptGuard::begin(Entry::Transact, 0, 0);
            {
                let _child = AttemptGuard::begin(Entry::Transact, 0, 0);
                let _phase = PhaseGuard::enter(Phase::Vm);
            }
            assert!(active_attempt());
            let error: Result<ExecutionResult, EVMError<Infallible>> =
                Err(EVMError::Transaction(InvalidTransaction::NonceTooHigh {
                    tx: 1,
                    state: 0,
                }));
            parent.outcome(&error);
        })
        .unwrap();
        assert_eq!(report.dropped, 1);
        assert!(report.attempts[0].phases[Phase::Vm as usize].is_none());
    }
}
