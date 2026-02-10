use crate::hex::hex_decode_prefixed;
use crate::service_state::ServiceState;
use alloy::consensus::{EthereumTxEnvelope, TxEip4844Variant, TxEnvelope};
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

fn unwrap_txn_envelope(
    txn_envelope: EthereumTxEnvelope<TxEip4844Variant>,
) -> anyhow::Result<TransactionData> {
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
    let txn = unwrap_txn_envelope(txn_envelope)?;
    let txn_hash = txn.hash;
    tracing::debug!(target: concat!(module_path!(), "::debug"), "raw transaction hash: {txn_hash}");

    let params_encoded = &txn.input;
    if params_encoded.starts_with(&claimAssetCall::SELECTOR) {
        tracing::debug!("claimAsset call");
        let params = claimAssetCall::abi_decode(params_encoded)?;
        tracing::debug!(target: concat!(module_path!(), "::debug"), "claimAsset call params: {params:?}");

        let result = claim::publish_claim(params, &service.miden_client, service.accounts).await;
        if let Err(err) = &result {
            tracing::error!("publish_claim failed: {err:#?}");
        }
        let txn_id = result?;
        tracing::info!("published claim with eth txn: {txn_hash}; miden txn: {txn_id}");
    } else if params_encoded.starts_with(&insertGlobalExitRootCall::SELECTOR) {
        tracing::debug!("insertGlobalExitRoot call");
        let params = insertGlobalExitRootCall::abi_decode(params_encoded)?;
        tracing::debug!(target: concat!(module_path!(), "::debug"), "insertGlobalExitRoot call params: {params:?}");

        let block_num = service.block_num_tracker.latest();
        let result = ger::insert_ger(params, txn_hash, block_num).await;
        if let Err(err) = &result {
            tracing::error!("insert_ger failed: {err:#?}");
        }
        tracing::info!("inserted GER with eth txn: {txn_hash}");
    } else {
        tracing::error!("unhandled txn method {params_encoded:?}");
        anyhow::bail!("unhandled txn method {params_encoded:?}");
    }

    Ok(txn_hash)
}
