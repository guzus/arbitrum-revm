/// L1 poster-cost computation for Arbitrum transactions.
///
/// Mirrors Nitro's `L1PricingState.GetPosterInfo` / `getPosterUnitsWithoutCache`
/// in arbos/l1pricing/l1pricing.go.
///
/// Summary of the Nitro algorithm:
///   1. Only transactions where `block.Coinbase == BatchPosterAddress` incur L1 data costs.
///   2. The tx bytes are RLP-encoded (MarshalBinary) and brotli-compressed at the chain's
///      configured compression level.
///   3. `calldataUnits = 16 * len(compressed)` (TxDataNonZeroGasEIP2028 multiplier).
///   4. `posterCost = pricePerUnit * calldataUnits`.
///   5. `posterGas  = posterCost / gasPrice` (round down).
///   6. `posterFee  = gasPrice * posterGas`.
use revm::{
    context_interface::Transaction,
    primitives::{Bytes, TxKind, U256},
};

use crate::constants::{
    ARBITRUM_DEPOSIT_TX_TYPE, ARBITRUM_INTERNAL_TX_TYPE, ARBITRUM_RETRY_TX_TYPE,
    ARBITRUM_SUBMIT_RETRYABLE_TX_TYPE, BATCH_POSTER_ADDRESS,
};
use crate::transaction::ArbTxTr;

/// Arbitrum-specific tx types do not contribute to L1 poster costs.
///
/// Mirrors `TxTypeHasPosterCosts` in nitro/arbos/util/util.go.
#[inline]
fn tx_type_has_poster_costs(tx_type: u8) -> bool {
    !matches!(
        tx_type,
        ARBITRUM_INTERNAL_TX_TYPE
            | ARBITRUM_DEPOSIT_TX_TYPE
            | ARBITRUM_SUBMIT_RETRYABLE_TX_TYPE
            | ARBITRUM_RETRY_TX_TYPE
    )
}

