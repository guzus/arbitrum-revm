use crate::l1_cost::PreparedPosterCompression;
use alloy_consensus::transaction::Transaction as AlloyTransaction;
use alloy_eips::eip2718::{Encodable2718, Typed2718};
use arbitrum_alloy_consensus::transactions::ArbTxEnvelope;
use revm::context_interface::{either::Either, transaction::AccessList};
use revm::{
    context::{
        TxEnv,
        tx::{TxEnvBuildError, TxEnvBuilder},
    },
    context_interface::Transaction,
    handler::SystemCallTx,
    primitives::{Address, B256, Bytes, TxKind, U256},
};
use std::sync::Arc;

/// Retry-transaction metadata that is not representable in revm's base `TxEnv`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RetryTxMeta {
    /// Retryable ticket id being redeemed.
    pub ticket_id: B256,
    /// Retry refund recipient.
    pub refund_to: Address,
    /// Maximum refundable amount.
    pub max_refund: U256,
    /// Submission-fee refund component.
    pub submission_fee_refund: U256,
}

/// Error raised when an RPC/consensus transaction cannot be lowered into a revm
/// [`TxEnv`]. The only fallible step is sender recovery, which fails for malformed
/// signatures or envelopes that do not carry a recoverable sender.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TxConversionError(String);

impl TxConversionError {
    fn sender(err: impl core::fmt::Display) -> Self {
        Self(format!("failed to recover transaction sender: {err}"))
    }
}

impl core::fmt::Display for TxConversionError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(&self.0)
    }
}

impl core::error::Error for TxConversionError {}

/// Lowers an Arbitrum consensus transaction envelope into the revm transaction
/// wrapper. This is the canonical adapter from "a tx as received over RPC / decoded
/// from a sequencer feed" to "a tx the EVM can execute".
///
/// `arbitrum_alloy_rpc_types::ArbTransaction` and the sequencer feed envelopes all expose
/// their inner [`ArbTxEnvelope`] via [`AsRef`], so callers can write
/// `ArbTransaction::try_from(rpc_tx.as_ref())`.
impl TryFrom<&ArbTxEnvelope> for ArbTransaction<TxEnv> {
    type Error = TxConversionError;

    fn try_from(tx: &ArbTxEnvelope) -> Result<Self, Self::Error> {
        let access_list = tx
            .access_list()
            .map(|items| AccessList(items.0.clone()))
            .unwrap_or_default();
        let blob_hashes = tx
            .blob_versioned_hashes()
            .map_or_else(Vec::new, |hashes| hashes.to_vec());
        let authorization_list = tx
            .authorization_list()
            .map(|auths| auths.iter().cloned().map(Either::Left).collect())
            .unwrap_or_default();

        let base = TxEnv {
            tx_type: tx.ty(),
            caller: tx.sender().map_err(TxConversionError::sender)?,
            gas_limit: tx.gas_limit(),
            gas_price: tx.gas_price().unwrap_or(tx.max_fee_per_gas()),
            kind: tx.kind(),
            value: tx.value(),
            data: tx.input().clone(),
            nonce: tx.nonce(),
            chain_id: tx.chain_id(),
            access_list,
            gas_priority_fee: tx.max_priority_fee_per_gas(),
            blob_hashes,
            max_fee_per_blob_gas: tx.max_fee_per_blob_gas().unwrap_or(0),
            authorization_list,
        };

        let retry_meta = match tx {
            ArbTxEnvelope::Retry(retry) => Some(RetryTxMeta {
                ticket_id: retry.ticket_id,
                refund_to: retry.refund_to,
                max_refund: retry.max_refund,
                submission_fee_refund: retry.submission_fee_refund,
            }),
            _ => None,
        };

        Ok(ArbTransaction {
            base,
            retry_meta,
            tx_hash: Some(tx.hash()),
            encoded_2718: Some(Bytes::from(tx.encoded_2718())),
            prepared_poster_compression: None,
        })
    }
}

/// Converts an Arbitrum consensus transaction envelope into a revm [`TxEnv`] wrapper.
///
/// Thin wrapper around the [`TryFrom`] adapter that preserves the historical
/// `eyre::Result` signature for existing callers.
pub fn arb_envelope_to_tx_env(tx: &ArbTxEnvelope) -> eyre::Result<ArbTransaction<TxEnv>> {
    ArbTransaction::try_from(tx).map_err(|err| eyre::eyre!(err))
}

/// Arbitrum transaction trait.
pub trait ArbTxTr: Transaction {
    /// Returns retry metadata when the wrapped tx is `ArbitrumRetryTx`.
    fn retry_meta(&self) -> Option<&RetryTxMeta> {
        None
    }

