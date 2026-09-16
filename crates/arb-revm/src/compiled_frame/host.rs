//! Compiled-only [`Host`] adapter: NUMBER reads L1 without writing `BlockEnv`.
//!
//! revmc `79e3c8ca` `__revmc_builtin_number` calls `ecx.host.block_number()`. Stock
//! `Context` Host returns the L2 header. The interpreter override
//! `arb_block_number` reads `chain().l1_block_number` instead.
//! This wrapper is used only at `EvmCompilerFn::call_with_interpreter`; it must
//! not be installed on the real EVM context.
//!
//! Every other [`Host`] method, including those with trait defaults (`sstore`,
//! `sload`, `balance`, `load_account_delegated`, `load_account_code`,
//! `load_account_code_hash`), is forwarded to the original host so an inner
//! override is not replaced by the trait default.
//!
//! The ArbOS constructor also routes BLOCKHASH directly to the journal-backed
//! L1 ring. It must be paired with the explicit ArbOS compiler semantic mode.

use revm::{
    context_interface::{
        ContextTr,
        cfg::GasParams,
        context::{SStoreResult, SelfDestructResult, StateLoad},
        host::LoadError,
        journaled_state::{AccountInfoLoad, AccountLoad},
    },
    interpreter::Host,
    primitives::{Address, B256, Bytes, Log, StorageKey, StorageValue, U256},
};

/// Stack-local Host used only while a compiled frame is running.
pub(crate) struct CompiledNumberHost<'a, H: Host + ?Sized> {
    inner: &'a mut H,
    l1_block_number: U256,
    block_hash_lookup: fn(&mut H, u64) -> Option<B256>,
}

impl<'a, H: Host + ?Sized> CompiledNumberHost<'a, H> {
    pub(crate) fn new(inner: &'a mut H, l1_block_number: u64) -> Self {
        Self {
            inner,
            l1_block_number: U256::from(l1_block_number),
            block_hash_lookup: H::block_hash,
        }
    }
}

impl<'a, H: Host + ContextTr> CompiledNumberHost<'a, H> {
    pub(crate) fn new_arb_ring(inner: &'a mut H, l1_block_number: u64) -> Self {
        Self {
            inner,
            l1_block_number: U256::from(l1_block_number),
            block_hash_lookup: |host, number| {
                Some(
                    crate::storage::ArbosState::open()
                        .block_hashes
                        .block_hash(number, host.journal_mut())
                        .unwrap_or(B256::ZERO),
                )
            },
        }
    }
}

