use crate::claim::claimAssetCall;
use crate::ger::{insertGlobalExitRootCall, updateExitRootCall};
use crate::hex::hex_decode_prefixed;
use crate::service_state::ServiceState;
use crate::store::TxnEntry;
use crate::*;
use alloy::consensus::TxEnvelope;
use alloy::consensus::transaction::SignerRecoverable;
use alloy::eips::Decodable2718;
use alloy::primitives::{Address, TxHash};
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
    service: &ServiceState,
    ger_bytes: [u8; 32],
) -> anyhow::Result<()> {
    match result {
        Ok(_ger_result) => {
            service.store.mark_ger_injected(ger_bytes).await;
            tracing::info!("inserted GER with eth txn: {txn_hash}");
            // Pass empty logs to store — the GER event is already stored
            // by insert_ger() → add_ger_update_event(). Passing
            // log_data here would create a duplicate at the bridge address
            // instead of the GER contract address.
            service
                .store
                .txn_begin(
                    txn_hash,
                    TxnEntry {
                        id: None,
                        envelope: txn_envelope,
                        signer: Address::ZERO,
                        expires_at: None,
                        logs: vec![],
                    },
                )
                .await?;
            let block_num = service.store.get_latest_block_number().await;
            let block_hash = service.block_state.get_block_hash(block_num);
            service
                .store
                .txn_commit(txn_hash, Ok(()), block_num, block_hash)
                .await?;
            Ok(())
        }
        Err(err) => {
            tracing::error!("insert_ger failed: {err:#?}");
            Err(err)
        }
    }
}