    /// Returns the consensus transaction hash when available.
    fn tx_hash(&self) -> Option<B256> {
        None
    }

    /// Optional execution-only compression hint; never a source of transaction semantics.
    fn prepared_poster_compression(&self) -> Option<&Arc<PreparedPosterCompression>> {
        None
    }

    /// Optional shared ownership of the same canonical bytes returned by
    /// `encoded_2718_bytes`. Implementations must return identical bytes, never a
    /// cached encoding of a different transaction. Default preserves custom implementations.
    fn encoded_2718_shared(&self) -> Option<Bytes> {
        None
    }

    /// Returns canonical EIP-2718 transaction bytes when available.
    fn encoded_2718_bytes(&self) -> Option<&[u8]> {
        None
    }
}

/// Arbitrum transaction wrapper around a base transaction type.
#[derive(Clone, Debug)]
pub struct ArbTransaction<T: Transaction> {
    /// Base transaction fields.
    pub base: T,
    /// Arbitrum transaction extensions that are not part of base `TxEnv`.
    pub retry_meta: Option<RetryTxMeta>,
    /// Consensus transaction hash, if the transaction came from an envelope.
    pub tx_hash: Option<B256>,
    /// Canonical EIP-2718 envelope bytes for this tx, if known.
    pub encoded_2718: Option<Bytes>,
    /// Immutable optimization hint, validated against exact bytes and current settings.
    /// Excluded from semantic equality. Fresh conversions and system calls leave it empty.
    pub prepared_poster_compression: Option<Arc<PreparedPosterCompression>>,
}

impl<T: Transaction + PartialEq> PartialEq for ArbTransaction<T> {
    fn eq(&self, other: &Self) -> bool {
        self.base == other.base
            && self.retry_meta == other.retry_meta
            && self.tx_hash == other.tx_hash
            && self.encoded_2718 == other.encoded_2718
    }
}

impl<T: Transaction + Eq> Eq for ArbTransaction<T> {}

impl<T: Transaction> ArbTransaction<T> {
    /// Creates a new wrapped transaction.
    pub fn new(base: T) -> Self {
        Self {
            base,
            retry_meta: None,
            tx_hash: None,
            encoded_2718: None,
            prepared_poster_compression: None,
        }
    }

    /// Attaches retry metadata.
    pub fn with_retry_meta(mut self, retry_meta: RetryTxMeta) -> Self {
        self.retry_meta = Some(retry_meta);
        self
    }

    /// Attaches a consensus transaction hash.
    pub fn with_tx_hash(mut self, tx_hash: B256) -> Self {
        self.tx_hash = Some(tx_hash);
        self
    }

    /// Attaches canonical EIP-2718 envelope bytes.
    pub fn with_encoded_2718(mut self, encoded_2718: Bytes) -> Self {
        self.encoded_2718 = Some(encoded_2718);
        self
    }
}

impl<T: Transaction> AsRef<T> for ArbTransaction<T> {
    fn as_ref(&self) -> &T {
        &self.base
    }
}

impl ArbTransaction<TxEnv> {
    /// Creates a builder for [`ArbTransaction<TxEnv>`].
    pub fn builder() -> ArbTransactionBuilder {
        ArbTransactionBuilder::new()
    }
}

impl Default for ArbTransaction<TxEnv> {
    fn default() -> Self {
        Self {
            base: TxEnv::default(),
            retry_meta: None,
            tx_hash: None,
            encoded_2718: None,
            prepared_poster_compression: None,
        }
    }
}

impl<TX: Transaction + SystemCallTx> SystemCallTx for ArbTransaction<TX> {
    fn new_system_tx_with_caller(
        caller: Address,
        system_contract_address: Address,
        data: Bytes,
    ) -> Self {
        Self {
            base: TX::new_system_tx_with_caller(caller, system_contract_address, data),
            retry_meta: None,
            tx_hash: None,
            encoded_2718: None,
            prepared_poster_compression: None,
        }
    }
}

impl<T: Transaction> ArbTxTr for ArbTransaction<T> {
    fn encoded_2718_shared(&self) -> Option<Bytes> {
        self.encoded_2718.clone()
    }

    fn prepared_poster_compression(&self) -> Option<&Arc<PreparedPosterCompression>> {
        self.prepared_poster_compression.as_ref()
    }

    fn retry_meta(&self) -> Option<&RetryTxMeta> {
        self.retry_meta.as_ref()
    }

    fn tx_hash(&self) -> Option<B256> {
        self.tx_hash
    }