/// Fallback encoder used only when canonical tx bytes are unavailable.
///
/// Mirrors `tx.MarshalBinary()` in Nitro: produces the EIP-2718 typed envelope
/// (or RLP-encoded legacy bytes) without requiring the actual ECDSA signature.
/// Signature bytes are zeroed, they contribute only ~65 bytes of overhead that
/// compresses away well at any quality level.
///
/// Returns an empty `Vec` for Arbitrum-specific tx types that carry no L1 cost.
fn encode_tx_for_l1_cost<T: Transaction>(tx: &T) -> Vec<u8> {
    let tx_type = tx.tx_type();
    if !tx_type_has_poster_costs(tx_type) {
        return vec![];
    }

    // We build a minimal byte representation that captures the dominant
    // variable-size fields (especially `data`) so brotli produces an accurate
    // compressed length.  The fixed-overhead fields (nonce, fees, value) are
    // encoded as variable-length big-endian integers; signature is all zeros.
    let data = tx.input().to_vec();
    let to_bytes_owned: Vec<u8> = match tx.kind() {
        TxKind::Call(addr) => addr.as_slice().to_vec(),
        TxKind::Create => vec![],
    };
    let value_raw = tx.value().to_be_bytes::<32>();
    let gas_price_raw = tx.gas_price().to_be_bytes();
    let gas_limit_raw = tx.gas_limit().to_be_bytes();
    let nonce_raw = tx.nonce().to_be_bytes();

    let value_bytes = strip_leading_zeros(&value_raw);
    let gas_price_bytes = strip_leading_zeros(&gas_price_raw);
    let gas_limit_bytes = strip_leading_zeros(&gas_limit_raw);
    let nonce_bytes = strip_leading_zeros(&nonce_raw);

    match tx_type {
        // EIP-1559 (type 2): 0x02 prefix + RLP fields
        2 => {
            let max_priority_raw = tx.max_priority_fee_per_gas().unwrap_or(0).to_be_bytes();
            let max_fee_raw = tx.max_fee_per_gas().to_be_bytes();
            let chain_id_raw = tx.chain_id().unwrap_or(0_u64).to_be_bytes();
            let max_priority_bytes = strip_leading_zeros(&max_priority_raw);
            let max_fee_bytes = strip_leading_zeros(&max_fee_raw);
            let chain_id_bytes = strip_leading_zeros(&chain_id_raw);

            let mut out = vec![0x02_u8]; // EIP-2718 type prefix
            rlp_encode_list(
                &mut out,
                &[
                    chain_id_bytes,
                    nonce_bytes,
                    max_priority_bytes,
                    max_fee_bytes,
                    gas_limit_bytes,
                    &to_bytes_owned,
                    value_bytes,
                    &data,
                    &[],         // access_list (empty → 0xc0 list)
                    &[0_u8],     // sig_y_parity
                    &[0_u8; 32], // sig_r
                    &[0_u8; 32], // sig_s
                ],
            );
            out
        }
        // EIP-2930 (type 1): 0x01 prefix + RLP fields
        1 => {
            let chain_id_raw = tx.chain_id().unwrap_or(0_u64).to_be_bytes();
            let chain_id_bytes = strip_leading_zeros(&chain_id_raw);

            let mut out = vec![0x01_u8];
            rlp_encode_list(
                &mut out,
                &[
                    chain_id_bytes,
                    nonce_bytes,
                    gas_price_bytes,
                    gas_limit_bytes,
                    &to_bytes_owned,
                    value_bytes,
                    &data,
                    &[],         // access_list
                    &[0_u8],     // sig_y_parity
                    &[0_u8; 32], // sig_r
                    &[0_u8; 32], // sig_s
                ],
            );
            out
        }
        // Legacy (type 0): plain RLP list
        _ => {
            let mut out = vec![];
            rlp_encode_list(
                &mut out,
                &[
                    nonce_bytes,
                    gas_price_bytes,
                    gas_limit_bytes,
                    &to_bytes_owned,
                    value_bytes,
                    &data,
                    &[0_u8],     // v
                    &[0_u8; 32], // r
                    &[0_u8; 32], // s
                ],
            );
            out
        }
    }
}

/// Minimal RLP list encoder: `rlp_list_header(payload_len) ++ items...`
fn rlp_encode_list(out: &mut Vec<u8>, items: &[&[u8]]) {
    // Compute payload: each item is rlp_encode_bytes(item)
    let payload: Vec<u8> = items
        .iter()
        .flat_map(|item| rlp_encode_bytes(item))
        .collect();
    // Write list header
    rlp_write_length(out, payload.len(), 0xC0);
    out.extend_from_slice(&payload);
}

/// RLP-encode a byte string: single byte if < 0x80, 0x80+len prefix otherwise.
fn rlp_encode_bytes(bytes: &[u8]) -> Vec<u8> {
    if bytes.len() == 1 && bytes[0] < 0x80 {
        return vec![bytes[0]];
    }
    if bytes.is_empty() {
        return vec![0x80]; // empty string
    }
    let mut out = vec![];
    rlp_write_length(&mut out, bytes.len(), 0x80);
    out.extend_from_slice(bytes);
    out
}

fn rlp_write_length(out: &mut Vec<u8>, len: usize, offset: u8) {
    if len < 56 {
        out.push(offset + len as u8);
    } else {
        let len_bytes = len.to_be_bytes();
        let len_bytes = strip_leading_zeros(&len_bytes);
        out.push(offset + 55 + len_bytes.len() as u8);
        out.extend_from_slice(len_bytes);
    }
}

fn strip_leading_zeros(bytes: &[u8]) -> &[u8] {
    let first_nonzero = bytes.iter().position(|&b| b != 0).unwrap_or(bytes.len());
    &bytes[first_nonzero..]
}

