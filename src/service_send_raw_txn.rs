use crate::claim::claimAssetCall;
use crate::ger::{insertGlobalExitRootCall, updateExitRootCall};
use crate::hex::hex_decode_prefixed;
use crate::service_state::ServiceState;
use crate::store::TxnEntry;
use crate::*;
use alloy::consensus::TxEnvelope;
use alloy::consensus::transaction::SignerRecoverable;
use alloy::eips::Decodable2718;
use alloy::primitives::{Address, LogData, TxHash};
use alloy_core::sol_types::SolCall;

struct TransactionData {
    pub hash: TxHash,
    pub input: alloy::primitives::Bytes,
}

fn unwrap_txn_envelope(txn_envelope: TxEnvelope) -> anyhow::Result<TransactionData> {
    let data = match txn_envelope {
        TxEnvelope::Eip1559(txn_signed) => {
            let hash = *txn_signed.hash();
            let txn = txn_signed.strip_signature();
            TransactionData {
                hash,
                input: txn.input,
            }
        }
        TxEnvelope::Legacy(txn_signed) => {
            let hash = *txn_signed.hash();
            let txn = txn_signed.strip_signature();
            TransactionData {
                hash,
                input: txn.input,
            }
        }
        _ => {
            tracing::error!("unhandled txn type {:?}", txn_envelope.tx_type());
            anyhow::bail!("unhandled txn type {:?}", txn_envelope.tx_type());
        }
    };
    Ok(data)
}

async fn handle_ger_result(
    result: anyhow::Result<ger::GerInsertResult>,
    txn_hash: TxHash,
    txn_envelope: TxEnvelope,
    signer: Address,
    service: &ServiceState,
    ger_bytes: [u8; 32],
) -> anyhow::Result<()> {
    match result {
        Ok(ger_result) => {
            service.store.mark_ger_injected(ger_bytes).await?;
            tracing::info!("inserted GER with eth txn: {txn_hash}");
            record_local_success_at_block(
                service,
                txn_hash,
                txn_envelope,
                signer,
                ger_result.block_number,
                vec![],
            )
            .await?;
            Ok(())
        }
        Err(err) => {
            tracing::error!("insert_ger failed: {err:#?}");
            Err(err)
        }
    }
}

async fn record_local_pending_tx(
    service: &ServiceState,
    tx_hash: TxHash,
    txn_envelope: TxEnvelope,
    signer: Address,
    expires_at: Option<u64>,
    logs: Vec<LogData>,
) -> anyhow::Result<()> {
    service
        .store
        .txn_begin(
            tx_hash,
            TxnEntry {
                id: None,
                envelope: txn_envelope,
                signer,
                expires_at,
                logs,
            },
        )
        .await
}

async fn record_local_immediate_success(
    service: &ServiceState,
    tx_hash: TxHash,
    txn_envelope: TxEnvelope,
    signer: Address,
    logs: Vec<LogData>,
) -> anyhow::Result<()> {
    let block_num = service.store.get_latest_block_number().await?;
    record_local_success_at_block(service, tx_hash, txn_envelope, signer, block_num, logs).await
}

async fn record_local_success_at_block(
    service: &ServiceState,
    tx_hash: TxHash,
    txn_envelope: TxEnvelope,
    signer: Address,
    block_num: u64,
    logs: Vec<LogData>,
) -> anyhow::Result<()> {
    record_local_pending_tx(service, tx_hash, txn_envelope, signer, None, logs).await?;
    let block_hash = service.block_state.get_block_hash(block_num);
    service
        .store
        .txn_commit(tx_hash, Ok(()), block_num, block_hash)
        .await
}

