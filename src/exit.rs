use crate::claim::ClaimEvent;

alloy_core::sol! {
    // https://github.com/agglayer/agglayer-contracts/blob/main/contracts/v2/PolygonZkEVMBridgeV2.sol#L125
    #[derive(Debug)]
    event BridgeEvent(
        uint8 leafType,
        uint32 originNetwork,
        address originAddress,
        uint32 destinationNetwork,
        address destinationAddress,
        uint256 amount,
        bytes metadata,
        uint32 depositCount
    );
}

const LEAF_TYPE_ASSET: u8 = 0;

pub fn bridge_event_reversing_claim(
    claim: ClaimEvent,
    chain_id: u64,
    deposit_count: u32,
) -> BridgeEvent {
    BridgeEvent {
        leafType: LEAF_TYPE_ASSET,
        originNetwork: chain_id as u32,
        originAddress: claim.destinationAddress,
        destinationNetwork: claim.originNetwork,
        destinationAddress: claim.originAddress,
        amount: claim.amount,
        metadata: Default::default(),
        depositCount: deposit_count,
    }
}