impl<H: Host + ?Sized> Host for CompiledNumberHost<'_, H> {
    fn basefee(&self) -> U256 {
        self.inner.basefee()
    }

    fn blob_gasprice(&self) -> U256 {
        self.inner.blob_gasprice()
    }

    fn gas_limit(&self) -> U256 {
        self.inner.gas_limit()
    }

    fn difficulty(&self) -> U256 {
        self.inner.difficulty()
    }

    fn prevrandao(&self) -> Option<U256> {
        self.inner.prevrandao()
    }

    #[inline]
    fn block_number(&self) -> U256 {
        self.l1_block_number
    }

    fn timestamp(&self) -> U256 {
        self.inner.timestamp()
    }

    fn beneficiary(&self) -> Address {
        self.inner.beneficiary()
    }

    fn slot_num(&self) -> U256 {
        self.inner.slot_num()
    }

    fn chain_id(&self) -> U256 {
        self.inner.chain_id()
    }

    fn effective_gas_price(&self) -> U256 {
        self.inner.effective_gas_price()
    }

    fn caller(&self) -> Address {
        self.inner.caller()
    }

    fn blob_hash(&self, number: usize) -> Option<U256> {
        self.inner.blob_hash(number)
    }

    fn max_initcode_size(&self) -> usize {
        self.inner.max_initcode_size()
    }

    fn gas_params(&self) -> &GasParams {
        self.inner.gas_params()
    }

    fn is_amsterdam_eip8037_enabled(&self) -> bool {
        self.inner.is_amsterdam_eip8037_enabled()
    }

    fn block_hash(&mut self, number: u64) -> Option<B256> {
        (self.block_hash_lookup)(self.inner, number)
    }

    fn selfdestruct(
        &mut self,
        address: Address,
        target: Address,
        skip_cold_load: bool,
    ) -> Result<StateLoad<SelfDestructResult>, LoadError> {
        self.inner.selfdestruct(address, target, skip_cold_load)
    }

    fn log(&mut self, log: Log) {
        self.inner.log(log)
    }

    fn sstore_skip_cold_load(
        &mut self,
        address: Address,
        key: StorageKey,
        value: StorageValue,
        skip_cold_load: bool,
    ) -> Result<StateLoad<SStoreResult>, LoadError> {
        self.inner
            .sstore_skip_cold_load(address, key, value, skip_cold_load)
    }

    fn sstore(
        &mut self,
        address: Address,
        key: StorageKey,
        value: StorageValue,
    ) -> Option<StateLoad<SStoreResult>> {
        self.inner.sstore(address, key, value)
    }

    fn sload_skip_cold_load(
        &mut self,
        address: Address,
        key: StorageKey,
        skip_cold_load: bool,
    ) -> Result<StateLoad<StorageValue>, LoadError> {
        self.inner
            .sload_skip_cold_load(address, key, skip_cold_load)
    }

    fn sload(&mut self, address: Address, key: StorageKey) -> Option<StateLoad<StorageValue>> {
        self.inner.sload(address, key)
    }

    fn tstore(&mut self, address: Address, key: StorageKey, value: StorageValue) {
        self.inner.tstore(address, key, value)
    }

    fn tload(&mut self, address: Address, key: StorageKey) -> StorageValue {
        self.inner.tload(address, key)
    }

    fn load_account_info_skip_cold_load(
        &mut self,
        address: Address,
        load_code: bool,
        skip_cold_load: bool,
    ) -> Result<AccountInfoLoad<'_>, LoadError> {
        self.inner
            .load_account_info_skip_cold_load(address, load_code, skip_cold_load)
    }

    fn balance(&mut self, address: Address) -> Option<StateLoad<U256>> {
        self.inner.balance(address)
    }

    fn load_account_delegated(&mut self, address: Address) -> Option<StateLoad<AccountLoad>> {
        self.inner.load_account_delegated(address)
    }

    fn load_account_code(&mut self, address: Address) -> Option<StateLoad<Bytes>> {
        self.inner.load_account_code(address)
    }

    fn load_account_code_hash(&mut self, address: Address) -> Option<StateLoad<B256>> {
        self.inner.load_account_code_hash(address)
    }
}

#[cfg(test)]
mod tests {
    use super::CompiledNumberHost;
    use revm::{
        context_interface::{
            DummyHost,
            cfg::GasParams,
            context::{SStoreResult, SelfDestructResult, StateLoad},
            host::LoadError,
            journaled_state::AccountInfoLoad,
        },
        interpreter::Host,
        primitives::{Address, B256, Log, StorageKey, StorageValue, U256, hardfork::SpecId},
    };

