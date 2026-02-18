use crate::hex::hex_decode_prefixed;
use crate::service_state::ServiceState;
use alloy::consensus::TxEnvelope;
use alloy::eips::Decodable2718;
use alloy::primitives::TxHash;
use alloy_core::sol_types::SolCall;
use miden_agglayer_service::claim::claimAssetCall;
use miden_agglayer_service::ger::insertGlobalExitRootCall;
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
            TransactionData { hash, input: txn.input }
        },
        TxEnvelope::Legacy(txn_signed) => {
            let hash = *txn_signed.hash();
            let txn = txn_signed.strip_signature();
            TransactionData { hash, input: txn.input }
        },
        _ => {
            tracing::error!("unhandled txn type {:?}", txn_envelope.tx_type());
            anyhow::bail!("unhandled txn type {:?}", txn_envelope.tx_type());
        },
    };
    Ok(data)
}

pub async fn service_send_raw_txn(service: ServiceState, input: String) -> anyhow::Result<TxHash> {
    let payload = hex_decode_prefixed(&input)?;
    let mut payload_slice = payload.as_slice();
    let txn_envelope = TxEnvelope::decode_2718(&mut payload_slice)?;
    let txn = unwrap_txn_envelope(txn_envelope.clone())?;
    let txn_hash = txn.hash;
    tracing::debug!(target: concat!(module_path!(), "::debug"), "raw transaction hash: {txn_hash}");

    let params_encoded = &txn.input;
    if params_encoded.starts_with(&claimAssetCall::SELECTOR) {
        tracing::debug!("claimAsset call");
        let params = claimAssetCall::abi_decode(params_encoded)?;
        tracing::debug!(target: concat!(module_path!(), "::debug"), "claimAsset call params: {params:?}");

        let result = claim::publish_claim(
            params,
            &service.miden_client,
            service.accounts,
            service.block_num_tracker.latest(),
        )
        .await;
        match result {
            Ok(txn) => {
                let txn_id = txn.txn_id;
                tracing::info!("published claim with eth txn: {txn_hash}; miden txn: {txn_id}");
                service.txn_manager.begin(
                    txn_hash,
                    Some(txn_id),
                    txn_envelope,
                    Some(txn.expires_at),
                    Vec::new(),
                )?;
            },
            Err(err) => {
                tracing::error!("publish_claim failed: {err:#?}");
                return Err(err);
            },
        }
    } else if params_encoded.starts_with(&insertGlobalExitRootCall::SELECTOR) {
        tracing::debug!("insertGlobalExitRoot call");
        let params = insertGlobalExitRootCall::abi_decode(params_encoded)?;
        tracing::debug!(target: concat!(module_path!(), "::debug"), "insertGlobalExitRoot call params: {params:?}");

        let result = ger::insert_ger(params).await;
        match result {
            Ok(log_data) => {
                tracing::info!("inserted GER with eth txn: {txn_hash}");
                service.txn_manager.begin(txn_hash, None, txn_envelope, None, vec![log_data])?;
                let block_num = service.block_num_tracker.latest() + 1;
                service.txn_manager.commit(txn_hash, Ok(()), block_num)?;
            },
            Err(err) => {
                tracing::error!("insert_ger failed: {err:#?}");
                return Err(err);
            },
        }
    } else {
        tracing::error!("unhandled txn method {params_encoded:?}");
        anyhow::bail!("unhandled txn method {params_encoded:?}");
    }

    Ok(txn_hash)
}
