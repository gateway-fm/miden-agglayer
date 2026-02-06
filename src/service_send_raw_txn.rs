use crate::hex::hex_decode_prefixed;
use crate::service_state::ServiceState;
use alloy::consensus::TxEnvelope;
use alloy::eips::Decodable2718;
use alloy::primitives::TxHash;
use alloy_core::sol_types::SolCall;
use miden_agglayer_service::claim::claimAssetCall;
use miden_agglayer_service::*;

pub async fn service_send_raw_txn(service: ServiceState, input: String) -> anyhow::Result<TxHash> {
    let payload = hex_decode_prefixed(&input)?;
    let mut payload_slice = payload.as_slice();
    let txn_envelope = TxEnvelope::decode_2718(&mut payload_slice)?;

    match txn_envelope {
        TxEnvelope::Eip1559(txn_signed) => {
            let txn = txn_signed.tx();
            let txn_hash = *txn_signed.hash();
            tracing::debug!("hash: {txn_hash}");
            tracing::debug!("chain_id: {}", txn.chain_id);
            tracing::debug!("to: {:?}", txn.to);

            let params_encoded = &txn.input;
            if params_encoded.starts_with(&claimAssetCall::SELECTOR) {
                let params = claimAssetCall::abi_decode(params_encoded)?;
                tracing::debug!("claimAsset call params: {params:?}");

                let result =
                    claim::publish_claim(params, &service.miden_client, service.accounts).await;
                if let Err(err) = &result {
                    tracing::error!("publish_claim failed: {err:#?}");
                }
                let txn_id = result?;
                tracing::debug!("published claim txn_id: {txn_id}");
            } else {
                panic!("unhandled txn method {params_encoded:?}");
            }

            Ok(txn_hash)
        },
        _ => {
            panic!("unhandled txn type {:?}", txn_envelope.tx_type());
        },
    }
}