    #[test]
    fn generated_ethereum_and_ring_symbols_coexist() {
        use crate::{ArbContext, ArbSpecId, DefaultArb, storage::ArbosState};
        use revm::{
            context_interface::ContextTr,
            database::EmptyDB,
            interpreter::{
                InputsImpl, Interpreter, InterpreterAction, SharedMemory, interpreter::ExtBytecode,
            },
            primitives::Bytes,
            state::Bytecode,
        };
        use revmc::{BlockHashSemantics, EvmCompiler, EvmLlvmBackend};
        let spec = ArbSpecId::NITRO.into_eth_spec();
        let mut eth = EvmCompiler::new_llvm(false).unwrap();
        let mut arb = EvmCompiler::new_with_block_hash_semantics(
            EvmLlvmBackend::new(false).unwrap(),
            BlockHashSemantics::ArbosL1Ring,
        );
        assert_eq!(eth.block_hash_semantics(), BlockHashSemantics::Ethereum);
        let code = [
            0x61, 0x03, 0x09, 0x40, 0x60, 0, 0x52, 0x60, 32, 0x60, 0, 0xf3,
        ];
        // SAFETY: both compiler owners remain live through every call below.
        let eth_fn = unsafe { eth.jit("ethereum_ring_coexist", &code[..], spec) }.unwrap();
        let arb_fn = unsafe { arb.jit("arbos_ring_coexist", &code[..], spec) }.unwrap();
        arb.clear_ir().unwrap();
        assert_eq!(arb.block_hash_semantics(), BlockHashSemantics::ArbosL1Ring);
        for (func, expected) in [
            (eth_fn, B256::ZERO),
            (arb_fn, B256::repeat_byte(0x42)),
            (eth_fn, B256::ZERO),
            (arb_fn, B256::repeat_byte(0x42)),
        ] {
            let mut ctx: ArbContext<EmptyDB> = ArbContext::arb();
            ctx.block.number = U256::from(1000);
            let hashes = &ArbosState::open().block_hashes;
            hashes.set_l1_block_number(778, ctx.journal_mut()).unwrap();
            hashes
                .set_block_hash(777, B256::repeat_byte(0x42), ctx.journal_mut())
                .unwrap();
            let mut interpreter = Interpreter::new(
                SharedMemory::new(),
                ExtBytecode::new(Bytecode::new_raw(Bytes::copy_from_slice(&code))),
                InputsImpl::default(),
                false,
                spec,
                100_000,
            );
            let mut host = CompiledNumberHost::new_arb_ring(&mut ctx, 777);
            // SAFETY: valid interpreter/host and the matching compiler owners are alive.
            let action = unsafe { func.call_with_interpreter(&mut interpreter, &mut host) };
            let InterpreterAction::Return(result) = action else {
                panic!("unexpected {action:?}")
            };
            assert_eq!(result.output.as_ref(), expected.as_slice());
            assert_eq!(ctx.block.number, U256::from(1000));
        }
    }