pub async fn service_send_raw_txn(service: ServiceState, input: String) -> anyhow::Result<TxHash> {
    let payload = hex_decode_prefixed(&input)?;
    let mut payload_slice = payload.as_slice();
    let txn_envelope = TxEnvelope::decode_2718(&mut payload_slice)?;
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
            if let Some(l1_url) = &service.l1_rpc_url {
                tracing::info!(
                    "forwarding L2→L1 claim to L1 (destinationNetwork=0), hash={txn_hash}"
                );
                let provider = alloy::providers::ProviderBuilder::new()
                    .connect_http(l1_url.parse().map_err(|e| anyhow::anyhow!("bad L1 URL: {e}"))?);
                use alloy::providers::Provider;
                let result = provider
                    .raw_request::<_, String>(
                        "eth_sendRawTransaction".into(),
                        [&input],
                    )
                    .await;
                match result {
                    Ok(hash) => tracing::info!("L1 claim tx forwarded: {hash}"),
                    Err(e) => tracing::warn!("L1 claim tx forward failed: {e:#}"),
                }
                return Ok(txn_hash);
            }
        }

        // Skip zero-amount claims (e.g., genesis batch deposit). These create
        // CLAIM notes that crash the NTX builder's faucet actor.
        if params.amount.is_zero() {
            tracing::info!("skipping zero-amount claim (genesis batch)");
            return Ok(txn_hash);
        }

        service.store.try_claim(params.globalIndex).await?;

        // Emit ClaimEvent BEFORE publish_claim so the bridge-service's L2 sync
        // and the aggsender's BridgeL2Sync both see it as an imported_exit.
        //
        // Why before: publish_claim takes ~15s (GER propagation wait). During that
        // time, the bridge-service continuously syncs L2 blocks. If the ClaimEvent
        // is emitted after, the bridge-service detects the block-number gap as a
        // reorg and gets stuck in a resync loop.
        //
        // Safety: store.try_claim (above) prevents double-processing.
        // If publish_claim fails, the bridge-service will retry the claimAsset tx.
        // The ClaimEvent data is fully determined by the claimAsset params.
        {
            use alloy::sol_types::SolEvent;
            let event = claim::ClaimEvent::from(params.clone());
            let log_data = event.encode_log_data();
            let block_num = service.store.advance_block_number().await;
            let block_hash = service.block_state.get_block_hash(block_num);
            let claim_log = crate::log_synthesis::SyntheticLog {
                address: crate::bridge_address::get_bridge_address().to_string(),
                topics: log_data.topics().iter().map(|t| t.to_string()).collect(),
                data: log_data.data.to_string(),
                block_number: block_num,
                block_hash,
                transaction_hash: format!("{txn_hash:#x}"),
                transaction_index: 0,
                log_index: 0,
                removed: false,
            };
            service.store.add_log(claim_log).await;
            tracing::info!("emitted ClaimEvent at block {block_num} for aggsender imported_exit");
        }

        let result = claim::publish_claim(
            params.clone(),
            &service.miden_client,
            service.accounts,
            service.store.clone(),
            service.store.get_latest_block_number().await,
        )
        .await;
        match result {
            Ok(claim_result) => {
                let txn_id = claim_result.txn_id;
                tracing::info!("published claim with eth txn: {txn_hash}; miden txn: {txn_id}");

                let block_num = service.store.get_latest_block_number().await;
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
                service.store.unclaim(&params.globalIndex).await;
                tracing::error!("publish_claim failed: {err:#?}");
                return Err(err);
            }
        }
    } else if params_encoded.starts_with(&insertGlobalExitRootCall::SELECTOR) {
        tracing::debug!("insertGlobalExitRoot call");
        let params = insertGlobalExitRootCall::abi_decode(params_encoded)?;
        tracing::debug!(target: concat!(module_path!(), "::debug"), "insertGlobalExitRoot call params: {params:?}");
        let ger_bytes: [u8; 32] = params.root.0;

        // Look up individual exit roots from L1 so zkevm_getExitRootsByGER returns
        // real values. Without this, bridge-service builds claims with zero roots.
        let (mainnet_root, rollup_root) = if let Some(l1_url) = &service.l1_rpc_url {
            let l1_ger_addr = std::env::var("L1_GER_ADDRESS")
                .unwrap_or_else(|_| "0x1f7ad7caA53e35b4f0D138dC5CBF91aC108a2674".to_string());
            match ger::fetch_l1_exit_roots(l1_url, &l1_ger_addr).await {
                Ok((m, r)) => {
                    let computed = ger::combined_ger(&m, &r);
                    if computed == ger_bytes {
                        tracing::info!(
                            mainnet = %alloy::hex::encode(m),
                            rollup = %alloy::hex::encode(r),
                            "fetched exit roots from L1 (verified)"
                        );
                        (Some(m), Some(r))
                    } else {
                        tracing::warn!("L1 exit roots stale, storing without decomposition");
                        (None, None)
                    }
                }
                Err(e) => {
                    tracing::warn!("failed to fetch L1 exit roots: {e:#}");
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
            &service,
            combined_ger,
        )
        .await?;
    } else {
        tracing::error!("unhandled txn method {params_encoded:?}");
        anyhow::bail!("unhandled txn method {params_encoded:?}");
    }

    service.store.nonce_increment(&format!("{signer:#x}")).await;
    Ok(txn_hash)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::block_state::BlockState;
    use crate::{MidenClient, ServiceState};
    use alloy::consensus::{Signed, TxEnvelope, TxLegacy};
    use alloy::eips::Encodable2718;
    use alloy::primitives::{Signature, TxHash};
    use std::sync::Arc;

    fn create_test_service() -> ServiceState {
        let store: Arc<dyn crate::store::Store> = Arc::new(crate::store::memory::InMemoryStore::new());
        let block_state = Arc::new(BlockState::new());
        let miden_client = MidenClient::new_test();
        let accounts = crate::load_config(None).unwrap_or_else(|_| unsafe { std::mem::zeroed() });
        ServiceState::new(miden_client, accounts, 1, 1, store, block_state, None)
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

        // Create a dummy transaction with some random input
        let txn = TxLegacy {
            input: alloy::primitives::bytes!("12345678"),
            ..Default::default()
        };
        let signature = Signature::test_signature();
        let signed_txn = Signed::new_unchecked(txn, signature, TxHash::default());
        let envelope = TxEnvelope::Legacy(signed_txn);
        let mut encoded = Vec::new();
        envelope.encode_2718(&mut encoded);
        let input_hex = format!("0x{}", ::hex::encode(encoded));

        let result = service_send_raw_txn(service, input_hex).await;
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("unhandled txn method")
        );
    }
}
