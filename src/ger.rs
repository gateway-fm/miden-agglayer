use alloy::primitives::{FixedBytes, LogData};
use alloy::sol_types::SolEvent;

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

pub async fn insert_ger(params: insertGlobalExitRootCall) -> anyhow::Result<LogData> {
    let new_ger = params.root;
    let event = UpdateHashChainValue::new(new_ger, FixedBytes::default());
    let log_data = event.encode_log_data();
    Ok(log_data)
}