pub async fn service_send_raw_txn(service: ServiceState, input: String) -> anyhow::Result<TxHash> {
    let payload = hex_decode_prefixed(&input)?;
    let mut payload_slice = payload.as_slice();
    let txn_envelope = TxEnvelope::decode_2718(&mut payload_slice)?;

    // Validate chain_id to prevent cross-chain replay attacks
    let tx_chain_id = match &txn_envelope {
        TxEnvelope::Eip1559(signed) => signed.tx().chain_id,
        TxEnvelope::Legacy(signed) => signed.tx().chain_id.unwrap_or(0),
        _ => 0,
    };
    if tx_chain_id != 0 && tx_chain_id != service.chain_id {
        anyhow::bail!(
            "chain_id mismatch: transaction has {tx_chain_id}, expected {}",
            service.chain_id
        );
    }

    let txn = unwrap_txn_envelope(txn_envelope.clone())?;
    let txn_hash = txn.hash;
    let signer = txn_envelope.recover_signer()?;
    tracing::debug!(target: concat!(module_path!(), "::debug"), "raw transaction hash: {txn_hash}");

    let params_encoded = &txn.input;
    if params_encoded.starts_with(&claimAssetCall::SELECTOR) {
        tracing::debug!("claimAsset call");
        let params = claimAssetCall::abi_decode(params_encoded)?;
        tracing::debug!(target: concat!(module_path!(), "::debug"), "claimAsset call params: {params:?}");

        // Claims targeting a different network: forward to L1 for settlement.
        // Only claims where destinationNetwork matches our network_id are processed
        // as Miden CLAIM notes. All others go to L1.
        if params.destinationNetwork != service.network_id as u32 {
            let Some(l1_client) = &service.l1_client else {
                anyhow::bail!(
                    "claim targets destinationNetwork {} but L1 forwarding is not configured",
                    params.destinationNetwork
                );
            };
            tracing::info!("forwarding L2→L1 claim to L1 (destinationNetwork=0), hash={txn_hash}");
            let forwarded_hash = l1_client.send_raw_transaction(&input).await?;
            if !forwarded_hash.eq_ignore_ascii_case(&format!("{txn_hash:#x}")) {
                tracing::warn!(
                    expected = %format!("{txn_hash:#x}"),
                    actual = %forwarded_hash,
                    "L1 returned a different transaction hash for forwarded claim"
                );
            }
            tracing::info!("L1 claim tx forwarded: {forwarded_hash}");
            record_local_pending_tx(&service, txn_hash, txn_envelope, signer, None, vec![]).await?;
            service
                .store
                .nonce_increment(&format!("{signer:#x}"))
                .await?;
            return Ok(txn_hash);
        }

        // Skip zero-amount claims (e.g., genesis batch deposit). These create
        // CLAIM notes that crash the NTX builder's faucet actor.
        if params.amount.is_zero() {
            tracing::info!("skipping zero-amount claim (genesis batch)");
            record_local_immediate_success(&service, txn_hash, txn_envelope, signer, vec![])
                .await?;
            service
                .store
                .nonce_increment(&format!("{signer:#x}"))
                .await?;
            return Ok(txn_hash);
        }

        service.store.try_claim(params.globalIndex).await?;

        let result = claim::publish_claim(
            params.clone(),
            &service.miden_client,
            service.accounts,
            service.store.clone(),
            service.store.get_latest_block_number().await?,
        )
        .await;
        match result {
            Ok(claim_result) => {
                // Note: bridge-service will see this ClaimEvent from both ClaimTxManager and
                // L2 sync, causing a duplicate key error. See fixtures/bridge-db-patch.sql.
                let txn_id = claim_result.txn_id;
                tracing::info!("published claim with eth txn: {txn_hash}; miden txn: {txn_id}");
                let block_num = service.store.advance_block_number().await?;
                let block_hash = service.block_state.get_block_hash(block_num);
                service
                    .store
                    .txn_begin(
                        txn_hash,
                        TxnEntry {
                            id: Some(txn_id),
                            envelope: txn_envelope,
                            signer,
                            expires_at: Some(claim_result.expires_at),
                            logs: vec![claim_result.log],
                        },
                    )
                    .await?;
                service
                    .store
                    .txn_commit(txn_hash, Ok(()), block_num, block_hash)
                    .await?;
            }
            Err(err) => {
                let _ = service.store.unclaim(&params.globalIndex).await;
                tracing::error!("publish_claim failed: {err:#?}");
                return Err(err);
            }
        }
    } else if params_encoded.starts_with(&insertGlobalExitRootCall::SELECTOR) {
        tracing::debug!("insertGlobalExitRoot call");
        let params = insertGlobalExitRootCall::abi_decode(params_encoded)?;
        tracing::debug!(target: concat!(module_path!(), "::debug"), "insertGlobalExitRoot call params: {params:?}");
        let ger_bytes: [u8; 32] = params.root.0;

        // Resolve the combined GER to its L1 mainnet/rollup components.
        // We only check the latest roots on L1 — if L1 has moved on (rare),
        // the roots will be resolved lazily via zkevm_getExitRootsByGER.
        let (mainnet_root, rollup_root) = if let Some(l1_client) = &service.l1_client {
            match l1_client.fetch_exit_roots().await {
                Ok((m, r)) if ger::combined_ger(&m, &r) == ger_bytes => {
                    tracing::info!("fetched exit roots from L1 (verified)");
                    (Some(m), Some(r))
                }
                Ok(_) => {
                    tracing::debug!("L1 roots stale, will resolve lazily");
                    (None, None)
                }
                Err(e) => {
                    tracing::warn!("L1 fetch failed: {e:#}");
                    (None, None)
                }
            }
        } else {
            (None, None)
        };

        handle_ger_result(
            ger::insert_ger(
                ger_bytes,
                mainnet_root,
                rollup_root,
                &service.miden_client,
                service.accounts.clone(),
                &service.store,
                &service.block_state,
                txn_hash,
            )
            .await,
            txn_hash,
            txn_envelope,
            signer,
            &service,
            ger_bytes,
        )
        .await?;
    } else if params_encoded.starts_with(&updateExitRootCall::SELECTOR) {
        tracing::debug!("updateExitRoot call");
        let params = updateExitRootCall::abi_decode(params_encoded)?;
        tracing::debug!(target: concat!(module_path!(), "::debug"), "updateExitRoot call params: {params:?}");

        let mainnet_root = params.newMainnetExitRoot.0;
        let rollup_root = params.newRollupExitRoot.0;
        let combined_ger = ger::combined_ger(&mainnet_root, &rollup_root);

        handle_ger_result(
            ger::insert_ger(
                combined_ger,
                Some(mainnet_root),
                Some(rollup_root),
                &service.miden_client,
                service.accounts.clone(),
                &service.store,
                &service.block_state,
                txn_hash,
            )
            .await,
            txn_hash,
            txn_envelope,
            signer,
            &service,
            combined_ger,
        )
        .await?;
    } else {
        tracing::error!("unhandled txn method {params_encoded:?}");
        anyhow::bail!("unhandled txn method {params_encoded:?}");
    }

    service
        .store
        .nonce_increment(&format!("{signer:#x}"))
        .await?;
    Ok(txn_hash)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::block_state::BlockState;
    use crate::l1_client::L1Client;
    use crate::store::memory::InMemoryStore;
    use crate::test_helpers::{create_test_service, test_accounts_config};
    use alloy::consensus::{Signed, TxEnvelope, TxLegacy};
    use alloy::eips::Encodable2718;
    use alloy::primitives::{Bytes, FixedBytes, Signature, TxHash, U256};
    use alloy::rpc::types::Filter;
    use alloy_core::sol_types::SolCall;
    use alloy_rpc_types_eth::{Log, ReceiptEnvelope, TransactionReceipt};
    use std::sync::Arc;

    struct ForwardingL1Client {
        forwarded_result: anyhow::Result<String>,
    }

    #[async_trait::async_trait]
    impl L1Client for ForwardingL1Client {
        async fn eth_call(&self, _to: Address, _data: Bytes) -> anyhow::Result<Bytes> {
            anyhow::bail!("unused")
        }

        async fn send_raw_transaction(&self, _raw_tx_hex: &str) -> anyhow::Result<String> {
            self.forwarded_result
                .as_ref()
                .map(|hash| hash.clone())
                .map_err(|err| anyhow::anyhow!("{err:#}"))
        }

        async fn fetch_exit_roots(&self) -> anyhow::Result<([u8; 32], [u8; 32])> {
            anyhow::bail!("unused")
        }

        async fn get_block_number(&self) -> anyhow::Result<u64> {
            anyhow::bail!("unused")
        }

        async fn get_logs(&self, _filter: &Filter) -> anyhow::Result<Vec<Log>> {
            anyhow::bail!("unused")
        }

        async fn get_transaction_receipt(
            &self,
            _tx_hash: TxHash,
        ) -> anyhow::Result<Option<TransactionReceipt<ReceiptEnvelope<Log>>>> {
            Ok(None)
        }
    }

    struct FetchExitRootsL1Client {
        exit_roots: ([u8; 32], [u8; 32]),
    }

    #[async_trait::async_trait]
    impl L1Client for FetchExitRootsL1Client {
        async fn eth_call(&self, _to: Address, _data: Bytes) -> anyhow::Result<Bytes> {
            anyhow::bail!("unused")
        }

        async fn send_raw_transaction(&self, _raw_tx_hex: &str) -> anyhow::Result<String> {
            anyhow::bail!("unused")
        }

        async fn fetch_exit_roots(&self) -> anyhow::Result<([u8; 32], [u8; 32])> {
            Ok(self.exit_roots)
        }

        async fn get_block_number(&self) -> anyhow::Result<u64> {
            anyhow::bail!("unused")
        }

        async fn get_logs(&self, _filter: &Filter) -> anyhow::Result<Vec<Log>> {
            anyhow::bail!("unused")
        }

        async fn get_transaction_receipt(
            &self,
            _tx_hash: TxHash,
        ) -> anyhow::Result<Option<TransactionReceipt<ReceiptEnvelope<Log>>>> {
            Ok(None)
        }
    }

    /// Encode a legacy transaction with the given calldata into a hex string
    /// suitable for `service_send_raw_txn`.
    fn encode_legacy_tx(input: Vec<u8>) -> (String, Address) {
        let txn = TxLegacy {
            input: input.into(),
            ..Default::default()
        };
        let signature = Signature::test_signature();
        let signed = Signed::new_unchecked(txn, signature, TxHash::default());
        let envelope = TxEnvelope::Legacy(signed);
        let signer = envelope.recover_signer().expect("recover signer");
        let mut encoded = Vec::new();
        envelope.encode_2718(&mut encoded);
        (format!("0x{}", ::hex::encode(encoded)), signer)
    }

    fn create_test_service_with_l1(l1_client: Arc<dyn L1Client>) -> ServiceState {
        let store: Arc<dyn crate::store::Store> = Arc::new(InMemoryStore::new());
        let block_state = Arc::new(BlockState::new());
        ServiceState::new(
            crate::MidenClient::new_test(),
            test_accounts_config(),
            1,
            1,
            store,
            block_state,
            Some(l1_client),
            String::new(),
            String::new(),
        )
    }

    #[tokio::test]
    async fn test_service_send_raw_txn_invalid_hex() {
        let service = create_test_service();
        let result = service_send_raw_txn(service, "invalid".to_string()).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_service_send_raw_txn_invalid_rlp() {
        let service = create_test_service();
        let result = service_send_raw_txn(service, "0x1234".to_string()).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_service_send_raw_txn_unhandled_method() {
        let service = create_test_service();
        let (input_hex, _) = encode_legacy_tx(vec![0x12, 0x34, 0x56, 0x78]);

        let result = service_send_raw_txn(service, input_hex).await;
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("unhandled txn method")
        );
    }

    // ── Happy-path tests ────────────────────────────────────────────

    #[tokio::test]
    async fn test_insert_global_exit_root_stores_ger_and_emits_log() {
        let service = create_test_service();
        let store = service.store.clone();
        let ger_bytes = [0xAA; 32];

        let calldata = insertGlobalExitRootCall {
            root: FixedBytes::from(ger_bytes),
        }
        .abi_encode();
        let (input_hex, _) = encode_legacy_tx(calldata);

        let result = service_send_raw_txn(service, input_hex).await;
        assert!(
            result.is_ok(),
            "insertGlobalExitRoot should succeed: {result:?}"
        );

        // GER should be marked as seen and injected in the store
        assert!(store.has_seen_ger(&ger_bytes).await.unwrap());
        assert!(store.is_ger_injected(&ger_bytes).await.unwrap());

        // The MidenClient test stub should have been called once (for submit_ger_to_miden)
        // (checked indirectly: if it wasn't called, insert_ger would have bailed)

        // An UpdateHashChainValue log should have been emitted
        let filter = crate::log_synthesis::LogFilter {
            from_block: Some("0x0".to_string()),
            to_block: Some("0xFFFF".to_string()),
            ..Default::default()
        };
        let logs = store.get_logs(&filter, 0xFFFF).await.unwrap();
        assert!(
            !logs.is_empty(),
            "expected at least one log from GER insertion"
        );
        assert!(
            logs.iter().any(|l| l.topics.first().map(|t| t.as_str())
                == Some(crate::log_synthesis::UPDATE_HASH_CHAIN_VALUE_TOPIC)),
            "expected UpdateHashChainValue log"
        );
    }

    #[tokio::test]
    async fn test_insert_global_exit_root_persists_resolved_l1_exit_roots() {
        let mainnet = [0x55; 32];
        let rollup = [0x66; 32];
        let ger_bytes = crate::ger::combined_ger(&mainnet, &rollup);
        let service = create_test_service_with_l1(Arc::new(FetchExitRootsL1Client {
            exit_roots: (mainnet, rollup),
        }));
        let store = service.store.clone();

        let calldata = insertGlobalExitRootCall {
            root: FixedBytes::from(ger_bytes),
        }
        .abi_encode();
        let (input_hex, _) = encode_legacy_tx(calldata);

        service_send_raw_txn(service, input_hex).await.unwrap();

        let entry = store.get_ger_entry(&ger_bytes).await.unwrap().unwrap();
        assert_eq!(entry.mainnet_exit_root, Some(mainnet));
        assert_eq!(entry.rollup_exit_root, Some(rollup));
    }

    #[tokio::test]
    async fn test_claim_asset_zero_amount_skipped() {
        let service = create_test_service();
        let store = service.store.clone();

        let calldata = claimAssetCall {
            smtProofLocalExitRoot: [FixedBytes::ZERO; 32],
            smtProofRollupExitRoot: [FixedBytes::ZERO; 32],
            globalIndex: U256::from(1u64),
            mainnetExitRoot: FixedBytes::ZERO,
            rollupExitRoot: FixedBytes::ZERO,
            originNetwork: 0,
            originTokenAddress: Address::ZERO,
            destinationNetwork: 1, // matches service.network_id
            destinationAddress: Address::ZERO,
            amount: U256::ZERO, // zero amount — should be skipped
            metadata: Default::default(),
        }
        .abi_encode();
        let (input_hex, signer) = encode_legacy_tx(calldata);

        let result = service_send_raw_txn(service, input_hex).await;
        assert!(
            result.is_ok(),
            "zero-amount claimAsset should succeed: {result:?}"
        );
        let tx_hash = result.unwrap();

        // The claim should NOT be recorded (try_claim is never called for zero-amount)
        assert!(
            !store.is_claimed(&U256::from(1u64)).await.unwrap(),
            "zero-amount claim should not be recorded in store"
        );
        assert_eq!(store.nonce_get(&format!("{signer:#x}")).await.unwrap(), 1);
        assert!(store.txn_receipt(tx_hash).await.unwrap().is_some());

        // No ClaimEvent log should have been emitted
        let filter = crate::log_synthesis::LogFilter {
            from_block: Some("0x0".to_string()),
            to_block: Some("0xFFFF".to_string()),
            ..Default::default()
        };
        let logs = store.get_logs(&filter, 0xFFFF).await.unwrap();
        let claim_logs: Vec<_> = logs
            .iter()
            .filter(|l| {
                l.topics.first().map(|t| t.as_str())
                    == Some(crate::log_synthesis::CLAIM_EVENT_TOPIC)
            })
            .collect();
        assert!(
            claim_logs.is_empty(),
            "zero-amount claim should not emit ClaimEvent"
        );
    }

    /// Test that a valid claimAsset call with non-zero amount:
    /// 1. Marks the claim in the store via try_claim
    /// 2. Invokes the MidenClient (publish_claim calls .with())
    /// 3. Emits ClaimEvent only AFTER successful claim execution
    ///
    /// The test MidenClient stub returns Ok(()) without executing the
    /// closure, so publish_claim's OnceLock is never set and it returns
    /// an error. The ClaimEvent must NOT appear in the store since the
    /// claim failed — this prevents state divergence between what
    /// bridge-service believes and what Miden actually holds.
    #[tokio::test]
    async fn test_claim_asset_no_event_on_failure() {
        let service = create_test_service();
        let store = service.store.clone();
        let miden_client = service.miden_client.clone();

        let global_index = U256::from(42u64);
        let calldata = claimAssetCall {
            smtProofLocalExitRoot: [FixedBytes::ZERO; 32],
            smtProofRollupExitRoot: [FixedBytes::ZERO; 32],
            globalIndex: global_index,
            mainnetExitRoot: FixedBytes::ZERO,
            rollupExitRoot: FixedBytes::ZERO,
            originNetwork: 0,
            originTokenAddress: Address::ZERO,
            destinationNetwork: 1, // matches service.network_id
            destinationAddress: Address::from([0x42; 20]),
            amount: U256::from(1_000_000u64),
            metadata: Default::default(),
        }
        .abi_encode();
        let (input_hex, _) = encode_legacy_tx(calldata);

        // publish_claim will fail because the test stub doesn't execute the
        // closure (no real MidenClientLib). That's expected.
        let result = service_send_raw_txn(service, input_hex).await;
        assert!(result.is_err(), "publish_claim should fail with test stub");

        // The MidenClient should have been called (publish_claim uses .with())
        assert!(
            miden_client.test_was_called(),
            "MidenClient should have been invoked by publish_claim"
        );

        // ClaimEvent must NOT be in the store — it's only emitted after
        // successful claim execution, preventing state divergence.
        let filter = crate::log_synthesis::LogFilter {
            from_block: Some("0x0".to_string()),
            to_block: Some("0xFFFF".to_string()),
            ..Default::default()
        };
        let logs = store.get_logs(&filter, 0xFFFF).await.unwrap();
        let claim_logs: Vec<_> = logs
            .iter()
            .filter(|l| {
                l.topics.first().map(|t| t.as_str())
                    == Some(crate::log_synthesis::CLAIM_EVENT_TOPIC)
            })
            .collect();
        assert!(
            claim_logs.is_empty(),
            "ClaimEvent must not be emitted when publish_claim fails"
        );

        // On publish_claim failure, the claim is rolled back via unclaim()
        assert!(
            !store.is_claimed(&global_index).await.unwrap(),
            "claim should be unclaimed after publish_claim failure"
        );
    }

    #[tokio::test]
    async fn test_forwarded_claim_tracks_pending_tx_and_nonce() {
        let expected_hash = format!("{:#x}", TxHash::from([3u8; 32]));
        let service = create_test_service_with_l1(Arc::new(ForwardingL1Client {
            forwarded_result: Ok(expected_hash.clone()),
        }));
        let store = service.store.clone();

        let calldata = claimAssetCall {
            smtProofLocalExitRoot: [FixedBytes::ZERO; 32],
            smtProofRollupExitRoot: [FixedBytes::ZERO; 32],
            globalIndex: U256::from(9u64),
            mainnetExitRoot: FixedBytes::ZERO,
            rollupExitRoot: FixedBytes::ZERO,
            originNetwork: 0,
            originTokenAddress: Address::ZERO,
            destinationNetwork: 2,
            destinationAddress: Address::ZERO,
            amount: U256::from(1u64),
            metadata: Default::default(),
        }
        .abi_encode();
        let (input_hex, signer) = encode_legacy_tx(calldata);

        let tx_hash = service_send_raw_txn(service, input_hex).await.unwrap();

        assert_eq!(store.nonce_get(&format!("{signer:#x}")).await.unwrap(), 1);
        assert!(store.txn_get(tx_hash).await.unwrap().is_some());
        assert!(store.txn_receipt(tx_hash).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn test_forwarded_claim_does_not_ack_failed_l1_submission() {
        let service = create_test_service_with_l1(Arc::new(ForwardingL1Client {
            forwarded_result: Err(anyhow::anyhow!("upstream rejected tx")),
        }));
        let store = service.store.clone();

        let calldata = claimAssetCall {
            smtProofLocalExitRoot: [FixedBytes::ZERO; 32],
            smtProofRollupExitRoot: [FixedBytes::ZERO; 32],
            globalIndex: U256::from(10u64),
            mainnetExitRoot: FixedBytes::ZERO,
            rollupExitRoot: FixedBytes::ZERO,
            originNetwork: 0,
            originTokenAddress: Address::ZERO,
            destinationNetwork: 2,
            destinationAddress: Address::ZERO,
            amount: U256::from(1u64),
            metadata: Default::default(),
        }
        .abi_encode();
        let (input_hex, signer) = encode_legacy_tx(calldata);

        let result = service_send_raw_txn(service, input_hex).await;
        assert!(result.is_err());
        assert_eq!(store.nonce_get(&format!("{signer:#x}")).await.unwrap(), 0);
    }
}
