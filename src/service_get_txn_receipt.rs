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
    let local_tx = service.store.txn_get(txn_hash).await?;
    let (result, block_num) = match service.store.txn_receipt(txn_hash).await? {
        Some((result, block_num)) => (result, block_num),
        None => {
            let should_query_l1 = local_tx.as_ref().is_none_or(|txn| txn.id.is_none());
            if should_query_l1 && let Some(l1_client) = &service.l1_client {
                return l1_client.get_transaction_receipt(txn_hash).await;
            }
            return Ok(None);
        }
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
    use crate::block_state::BlockState;
    use crate::l1_client::L1Client;
    use crate::store::TxnEntry;
    use crate::store::memory::InMemoryStore;
    use crate::test_helpers::{create_test_service, test_accounts_config};
    use alloy::consensus::TxEnvelope;
    use alloy::primitives::{Address, Bytes, TxHash};
    use alloy::rpc::types::Filter;
    use alloy_rpc_types_eth::Log;
    use std::sync::Arc;

    struct ReceiptStub {
        receipt: Option<TransactionReceipt<ReceiptEnvelope<Log>>>,
    }

    #[async_trait::async_trait]
    impl L1Client for ReceiptStub {
        async fn eth_call(&self, _to: Address, _data: Bytes) -> anyhow::Result<Bytes> {
            anyhow::bail!("unused")
        }

        async fn send_raw_transaction(&self, _raw_tx_hex: &str) -> anyhow::Result<String> {
            anyhow::bail!("unused")
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
            Ok(self.receipt.clone())
        }
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
        let block_state = service.block_state.clone();
        let txn_hash = TxHash::from([2u8; 32]);

        // Mock a transaction in the store
        // We need a TxEnvelope. Legacy is easiest to create dummy.
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
        // Go's hexutil.Uint can't unmarshal null, so these must be Some
        assert_eq!(receipt.transaction_index, Some(0));
        assert!(receipt.block_hash.is_some());
        assert!(matches!(
            receipt.inner.as_receipt().unwrap().status,
            alloy::consensus::Eip658Value::Eip658(true)
        ));
    }

    #[tokio::test]
    async fn test_service_get_txn_receipt_falls_back_to_l1() {
        let store: Arc<dyn crate::store::Store> = Arc::new(InMemoryStore::new());
        let block_state = Arc::new(BlockState::new());
        let tx_hash = TxHash::from([9u8; 32]);
        let l1_receipt: TransactionReceipt<ReceiptEnvelope<Log>> = TransactionReceipt {
            inner: ReceiptEnvelope::Eip1559(ReceiptWithBloom::<Receipt<Log>>::default()),
            transaction_hash: tx_hash,
            transaction_index: Some(0),
            block_hash: Some(alloy::primitives::B256::from([7u8; 32])),
            block_number: Some(55),
            gas_used: 0,
            effective_gas_price: 0,
            blob_gas_used: None,
            blob_gas_price: None,
            from: Address::ZERO,
            to: None,
            contract_address: None,
        };
        let service = ServiceState::new(
            crate::MidenClient::new_test(),
            test_accounts_config(),
            1,
            1,
            store,
            block_state,
            Some(Arc::new(ReceiptStub {
                receipt: Some(l1_receipt.clone()),
            })),
            String::new(),
            String::new(),
        );

        let result = service_get_txn_receipt(service, tx_hash.to_string())
            .await
            .unwrap();
        assert_eq!(result, Some(l1_receipt));
    }
}
