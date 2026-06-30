use crate::service_state::ServiceState;
use alloy::consensus::Eip658Value;
use alloy::primitives::TxHash;
use alloy_rpc_types_eth::{Log, Receipt, ReceiptEnvelope, ReceiptWithBloom, TransactionReceipt};
use std::str::FromStr;

// polycli polls receipts to get the eth_sendRawTransaction status
// it logs cumulativeGasUsed and transactionHash
// return null if the transaction is not yet included onto the blockchain, return status=0 for errors
pub async fn service_get_txn_receipt(
    service: ServiceState,
    txn_hash: String,
) -> anyhow::Result<Option<TransactionReceipt<ReceiptEnvelope<Log>>>> {
    let txn_hash = TxHash::from_str(&txn_hash)?;
    let (status, block_num) = match service.store.txn_receipt(txn_hash).await? {
        Some((result, block_num)) => (result.is_ok(), block_num),
        None => {
            // Synthetic-log receipt fallback (receipts contract). A tx that emitted
            // a synthetic log — a projected BridgeEvent / ClaimEvent / GER update —
            // has NO real txn record: it was never an `eth_sendRawTransaction`; the
            // SyntheticProjector DERIVES it from a consumed Miden note. But in EVM a
            // tx that produced a log MUST have a receipt. aggkit's L2BridgeSyncer
            // calls `eth_getTransactionReceipt` for every bridge/claim log it sees,
            // and on a null receipt it fails to append the log ("input too short: 0
            // bytes") and STALLS the entire L2 sync — so no certificate is ever
            // built and bridge-outs never settle on L1. Tie the hashes together via
            // `logs_by_tx`: if this tx_hash has synthetic logs, synthesise a success
            // receipt at the log's block.
            //
            // Legacy served this implicitly — the live claim path emitted its
            // ClaimEvent under the real `eth_sendRawTransaction` hash (which already
            // had a receipt). The projector emits under a derived hash, so the proxy
            // must serve the receipt for it directly.
            let logs = service
                .store
                .get_logs_for_tx(&format!("{txn_hash:#x}"))
                .await?;
            match logs.first() {
                Some(log) => (true, log.block_number),
                None => return Ok(None),
            }
        }
    };

    let mut receipt_inner = ReceiptWithBloom::<Receipt<Log>>::default();
    receipt_inner.receipt.status = Eip658Value::Eip658(status);
    receipt_inner.receipt.cumulative_gas_used = 0;

    // IMPORTANT: Go's hexutil.Uint.UnmarshalJSON cannot handle JSON null.
    // All numeric fields must be present with valid hex values, otherwise
    // Go's types.Receipt unmarshaling fails silently and the EthTxManager
    // treats the tx as "not mined".
    let block_hash = service.block_state.get_block_hash(block_num);
    // RD-940 latent-bug co-fix (originally scoped for the Phase 3 BlockMonitor
    // PR; folded here because the BlockMonitor unification is deferred to a
    // follow-up). The pre-fix path returned `from: Default::default()` (the
    // zero address) regardless of who actually signed the tx, breaking any
    // downstream consumer that relied on `receipt.from` matching the
    // recovered signer (aggsender's receipt-trust check, audit tooling). Look
    // up the TxnData via `txn_get` to recover the real signer; the lookup is
    // cheap (same row the receipt came from). If the lookup races and
    // returns None we keep the zero-address default rather than 500-ing —
    // the cost of falling back is the prior bug, not a worse failure mode.
    let from = service
        .store
        .txn_get(txn_hash)
        .await
        .ok()
        .flatten()
        .map(|t| t.signer)
        .unwrap_or_default();
    let receipt: TransactionReceipt<ReceiptEnvelope<Log>> = TransactionReceipt {
        inner: ReceiptEnvelope::Eip1559(receipt_inner),
        transaction_hash: txn_hash,
        transaction_index: Some(0),
        block_hash: Some(alloy::primitives::B256::from(block_hash)),
        block_number: Some(block_num),
        gas_used: 0,
        effective_gas_price: 0,
        blob_gas_used: None,
        blob_gas_price: None,
        from,
        to: None,
        contract_address: None,
    };
    Ok(Some(receipt))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::TxnEntry;
    use crate::test_helpers::create_test_service;
    use alloy::consensus::TxEnvelope;
    use alloy::primitives::{Address, TxHash};

    #[tokio::test]
    async fn test_service_get_txn_receipt_not_found() {
        let service = create_test_service();
        let txn_hash = TxHash::from([1u8; 32]).to_string();
        let result = service_get_txn_receipt(service, txn_hash).await.unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn test_service_get_txn_receipt_found() {
        let service = create_test_service();
        let block_state = service.block_state.clone();
        let txn_hash = TxHash::from([2u8; 32]);
        // RD-940: use a non-zero signer so the receipt `from` fix is
        // exercised in the basic happy-path test. Pre-fix the field
        // always read back as Address::ZERO.
        let signer = Address::from([0x42u8; 20]);

        let txn_envelope = TxEnvelope::Legacy(alloy::consensus::Signed::new_unchecked(
            alloy::consensus::TxLegacy::default(),
            alloy::primitives::Signature::test_signature(),
            txn_hash,
        ));

        service
            .store
            .txn_begin(
                txn_hash,
                TxnEntry {
                    id: None,
                    envelope: txn_envelope,
                    signer,
                    expires_at: None,
                    logs: vec![],
                },
            )
            .await
            .unwrap();

        service
            .store
            .txn_commit(txn_hash, Ok(()), 123, block_state.get_block_hash(123))
            .await
            .unwrap();

        let result = service_get_txn_receipt(service, txn_hash.to_string())
            .await
            .unwrap();
        assert!(result.is_some());
        let receipt = result.unwrap();
        assert_eq!(receipt.transaction_hash, txn_hash);
        assert_eq!(receipt.block_number, Some(123));
        assert_eq!(receipt.transaction_index, Some(0));
        assert!(receipt.block_hash.is_some());
        // RD-940 latent-bug co-fix: `from` must round-trip through the
        // store, not be `Default::default()` (zero address). Pre-fix this
        // assertion would fail.
        assert_eq!(
            receipt.from, signer,
            "receipt.from must be the recovered signer, not Address::ZERO"
        );
        assert!(matches!(
            receipt.inner.as_receipt().unwrap().status,
            alloy::consensus::Eip658Value::Eip658(true)
        ));
    }

    /// RD-940 Spec D wire-contract — a tx the worker has accepted but not
    /// yet committed should surface through `eth_getTransactionReceipt` as
    /// a flat JSON `null`, NOT a stub with `status: 0x0`. aggkit's
    /// ethtxmanager treats `null` as "keep polling" and `status: 0x0` as
    /// "tx failed permanently". The store contract is that `txn_receipt`
    /// returns `None` for non-committed hashes, and `service_get_txn_receipt`
    /// then maps `None → Ok(None)` which the dispatcher serialises to
    /// JSON `null`. Test: a hash that was `txn_begin`'d but not `txn_commit`'d
    /// must return None (i.e. the wire shape will be JSON null).
    #[tokio::test]
    async fn rd940_specd_pending_receipt_is_none() {
        let service = create_test_service();
        let txn_hash = TxHash::from([0x77u8; 32]);
        let txn_envelope = TxEnvelope::Legacy(alloy::consensus::Signed::new_unchecked(
            alloy::consensus::TxLegacy::default(),
            alloy::primitives::Signature::test_signature(),
            txn_hash,
        ));
        service
            .store
            .txn_begin(
                txn_hash,
                TxnEntry {
                    id: None,
                    envelope: txn_envelope,
                    signer: Address::from([0x99u8; 20]),
                    expires_at: None,
                    logs: vec![],
                },
            )
            .await
            .unwrap();
        // Deliberately NOT committing.

        let result = service_get_txn_receipt(service, txn_hash.to_string())
            .await
            .unwrap();
        assert!(
            result.is_none(),
            "pre-commit receipt MUST be None — aggkit reads it as 'keep polling'"
        );
    }

    /// Receipts contract — a tx that emitted a synthetic log (a projected
    /// BridgeEvent / ClaimEvent / GER update) has NO real txn record, yet
    /// `eth_getTransactionReceipt` MUST return a receipt, not null. aggkit's
    /// L2BridgeSyncer fetches the receipt for every bridge/claim log; a null
    /// receipt makes it fail to append the log ("input too short: 0 bytes") and
    /// stalls the L2 sync so no certificate is ever built. Regression guard for
    /// the SyntheticProjector cut-over (derived tx_hash, no `eth_sendRawTransaction`).
    #[tokio::test]
    async fn synthetic_log_tx_synthesises_a_receipt() {
        use crate::log_synthesis::SyntheticLog;
        let service = create_test_service();
        // A derived synthetic tx_hash with NO txn_begin/commit — exactly what the
        // projector emits for a consumed bridge-out / claim note.
        let tx = "0x4b7cc79e914e4e8ab23640d2aa2e48cdd5627669f5d982e2c85e47e8c1d558eb";
        service
            .store
            .add_log(SyntheticLog {
                address: "0xc8cbebf950b9df44d987c8619f092bea980ff038".to_string(),
                topics: vec![
                    "0x1df3f2a973a00d6635911755c260704e95e8a5876997546798770f76396fda4d"
                        .to_string(),
                ],
                data: "0x".to_string(),
                block_number: 39,
                block_hash: [0u8; 32],
                transaction_hash: tx.to_string(),
                transaction_index: 0,
                log_index: 0,
                removed: false,
            })
            .await
            .unwrap();

        let receipt = service_get_txn_receipt(service, tx.to_string())
            .await
            .unwrap()
            .expect("a tx that emitted a synthetic log MUST have a receipt, not null");
        assert_eq!(receipt.block_number, Some(39), "receipt at the log's block");
        assert!(
            matches!(
                receipt.inner.as_receipt().unwrap().status,
                alloy::consensus::Eip658Value::Eip658(true)
            ),
            "synthetic-log receipt is a success"
        );
    }
}