    fn encoded_2718_bytes(&self) -> Option<&[u8]> {
        self.encoded_2718.as_ref().map(|bytes| bytes.as_ref())
    }
}

impl<T: Transaction> Transaction for ArbTransaction<T> {
    type AccessListItem<'a>
        = T::AccessListItem<'a>
    where
        T: 'a;
    type Authorization<'a>
        = T::Authorization<'a>
    where
        T: 'a;

    fn tx_type(&self) -> u8 {
        self.base.tx_type()
    }

    fn caller(&self) -> Address {
        self.base.caller()
    }

    fn gas_limit(&self) -> u64 {
        self.base.gas_limit()
    }

    fn value(&self) -> U256 {
        self.base.value()
    }

    fn input(&self) -> &Bytes {
        self.base.input()
    }

    fn nonce(&self) -> u64 {
        self.base.nonce()
    }

    fn kind(&self) -> TxKind {
        self.base.kind()
    }

    fn chain_id(&self) -> Option<u64> {
        self.base.chain_id()
    }

    fn access_list(&self) -> Option<impl Iterator<Item = Self::AccessListItem<'_>>> {
        self.base.access_list()
    }

    fn max_priority_fee_per_gas(&self) -> Option<u128> {
        self.base.max_priority_fee_per_gas()
    }

    fn max_fee_per_gas(&self) -> u128 {
        self.base.max_fee_per_gas()
    }

    fn gas_price(&self) -> u128 {
        self.base.gas_price()
    }

    fn blob_versioned_hashes(&self) -> &[B256] {
        self.base.blob_versioned_hashes()
    }

    fn max_fee_per_blob_gas(&self) -> u128 {
        self.base.max_fee_per_blob_gas()
    }

    fn effective_gas_price(&self, base_fee: u128) -> u128 {
        // Arbitrum drops the priority tip unless on-chain tip collection is enabled
        // (Nitro `state_transition.go`: `if !CollectTips() && msg.GasPrice > baseFee {
        // msg.GasPrice = baseFee }`), so the EVM-visible effective price, what the
        // GASPRICE opcode returns, is the L2 base fee. Standard revm returns the nominal
        // gas price for a legacy tx (e.g. 1000 gwei), which diverges in contracts that
        // compute with `gas()`. Arbitrum One does not collect tips, so clamp to the base
        // fee. (Fee accounting already uses `chain().paid_gas_price`; tip-collecting chains
        // are not modelled here.)
        self.base.effective_gas_price(base_fee).min(base_fee)
    }

    fn authorization_list_len(&self) -> usize {
        self.base.authorization_list_len()
    }

    fn authorization_list(&self) -> impl Iterator<Item = Self::Authorization<'_>> {
        self.base.authorization_list()
    }
}

/// Builder for [`ArbTransaction<TxEnv>`].
#[derive(Default, Debug)]
pub struct ArbTransactionBuilder {
    base: TxEnvBuilder,
}

impl ArbTransactionBuilder {
    /// Creates a new builder with default values.
    pub fn new() -> Self {
        Self {
            base: TxEnvBuilder::new(),
        }
    }

    /// Sets the base transaction builder.
    pub fn base(mut self, base: TxEnvBuilder) -> Self {
        self.base = base;
        self
    }

    /// Builds with defaults for missing fields.
    pub fn build_fill(self) -> ArbTransaction<TxEnv> {
        ArbTransaction {
            base: self.base.build_fill(),
            retry_meta: None,
            tx_hash: None,
            encoded_2718: None,
            prepared_poster_compression: None,
        }
    }

    /// Builds strictly and returns validation errors from [`TxEnvBuilder`].
    pub fn build(self) -> Result<ArbTransaction<TxEnv>, TxEnvBuildError> {
        Ok(ArbTransaction {
            base: self.base.build()?,
            retry_meta: None,
            tx_hash: None,
            encoded_2718: None,
            prepared_poster_compression: None,
        })
    }
}

#[cfg(test)]
mod prepared_hint_tests {
    use super::*;

    fn hint() -> Arc<PreparedPosterCompression> {
        Arc::new(
            PreparedPosterCompression::prepare(
                b"canonical transaction bytes",
                0,
                brotli::DEFAULT_WINDOW_SIZE,
                brotli::Dictionary::Empty,
            )
            .unwrap(),
        )
    }

    #[test]
    fn prepared_hint_is_not_transaction_identity() {
        let plain = ArbTransaction::default();
        let mut prepared = plain.clone();
        let item = hint();
        prepared.prepared_poster_compression = Some(item.clone());
        assert_eq!(plain, prepared);
        assert!(Arc::ptr_eq(
            prepared.clone().prepared_poster_compression().unwrap(),
            &item
        ));
        prepared.base.nonce += 1;
        assert_ne!(plain, prepared);
    }

