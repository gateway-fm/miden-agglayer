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
    let (result, block_num) = match service.store.txn_receipt(txn_hash).await? {
        Some((result, block_num)) => (result, block_num),
        None => return Ok(None),
    };
    let status = result.is_ok();

    let mut receipt_inner = ReceiptWithBloom::<Receipt<Log>>::default();
    receipt_inner.receipt.status = Eip658Value::Eip658(status);
    receipt_inner.receipt.cumulative_gas_used = 0;

    // IMPORTANT: Go's hexutil.Uint.UnmarshalJSON cannot handle JSON null.
    // All numeric fields must be present with valid hex values, otherwise
    // Go's types.Receipt unmarshaling fails silently and the EthTxManager
    // treats the tx as "not mined".
    let block_hash = service.block_state.get_block_hash(block_num);
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
        from: Default::default(),
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
                    signer: Address::ZERO,
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
        assert!(matches!(
            receipt.inner.as_receipt().unwrap().status,
            alloy::consensus::Eip658Value::Eip658(true)
        ));
    }
}