/// Result of the poster-cost computation.
#[allow(dead_code)]
pub struct PosterInfo {
    /// Wei cost of posting this tx's calldata on L1.
    pub poster_cost: U256,
    /// Calldata units used (= 16 * compressed_len).
    pub calldata_units: u64,
    /// L2-gas equivalent of the poster cost (`poster_cost / gas_price`, rounded down).
    pub poster_gas: u64,
    /// Actual L1 fee charged (`gas_price * poster_gas`).
    pub poster_fee: U256,
}

/// Encodes a transaction into bytes for brotli compression, for L1 cost purposes.
///
/// For parity, this first uses canonical EIP-2718 bytes when present.
/// Returns an empty `Vec` when the tx type has no poster costs.
pub fn encode_tx_bytes<T: ArbTxTr>(tx: &T) -> Vec<u8> {
    if !tx_type_has_poster_costs(tx.tx_type()) {
        return Vec::new();
    }

    if let Some(encoded) = tx.encoded_2718_bytes() {
        return encoded.to_vec();
    }

    encode_tx_for_l1_cost(tx)
}

/// Execution-owned bytes without copying an already shared canonical encoding.
/// The existing borrowed API and exact fallback encoder remain authoritative for
/// custom transaction implementations without the optional shared accessor.
/// Protocol transactions stay empty even if they carry an encoded envelope.
pub(crate) fn poster_tx_bytes<T: ArbTxTr>(tx: &T) -> Bytes {
    if !tx_type_has_poster_costs(tx.tx_type()) {
        return Bytes::new();
    }
    tx.encoded_2718_shared()
        .unwrap_or_else(|| Bytes::from(encode_tx_bytes(tx)))
}

/// Immutable, opaque compression result prepared independently of EVM state.
///
/// Only successful compression is retained. It owns the exact encoded input;
/// consuming it still checks bytes and the actual requested compression settings.
/// No fee, price, transaction index, journal state or process-global cache is stored.
pub struct PreparedPosterCompression {
    tx_bytes: Box<[u8]>,
    brotli_level: u32,
    window_size: u32,
    dictionary: brotli::Dictionary,
    compressed_len: u64,
    hits: std::sync::atomic::AtomicU64,
}

impl core::fmt::Debug for PreparedPosterCompression {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("PreparedPosterCompression")
            .field("encoded_len", &self.tx_bytes.len())
            .field("brotli_level", &self.brotli_level)
            .field("window_size", &self.window_size)
            .field("dictionary", &self.dictionary)
            .field("compressed_len", &self.compressed_len)
            .finish_non_exhaustive()
    }
}

impl PreparedPosterCompression {
    /// Prepare canonical bytes returned by `encode_tx_bytes` (or its exact fallback).
    /// Workers should skip empty/system transactions and bound retained input bytes.
    /// Compression failures are returned, never memoized as successful fallback lengths.
    pub fn prepare(
        tx_bytes: &[u8],
        brotli_level: u32,
        window_size: u32,
        dictionary: brotli::Dictionary,
    ) -> Result<Self, brotli::BrotliStatus> {
        let compressed_len =
            brotli::compress(tx_bytes, brotli_level, window_size, dictionary)?.len() as u64;
        Ok(Self {
            tx_bytes: tx_bytes.into(),
            brotli_level,
            window_size,
            dictionary,
            compressed_len,
            hits: std::sync::atomic::AtomicU64::new(0),
        })
    }

    /// Successful exact-match consumptions. Diagnostic only; never consulted for fees.
    pub fn hit_count(&self) -> u64 {
        self.hits.load(std::sync::atomic::Ordering::Relaxed)
    }

    fn compressed_len_for(&self, tx_bytes: &[u8], brotli_level: u32) -> Option<u64> {
        (self.brotli_level == brotli_level
            && self.window_size == brotli::DEFAULT_WINDOW_SIZE
            && self.dictionary == brotli::Dictionary::Empty
            && self.tx_bytes.as_ref() == tx_bytes)
            .then(|| {
                self.hits.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                self.compressed_len
            })
    }
}

