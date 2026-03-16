use crate::claim::claimAssetCall;
use crate::ger::{insertGlobalExitRootCall, updateExitRootCall};
use crate::hex::hex_decode_prefixed;
use crate::service_state::ServiceState;
use crate::*;
use alloy::consensus::TxEnvelope;
use alloy::consensus::transaction::SignerRecoverable;
use alloy::eips::Decodable2718;
use alloy::primitives::TxHash;
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

fn handle_ger_result(
    result: anyhow::Result<ger::GerInsertResult>,
    txn_hash: TxHash,
    txn_envelope: TxEnvelope,
    service: &ServiceState,
) -> anyhow::Result<()> {
    match result {
        Ok(_ger_result) => {
            tracing::info!("inserted GER with eth txn: {txn_hash}");
            // Pass empty logs to TxnManager — the GER event is already stored
            // in LogStore by insert_ger() → add_ger_update_event(). Passing
            // log_data here would create a duplicate at the bridge address
            // instead of the GER contract address.
            service
                .txn_manager
                .begin(txn_hash, None, txn_envelope, None, vec![])?;
            let block_num = service.block_num_tracker.latest();
            service.txn_manager.commit(txn_hash, Ok(()), block_num)?;
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

        // Skip zero-amount claims (e.g., genesis batch deposit). These create
        // CLAIM notes that crash the NTX builder's faucet actor.
        if params.amount.is_zero() {
            tracing::info!("skipping zero-amount claim (genesis batch)");
            return Ok(txn_hash);
        }

        service.claim_tracker.try_claim(params.globalIndex)?;

        let result = claim::publish_claim(
            params.clone(),
            &service.miden_client,
            service.accounts,
            service.address_mapper.clone(),
            service.block_num_tracker.latest(),
        )
        .await;
        match result {
            Ok(claim_result) => {
                let txn_id = claim_result.txn_id;
                let claim_note_id = claim_result.claim_note_id.clone();
                tracing::info!("published claim with eth txn: {txn_hash}; miden txn: {txn_id}");
                service.txn_manager.begin(
                    txn_hash,
                    Some(txn_id),
                    txn_envelope,
                    Some(claim_result.expires_at),
                    vec![claim_result.log],
                )?;
                // Defer receipt until CLAIM note is consumed by faucet
                if let Some(note_id) = claim_note_id {
                    let submit_block = service.block_num_tracker.latest();
                    service.txn_manager.begin_awaiting_consumption(
                        txn_hash,
                        note_id,
                        submit_block,
                    )?;
                }
            }
            Err(err) => {
                service.claim_tracker.unclaim(&params.globalIndex);
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
                &service.log_store,
                &service.block_state,
                txn_hash,
            )
            .await,
            txn_hash,
            txn_envelope,
            &service,
        )?;
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
                &service.log_store,
                &service.block_state,
                txn_hash,
            )
            .await,
            txn_hash,
            txn_envelope,
            &service,
        )?;
    } else {
        tracing::error!("unhandled txn method {params_encoded:?}");
        anyhow::bail!("unhandled txn method {params_encoded:?}");
    }

    service.nonce_tracker.increment(&format!("{signer:#x}"));
    Ok(txn_hash)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::block_num_tracker::BlockNumTracker;
    use crate::block_state::BlockState;
    use crate::log_synthesis::LogStore;
    use crate::nonce_tracker::NonceTracker;
    use crate::txn_manager::TxnManager;
    use crate::{AddressMapper, ClaimTracker, MidenClient, ServiceState};
    use alloy::consensus::{Signed, TxEnvelope, TxLegacy};
    use alloy::eips::Encodable2718;
    use alloy::primitives::{Signature, TxHash};
    use std::sync::Arc;

    fn create_test_service() -> ServiceState {
        let log_store = Arc::new(LogStore::new());
        let block_state = Arc::new(BlockState::new());
        let txn_manager = Arc::new(TxnManager::new(log_store.clone(), block_state.clone()));
        let miden_client = MidenClient::new_test();
        let block_num_tracker = Arc::new(BlockNumTracker::new());
        let nonce_tracker = Arc::new(NonceTracker::new());
        let claim_tracker = Arc::new(ClaimTracker::new(None).unwrap());
        let address_mapper = Arc::new(AddressMapper::new(None).unwrap());

        let accounts = crate::load_config(None).unwrap_or_else(|_| unsafe { std::mem::zeroed() });

        ServiceState::new(
            miden_client,
            accounts,
            1,
            1,
            block_num_tracker,
            txn_manager,
            block_state,
            log_store,
            claim_tracker,
            nonce_tracker,
            address_mapper,
            None,
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
