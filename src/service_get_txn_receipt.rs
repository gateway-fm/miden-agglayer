use crate::service_state::ServiceState;
use alloy::consensus::{Eip658Value, Receipt, ReceiptEnvelope, ReceiptWithBloom};
use alloy::primitives::{Log, TxHash};
use alloy::rpc::types::TransactionReceipt;
use std::str::FromStr;

// polycli polls receipts to get the eth_sendRawTransaction status
// it logs cumulativeGasUsed and transactionHash
// return null if the transaction is not yet included onto the blockchain, return status=0 for errors
pub async fn service_get_txn_receipt(
    service: ServiceState,
    txn_hash: String,
) -> anyhow::Result<Option<TransactionReceipt<ReceiptEnvelope>>> {
    let txn_hash = TxHash::from_str(&txn_hash)?;
    let (result, block_num) = match service.txn_manager.receipt(txn_hash) {
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
    let receipt = TransactionReceipt {
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
    use crate::block_num_tracker::BlockNumTracker;
    use crate::block_state::BlockState;
    use crate::log_synthesis::LogStore;
    use crate::nonce_tracker::NonceTracker;
    use crate::txn_manager::TxnManager;
    use crate::{AddressMapper, ClaimTracker, MidenClient};
    use alloy::consensus::TxEnvelope;
    use alloy::primitives::TxHash;
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

        // Mock AccountsConfig - since it's a wrapper, we might need a way to create it.
        // For now, let's assume it can be empty.
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
    async fn test_service_get_txn_receipt_not_found() {
        let service = create_test_service();
        let txn_hash = TxHash::from([1u8; 32]).to_string();
        let result = service_get_txn_receipt(service, txn_hash).await.unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn test_service_get_txn_receipt_found() {
        let service = create_test_service();
        let txn_hash = TxHash::from([2u8; 32]);

        // Mock a transaction in TxnManager
        // We need a TxEnvelope. Legacy is easiest to create dummy.
        let txn_envelope = TxEnvelope::Legacy(alloy::consensus::Signed::new_unchecked(
            alloy::consensus::TxLegacy::default(),
            alloy::primitives::Signature::test_signature(),
            txn_hash,
        ));

        service
            .txn_manager
            .begin(txn_hash, None, txn_envelope, None, vec![])
            .unwrap();

        service.txn_manager.commit(txn_hash, Ok(()), 123).unwrap();

        let result = service_get_txn_receipt(service, txn_hash.to_string())
            .await
            .unwrap();
        assert!(result.is_some());
        let receipt = result.unwrap();
        assert_eq!(receipt.transaction_hash, txn_hash);
        assert_eq!(receipt.block_number, Some(123));
        // Go's hexutil.Uint can't unmarshal null, so these must be Some
        assert_eq!(receipt.transaction_index, Some(0));
        assert!(receipt.block_hash.is_some());
        assert!(matches!(
            receipt.inner.as_receipt().unwrap().status,
            alloy::consensus::Eip658Value::Eip658(true)
        ));
    }
}