/// Computes L1 poster cost information from pre-encoded transaction bytes.
///
/// `tx_bytes`, result of `encode_tx_bytes(tx)` (empty → no cost).
/// `coinbase`, block beneficiary; costs only charged when == BATCH_POSTER_ADDRESS.
/// `price_per_unit`, current L1 price per calldata unit in wei.
/// `gas_price`, effective gas price of the tx in wei.
/// `brotli_level`, ArbOS brotli compression level.
pub fn compute_poster_info(
    tx_bytes: &[u8],
    coinbase: revm::primitives::Address,
    price_per_unit: U256,
    gas_price: U256,
    brotli_level: u32,
) -> PosterInfo {
    compute_poster_info_with_prepared(
        tx_bytes,
        coinbase,
        price_per_unit,
        gas_price,
        brotli_level,
        None,
    )
}

/// Opt-in poster-cost calculation using a prepared compression result when exact.
///
/// Call after reading the current compression level and fee inputs from serial
/// execution state. Missing/mismatched preparation takes the existing synchronous
/// compression path. This function does not schedule work or mutate execution state.
pub fn compute_poster_info_with_prepared(
    tx_bytes: &[u8],
    coinbase: revm::primitives::Address,
    price_per_unit: U256,
    gas_price: U256,
    brotli_level: u32,
    prepared: Option<&PreparedPosterCompression>,
) -> PosterInfo {
    let zero = PosterInfo {
        poster_cost: U256::ZERO,
        calldata_units: 0,
        poster_gas: 0,
        poster_fee: U256::ZERO,
    };

    // Only batch poster blocks incur L1 data costs.
    if coinbase != BATCH_POSTER_ADDRESS || tx_bytes.is_empty() {
        return zero;
    }

    let compressed_len = prepared
        .and_then(|item| item.compressed_len_for(tx_bytes, brotli_level))
        .unwrap_or_else(|| compressed_len_or_fallback(tx_bytes, brotli_level));
    poster_info_from_compressed_len(compressed_len, price_per_unit, gas_price)
}

fn compressed_len_or_fallback(tx_bytes: &[u8], brotli_level: u32) -> u64 {
    match brotli::compress(
        tx_bytes,
        brotli_level,
        brotli::DEFAULT_WINDOW_SIZE,
        brotli::Dictionary::Empty,
    ) {
        Ok(value) => value.len() as u64,
        // Preserve the existing fallback (Nitro panics here).
        Err(_) => tx_bytes.len() as u64,
    }
}