    #[test]
    fn fresh_transactions_and_system_calls_do_not_inherit_hints() {
        assert!(
            ArbTransaction::new(TxEnv::default())
                .prepared_poster_compression()
                .is_none()
        );
        assert!(
            ArbTransaction::default()
                .prepared_poster_compression()
                .is_none()
        );
        assert!(
            ArbTransaction::builder()
                .build_fill()
                .prepared_poster_compression()
                .is_none()
        );
        let system = ArbTransaction::<TxEnv>::new_system_tx_with_caller(
            Address::ZERO,
            Address::ZERO,
            Bytes::new(),
        );
        assert!(system.prepared_poster_compression().is_none());
    }

    #[test]
    fn handler_consumes_hint_with_identical_execution_result() {
        use crate::{ArbBuilder, ArbChainContext, ArbSpecId, encode_tx_bytes};
        use revm::context::{BlockEnv, CfgEnv};
        use revm::{Context, ExecuteEvm, MainContext};
        let tx = ArbTransaction::default();
        let item = Arc::new(
            PreparedPosterCompression::prepare(
                &encode_tx_bytes(&tx),
                0,
                brotli::DEFAULT_WINDOW_SIZE,
                brotli::Dictionary::Empty,
            )
            .unwrap(),
        );
        let run = |tx| {
            let mut evm = Context::mainnet()
                .with_tx(ArbTransaction::default())
                .with_cfg(CfgEnv::new_with_spec(ArbSpecId::NITRO))
                .with_block(BlockEnv {
                    beneficiary: crate::constants::BATCH_POSTER_ADDRESS,
                    basefee: 0,
                    ..Default::default()
                })
                .with_chain(ArbChainContext::default())
                .build_arb();
            evm.transact(tx).unwrap()
        };
        let baseline = run(tx.clone());
        let mut prepared = tx;
        prepared.prepared_poster_compression = Some(item.clone());
        assert_eq!(baseline, run(prepared));
        assert_eq!(item.hit_count(), 1);
    }

    #[test]
    fn transaction_replacement_and_system_call_clear_prior_hint() {
        use crate::{ArbBuilder, ArbChainContext, ArbSpecId};
        use revm::context::CfgEnv;
        use revm::context_interface::ContextTr;
        use revm::handler::{EvmTr, system_call::SystemCallEvm};
        use revm::{Context, ExecuteEvm, MainContext};
        let mut evm = Context::mainnet()
            .with_tx(ArbTransaction::default())
            .with_cfg(CfgEnv::new_with_spec(ArbSpecId::NITRO))
            .with_chain(ArbChainContext::default())
            .build_arb();
        let mut hinted = ArbTransaction::default();
        hinted.prepared_poster_compression = Some(hint());
        // Invalid transactions exercise replacement even when validation fails.
        hinted.base.gas_limit = 0;
        assert!(evm.transact_one(hinted.clone()).is_err());
        assert!(evm.ctx().tx().prepared_poster_compression().is_some());
        let mut next = ArbTransaction::default();
        next.base.gas_limit = 0;
        assert!(evm.transact_one(next).is_err());
        assert!(evm.ctx().tx().prepared_poster_compression().is_none());
        assert!(evm.transact_one(hinted).is_err());
        let _ = evm.system_call_one_with_caller(Address::ZERO, Address::ZERO, Bytes::new());
        assert!(evm.ctx().tx().prepared_poster_compression().is_none());
    }

    #[test]
    fn counts_only_actual_exact_consumptions() {
        use crate::constants::BATCH_POSTER_ADDRESS;
        use crate::l1_cost::compute_poster_info_with_prepared;
        let item = hint();
        let run = |bytes: &[u8], level, coinbase| {
            compute_poster_info_with_prepared(
                bytes,
                coinbase,
                U256::ZERO,
                U256::ZERO,
                level,
                Some(&item),
            );
        };
        run(b"wrong bytes", 0, BATCH_POSTER_ADDRESS);
        run(b"canonical transaction bytes", 1, BATCH_POSTER_ADDRESS);
        run(b"canonical transaction bytes", 0, Address::ZERO);
        run(b"", 0, BATCH_POSTER_ADDRESS);
        assert_eq!(item.hit_count(), 0);
        run(b"canonical transaction bytes", 0, BATCH_POSTER_ADDRESS);
        assert_eq!(item.hit_count(), 1);
    }
}