    #[derive(Debug)]
    struct ReadFailure;
    impl std::fmt::Display for ReadFailure {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("test read failure")
        }
    }
    impl std::error::Error for ReadFailure {}
    impl revm::database_interface::DBErrorMarker for ReadFailure {}
    struct FailingDb;
    impl revm::Database for FailingDb {
        type Error = ReadFailure;
        fn basic(&mut self, _: Address) -> Result<Option<revm::state::AccountInfo>, ReadFailure> {
            Err(ReadFailure)
        }
        fn code_by_hash(&mut self, _: B256) -> Result<revm::state::Bytecode, ReadFailure> {
            Err(ReadFailure)
        }
        fn storage(&mut self, _: Address, _: U256) -> Result<U256, ReadFailure> {
            Err(ReadFailure)
        }
        fn block_hash(&mut self, _: u64) -> Result<B256, ReadFailure> {
            panic!("must not call L2 database block_hash")
        }
    }

    #[test]
    fn ring_adapter_maps_journal_failure_to_some_zero() {
        use crate::{ArbContext, DefaultArb};
        let mut ctx = ArbContext::arb().with_db(FailingDb);
        let mut host = CompiledNumberHost::new_arb_ring(&mut ctx, 777);
        assert_eq!(host.block_hash(777), Some(B256::ZERO));
        assert_eq!(host.block_hash(u64::MAX), Some(B256::ZERO));
    }

    /// Inner Host that overrides defaulted `sstore` so forwarding is observable.
    struct CountingHost {
        dummy: DummyHost,
        sstore: u32,
        sstore_skip: u32,
        block_number: U256,
    }

    impl CountingHost {
        fn new() -> Self {
            Self {
                dummy: DummyHost::new(SpecId::PRAGUE),
                sstore: 0,
                sstore_skip: 0,
                block_number: U256::from(1000_u64),
            }
        }
    }

    impl Host for CountingHost {
        fn basefee(&self) -> U256 {
            self.dummy.basefee()
        }
        fn blob_gasprice(&self) -> U256 {
            self.dummy.blob_gasprice()
        }
        fn gas_limit(&self) -> U256 {
            self.dummy.gas_limit()
        }
        fn gas_params(&self) -> &GasParams {
            self.dummy.gas_params()
        }
        fn is_amsterdam_eip8037_enabled(&self) -> bool {
            self.dummy.is_amsterdam_eip8037_enabled()
        }
        fn difficulty(&self) -> U256 {
            self.dummy.difficulty()
        }
        fn prevrandao(&self) -> Option<U256> {
            self.dummy.prevrandao()
        }
        fn block_number(&self) -> U256 {
            self.block_number
        }
        fn timestamp(&self) -> U256 {
            self.dummy.timestamp()
        }
        fn beneficiary(&self) -> Address {
            self.dummy.beneficiary()
        }
        fn slot_num(&self) -> U256 {
            self.dummy.slot_num()
        }
        fn chain_id(&self) -> U256 {
            self.dummy.chain_id()
        }
        fn effective_gas_price(&self) -> U256 {
            self.dummy.effective_gas_price()
        }
        fn caller(&self) -> Address {
            self.dummy.caller()
        }
        fn blob_hash(&self, number: usize) -> Option<U256> {
            self.dummy.blob_hash(number)
        }
        fn max_initcode_size(&self) -> usize {
            self.dummy.max_initcode_size()
        }
        fn block_hash(&mut self, number: u64) -> Option<B256> {
            self.dummy.block_hash(number)
        }
        fn selfdestruct(
            &mut self,
            address: Address,
            target: Address,
            skip_cold_load: bool,
        ) -> Result<StateLoad<SelfDestructResult>, LoadError> {
            self.dummy.selfdestruct(address, target, skip_cold_load)
        }
        fn log(&mut self, log: Log) {
            self.dummy.log(log)
        }
        fn tstore(&mut self, address: Address, key: StorageKey, value: StorageValue) {
            self.dummy.tstore(address, key, value)
        }
        fn tload(&mut self, address: Address, key: StorageKey) -> StorageValue {
            self.dummy.tload(address, key)
        }
        fn sstore_skip_cold_load(
            &mut self,
            address: Address,
            key: StorageKey,
            value: StorageValue,
            skip_cold_load: bool,
        ) -> Result<StateLoad<SStoreResult>, LoadError> {
            self.sstore_skip += 1;
            self.dummy
                .sstore_skip_cold_load(address, key, value, skip_cold_load)
        }
        fn sstore(
            &mut self,
            _address: Address,
            _key: StorageKey,
            _value: StorageValue,
        ) -> Option<StateLoad<SStoreResult>> {
            self.sstore += 1;
            Some(StateLoad::default())
        }
        fn sload_skip_cold_load(
            &mut self,
            address: Address,
            key: StorageKey,
            skip_cold_load: bool,
        ) -> Result<StateLoad<StorageValue>, LoadError> {
            self.dummy
                .sload_skip_cold_load(address, key, skip_cold_load)
        }
        fn load_account_info_skip_cold_load(
            &mut self,
            address: Address,
            load_code: bool,
            skip_cold_load: bool,
        ) -> Result<AccountInfoLoad<'_>, LoadError> {
            self.dummy
                .load_account_info_skip_cold_load(address, load_code, skip_cold_load)
        }
    }

    #[test]
    fn adapter_number_is_l1_and_does_not_write_inner_block_number() {
        let mut inner = CountingHost::new();
        assert_eq!(inner.block_number(), U256::from(1000_u64));
        {
            let adapter = CompiledNumberHost::new(&mut inner, 777);
            assert_eq!(adapter.block_number(), U256::from(777_u64));
            assert_eq!(adapter.timestamp(), U256::ZERO);
        }
        assert_eq!(inner.block_number(), U256::from(1000_u64));
    }

    #[test]
    fn adapter_forwards_defaulted_sstore_to_inner_override() {
        let mut inner = CountingHost::new();
        {
            let mut adapter = CompiledNumberHost::new(&mut inner, 777);
            let _ = adapter.sstore(Address::ZERO, U256::ZERO, U256::from(1));
        }
        assert_eq!(inner.sstore, 1, "must call inner sstore override");
        assert_eq!(
            inner.sstore_skip, 0,
            "must not fall through to the trait default (sstore_skip_cold_load)"
        );
    }
}
