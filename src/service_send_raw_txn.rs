use crate::hex::hex_decode_prefixed;
use crate::service_state::ServiceState;
use alloy::consensus::TxEnvelope;
use alloy::consensus::transaction::SignerRecoverable;
use alloy::eips::Decodable2718;
use alloy::primitives::TxHash;
use alloy_core::sol_types::SolCall;
use miden_agglayer_service::claim::claimAssetCall;
use miden_agglayer_service::ger::{insertGlobalExitRootCall, updateExitRootCall};
use miden_agglayer_service::*;

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
        Ok(ger_result) => {
            tracing::info!("inserted GER with eth txn: {txn_hash}");
            service.txn_manager.begin(
                txn_hash,
                None,
                txn_envelope,
                None,
                vec![ger_result.log_data],
            )?;
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

        handle_ger_result(
            ger::insert_ger(
                ger_bytes,
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
        let ger_bytes =
            ger::combined_ger(&params.newMainnetExitRoot.0, &params.newRollupExitRoot.0);

        handle_ger_result(
            ger::insert_ger(
                ger_bytes,
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
