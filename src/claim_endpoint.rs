use crate::hex::hex_decode_prefixed;
use crate::service_state::ServiceState;
use alloy::consensus::{Eip658Value, Receipt, ReceiptEnvelope, ReceiptWithBloom, TxEnvelope};
use alloy::eips::Decodable2718;
use alloy::primitives::{Log, TxHash};
use alloy::rpc::types::TransactionReceipt;
use alloy_core::sol_types::SolCall;
use miden_agglayer_service::claim::claimAssetCall;
use miden_agglayer_service::*;
use std::str::FromStr;

pub async fn claim_endpoint_raw_txn(
    service: ServiceState,
    input: String,
) -> anyhow::Result<TxHash> {
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

// polycli polls receipts to get the eth_sendRawTransaction status
// it logs cumulativeGasUsed and transactionHash
// TODO: return null if the transaction is not yet included onto the blockchain, return status=0 for errors
pub async fn claim_endpoint_txn_receipt(
    _service: ServiceState,
    txn_hash: String,
) -> anyhow::Result<Option<TransactionReceipt<ReceiptEnvelope>>> {
    let status = true;

    let mut receipt_inner = ReceiptWithBloom::<Receipt<Log>>::default();
    receipt_inner.receipt.status = Eip658Value::Eip658(status);
    receipt_inner.receipt.cumulative_gas_used = 0;

    let receipt = TransactionReceipt {
        inner: ReceiptEnvelope::Eip1559(receipt_inner),
        transaction_hash: TxHash::from_str(&txn_hash)?,
        transaction_index: None,
        block_hash: None,
        block_number: None,
        gas_used: 0,
        effective_gas_price: 0,
        blob_gas_used: None,
        blob_gas_price: None,
        from: Default::default(),
        to: None,
        contract_address: None,
    };
    Ok(Some(receipt))
}