fn poster_info_from_compressed_len(
    compressed_len: u64,
    price_per_unit: U256,
    gas_price: U256,
) -> PosterInfo {
    // calldataUnits = TxDataNonZeroGasEIP2028 * compressed_len = 16 * compressed_len
    const TX_DATA_NON_ZERO_GAS: u64 = 16;
    let calldata_units = TX_DATA_NON_ZERO_GAS.saturating_mul(compressed_len);

    // posterCost = pricePerUnit * calldataUnits
    let poster_cost = price_per_unit.saturating_mul(U256::from(calldata_units));

    // posterGas = posterCost / gasPrice  (round down; 0 if gasPrice == 0)
    let poster_gas = if gas_price.is_zero() {
        0_u64
    } else {
        u64::try_from(poster_cost / gas_price).unwrap_or(u64::MAX)
    };

    // posterFee = gasPrice * posterGas  (re-multiply to round down consistently)
    let poster_fee = gas_price.saturating_mul(U256::from(poster_gas));

    PosterInfo {
        poster_cost,
        calldata_units,
        poster_gas,
        poster_fee,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::constants::{
        ARBITRUM_DEPOSIT_TX_TYPE, ARBITRUM_INTERNAL_TX_TYPE, ARBITRUM_RETRY_TX_TYPE,
        ARBITRUM_SUBMIT_RETRYABLE_TX_TYPE,
    };

    use crate::ArbTransaction;
    use revm::context::TxEnv;

    // A downstream-style implementation retaining only the original borrowed API.
    // Transaction itself already implements forwarding for references.
    impl ArbTxTr for &ArbTransaction<TxEnv> {
        fn encoded_2718_bytes(&self) -> Option<&[u8]> {
            self.encoded_2718.as_ref().map(|bytes| bytes.as_ref())
        }
    }

    #[test]
    fn shared_canonical_bytes_retain_storage_and_survive_tx_replacement() {
        let encoded = Bytes::from(vec![0x42; 512]);
        let original_ptr = encoded.as_ptr();
        let mut tx = ArbTransaction::new(TxEnv::default()).with_encoded_2718(encoded);
        let shared = poster_tx_bytes(&tx);
        assert_eq!(shared.as_ptr(), original_ptr);
        assert_eq!(shared.as_ref(), encode_tx_bytes(&tx));
        tx.encoded_2718 = Some(Bytes::from(vec![0x99; 513]));
        let replacement = poster_tx_bytes(&tx);
        assert_eq!(replacement.as_ref(), encode_tx_bytes(&tx));
        assert_ne!(replacement.as_ptr(), shared.as_ptr());
        drop(tx);
        assert_eq!(shared.as_ref(), &[0x42; 512]);
    }

    #[test]
    fn shared_bytes_preserve_borrowed_custom_fallback_and_protocol_semantics() {
        for tx_type in [
            0,
            1,
            2,
            3,
            4,
            ARBITRUM_INTERNAL_TX_TYPE,
            ARBITRUM_DEPOSIT_TX_TYPE,
            ARBITRUM_SUBMIT_RETRYABLE_TX_TYPE,
            ARBITRUM_RETRY_TX_TYPE,
        ] {
            for encoded in [None, Some(Bytes::new()), Some(Bytes::from(vec![0x21; 256]))] {
                let mut tx = ArbTransaction::new(TxEnv::default());
                tx.base.tx_type = tx_type;
                tx.encoded_2718 = encoded;
                let old = encode_tx_bytes(&tx);
                let shared = poster_tx_bytes(&tx);
                let custom = &tx;
                // Explicit UFCS selects the custom reference implementation's default.
                assert!(
                    <&ArbTransaction<TxEnv> as ArbTxTr>::encoded_2718_shared(&custom).is_none()
                );
                let fallback = poster_tx_bytes(&custom);
                assert_eq!(shared.as_ref(), old);
                assert_eq!(fallback.as_ref(), old);
                if !tx_type_has_poster_costs(tx_type) {
                    assert!(shared.is_empty());
                }
                for level in [0, 1, 4] {
                    for (price, gas) in [
                        (U256::ZERO, U256::ZERO),
                        (U256::from(11), U256::from(3)),
                        (U256::MAX, U256::ONE),
                    ] {
                        for coinbase in [BATCH_POSTER_ADDRESS, revm::primitives::Address::ZERO] {
                            assert_same(
                                compute_poster_info(&old, coinbase, price, gas, level),
                                compute_poster_info(&shared, coinbase, price, gas, level),
                            );
                            assert_same(
                                compute_poster_info(&old, coinbase, price, gas, level),
                                compute_poster_info(&fallback, coinbase, price, gas, level),
                            );
                        }
                    }
                }
            }
        }
    }

    fn prepare(bytes: &[u8], level: u32) -> PreparedPosterCompression {
        PreparedPosterCompression::prepare(
            bytes,
            level,
            brotli::DEFAULT_WINDOW_SIZE,
            brotli::Dictionary::Empty,
        )
        .unwrap()
    }

    fn assert_same(left: PosterInfo, right: PosterInfo) {
        assert_eq!(left.calldata_units, right.calldata_units);
        assert_eq!(left.poster_cost, right.poster_cost);
        assert_eq!(left.poster_gas, right.poster_gas);
        assert_eq!(left.poster_fee, right.poster_fee);
    }

    #[test]
    fn exact_preparation_matches_direct_compression_and_current_fees() {
        for level in [0, 1, 4] {
            let bytes = vec![42; 512];
            let prepared = prepare(&bytes, level);
            let direct = brotli::compress(
                &bytes,
                level,
                brotli::DEFAULT_WINDOW_SIZE,
                brotli::Dictionary::Empty,
            )
            .unwrap();
            assert_eq!(
                prepared.compressed_len_for(&bytes, level),
                Some(direct.len() as u64)
            );
            for (price, gas) in [
                (U256::from(100), U256::from(3)),
                (U256::from(7), U256::from(2)),
                (U256::MAX, U256::from(1)),
            ] {
                let info = compute_poster_info_with_prepared(
                    &bytes,
                    BATCH_POSTER_ADDRESS,
                    price,
                    gas,
                    level,
                    Some(&prepared),
                );
                assert_eq!(info.calldata_units, 16 * direct.len() as u64);
                assert_eq!(
                    info.poster_cost,
                    price.saturating_mul(U256::from(info.calldata_units))
                );
                assert_eq!(
                    info.poster_gas,
                    u64::try_from(info.poster_cost / gas).unwrap_or(u64::MAX)
                );
                assert_same(
                    info,
                    compute_poster_info(&bytes, BATCH_POSTER_ADDRESS, price, gas, level),
                );
            }
        }
    }

    #[test]
    fn changed_level_and_changed_bytes_fall_back() {
        let mut bytes = vec![42; 512];
        let prepared = prepare(&bytes, 0);
        assert!(prepared.compressed_len_for(&bytes, 1).is_none());
        assert_same(
            compute_poster_info_with_prepared(
                &bytes,
                BATCH_POSTER_ADDRESS,
                U256::from(7),
                U256::from(3),
                1,
                Some(&prepared),
            ),
            compute_poster_info(
                &bytes,
                BATCH_POSTER_ADDRESS,
                U256::from(7),
                U256::from(3),
                1,
            ),
        );
        bytes[0] = 99;
        assert!(prepared.compressed_len_for(&bytes, 0).is_none());
        assert_same(
            compute_poster_info_with_prepared(
                &bytes,
                BATCH_POSTER_ADDRESS,
                U256::from(7),
                U256::from(3),
                0,
                Some(&prepared),
            ),
            compute_poster_info(
                &bytes,
                BATCH_POSTER_ADDRESS,
                U256::from(7),
                U256::from(3),
                0,
            ),
        );
        bytes.push(0);
        assert!(prepared.compressed_len_for(&bytes, 0).is_none());
    }

    #[test]
    fn window_and_dictionary_are_part_of_the_key() {
        let bytes = b"poster compression input";
        let different_window = PreparedPosterCompression::prepare(
            bytes,
            1,
            brotli::DEFAULT_WINDOW_SIZE - 1,
            brotli::Dictionary::Empty,
        )
        .unwrap();
        assert!(different_window.compressed_len_for(bytes, 1).is_none());
        let different_dictionary = PreparedPosterCompression::prepare(
            bytes,
            11,
            brotli::DEFAULT_WINDOW_SIZE,
            brotli::Dictionary::StylusProgram,
        )
        .unwrap();
        assert!(different_dictionary.compressed_len_for(bytes, 11).is_none());
        for (prepared, level) in [(&different_window, 1), (&different_dictionary, 11)] {
            assert_same(
                compute_poster_info_with_prepared(
                    bytes,
                    BATCH_POSTER_ADDRESS,
                    U256::from(7),
                    U256::from(3),
                    level,
                    Some(prepared),
                ),
                compute_poster_info(
                    bytes,
                    BATCH_POSTER_ADDRESS,
                    U256::from(7),
                    U256::from(3),
                    level,
                ),
            );
        }
    }

    #[test]
    fn preparation_errors_are_not_successful_memo_entries() {
        // Vendored Brotli explicitly rejects the Stylus dictionary below level 11.
        assert!(
            PreparedPosterCompression::prepare(
                b"input",
                1,
                brotli::DEFAULT_WINDOW_SIZE,
                brotli::Dictionary::StylusProgram
            )
            .is_err()
        );
        assert_same(
            compute_poster_info_with_prepared(
                b"input",
                BATCH_POSTER_ADDRESS,
                U256::from(7),
                U256::from(3),
                1,
                None,
            ),
            compute_poster_info(
                b"input",
                BATCH_POSTER_ADDRESS,
                U256::from(7),
                U256::from(3),
                1,
            ),
        );
    }

    #[test]
    fn empty_and_nonposter_inputs_still_have_zero_cost() {
        let prepared = prepare(b"input", 1);
        for (bytes, coinbase) in [
            (b"".as_slice(), BATCH_POSTER_ADDRESS),
            (b"input".as_slice(), revm::primitives::Address::ZERO),
        ] {
            let info = compute_poster_info_with_prepared(
                bytes,
                coinbase,
                U256::from(7),
                U256::from(3),
                1,
                Some(&prepared),
            );
            assert_eq!(info.calldata_units, 0);
            assert_eq!(info.poster_cost, U256::ZERO);
            assert_eq!(info.poster_gas, 0);
            assert_eq!(info.poster_fee, U256::ZERO);
        }
    }

    #[test]
    fn zero_prices_preserve_units_and_fee_edge_cases() {
        let bytes = b"input";
        let prepared = prepare(bytes, 1);
        let zero_price = compute_poster_info_with_prepared(
            bytes,
            BATCH_POSTER_ADDRESS,
            U256::ZERO,
            U256::from(3),
            1,
            Some(&prepared),
        );
        assert!(zero_price.calldata_units > 0);
        assert_eq!(zero_price.poster_cost, U256::ZERO);
        let zero_gas = compute_poster_info_with_prepared(
            bytes,
            BATCH_POSTER_ADDRESS,
            U256::from(7),
            U256::ZERO,
            1,
            Some(&prepared),
        );
        assert_eq!(zero_gas.calldata_units, zero_price.calldata_units);
        assert!(zero_gas.poster_cost > U256::ZERO);
        assert_eq!(zero_gas.poster_gas, 0);
        assert_eq!(zero_gas.poster_fee, U256::ZERO);
    }

    #[test]
    fn canonical_fallback_and_system_bytes_use_existing_encoder() {
        use crate::transaction::ArbTransaction;
        use revm::context::TxEnv;
        let mut tx = ArbTransaction::new(TxEnv::default());
        let fallback = encode_tx_bytes(&tx);
        assert!(!fallback.is_empty());
        let prepared = prepare(&fallback, 1);
        tx.encoded_2718 = Some(vec![1, 2, 3, 4].into());
        let canonical = encode_tx_bytes(&tx);
        assert_eq!(canonical, [1, 2, 3, 4]);
        assert!(prepared.compressed_len_for(&canonical, 1).is_none());
        for ty in [
            ARBITRUM_INTERNAL_TX_TYPE,
            ARBITRUM_DEPOSIT_TX_TYPE,
            ARBITRUM_SUBMIT_RETRYABLE_TX_TYPE,
            ARBITRUM_RETRY_TX_TYPE,
        ] {
            tx.base.tx_type = ty;
            let bytes = encode_tx_bytes(&tx);
            assert!(bytes.is_empty());
            assert_eq!(
                compute_poster_info_with_prepared(
                    &bytes,
                    BATCH_POSTER_ADDRESS,
                    U256::from(7),
                    U256::from(3),
                    1,
                    Some(&prepared)
                )
                .calldata_units,
                0
            );
        }
    }

    #[test]
    fn protocol_transactions_never_have_l1_poster_costs() {
        for tx_type in [
            ARBITRUM_INTERNAL_TX_TYPE,
            ARBITRUM_DEPOSIT_TX_TYPE,
            ARBITRUM_SUBMIT_RETRYABLE_TX_TYPE,
            ARBITRUM_RETRY_TX_TYPE,
        ] {
            assert!(!tx_type_has_poster_costs(tx_type));
        }
        for tx_type in [0_u8, 1, 2, 3, 4] {
            assert!(tx_type_has_poster_costs(tx_type));
        }
    }
}
