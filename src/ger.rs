use alloy::primitives::{BlockNumber, FixedBytes, TxHash};
use std::sync;
use sync::Mutex;

alloy_core::sol! {
    // https://github.com/agglayer/agglayer-contracts/blob/main/contracts/v2/sovereignChains/GlobalExitRootManagerL2SovereignChain.sol#L166
    #[derive(Debug)]
    function insertGlobalExitRoot(bytes32 root);
}

alloy_core::sol! {
    // https://github.com/agglayer/agglayer-contracts/blob/main/contracts/v2/sovereignChains/GlobalExitRootManagerL2SovereignChain.sol#L52
    #[derive(Debug)]
    event UpdateHashChainValue(
        bytes32 indexed newGlobalExitRoot,
        bytes32 indexed newHashChainValue
    );
}

impl UpdateHashChainValue {
    fn new(ger: FixedBytes<32>, chain_hash: FixedBytes<32>) -> Self {
        UpdateHashChainValue {
            newGlobalExitRoot: ger,
            newHashChainValue: chain_hash,
        }
    }
}

static LATEST_GER: Mutex<Option<(FixedBytes<32>, TxHash, BlockNumber)>> = Mutex::new(None);

pub async fn insert_ger(
    params: insertGlobalExitRootCall,
    txn_hash: TxHash,
    block_num: BlockNumber,
) -> anyhow::Result<()> {
    let new_ger = params.root;
    let mut latest_ger = LATEST_GER.lock().unwrap();
    *latest_ger = Some((new_ger, txn_hash, block_num));
    Ok(())
}

pub fn latest_ger_update_event() -> Option<(UpdateHashChainValue, TxHash, BlockNumber)> {
    let latest_ger_guard = LATEST_GER.lock().unwrap();
    let latest_ger = (*latest_ger_guard)?;
    let event = UpdateHashChainValue::new(latest_ger.0, FixedBytes::default());
    Some((event, latest_ger.1, latest_ger.2))
}
