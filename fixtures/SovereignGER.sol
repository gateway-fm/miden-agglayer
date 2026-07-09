// SPDX-License-Identifier: MIT
pragma solidity ^0.8.20;

/// Minimal GlobalExitRootManagerL2SovereignChain-compatible contract for the
/// L2B chain in the L2->L2 e2e (task #25). ABI-faithful to the parts the
/// bridge (PolygonZkEVMBridgeV2), aggkit (aggoracle inject + L2GERSync) and
/// claimAsset verification actually touch:
///   - updateExitRoot(bytes32)            [bridge-only, on deposit]
///   - insertGlobalExitRoot(bytes32)      [updater-only, aggoracle L1->L2 GER]
///   - removeGlobalExitRoots(bytes32[])   [updater-only]
///   - globalExitRootMap(bytes32) view    [claimAsset GER-existence check]
///   - lastRollupExitRoot() view
/// Deployed via anvil_setCode at the sovereign convention address
/// 0xa40D5f56745a118D0906a34E69aeC8C0Db1cB8fA (no constructor — call
/// initialize(bridge, updater) once after setCode).
contract SovereignGER {
    // slot 0 / 1 / 2 — plain storage (no immutables: setCode-deployed)
    address public bridgeAddress;
    address public globalExitRootUpdater;
    bytes32 public lastRollupExitRoot;
    // matches GlobalExitRootManagerL2SovereignChain: value = insertion counter
    mapping(bytes32 => uint256) public globalExitRootMap;
    uint256 public insertedGERCount;

    event InsertGlobalExitRoot(bytes32 indexed newGlobalExitRoot);
    event RemoveLastGlobalExitRoot(bytes32 indexed removedGlobalExitRoot);
    event UpdateRemovalGlobalExitRoot(bytes32 indexed removedGlobalExitRoot);

    function initialize(address _bridge, address _updater) external {
        require(bridgeAddress == address(0), "already initialized");
        require(_bridge != address(0) && _updater != address(0), "zero addr");
        bridgeAddress = _bridge;
        globalExitRootUpdater = _updater;
    }

    /// Called by the bridge after every deposit (BridgeV2 does
    /// globalExitRootManager.updateExitRoot(getRoot())).
    function updateExitRoot(bytes32 newRoot) external {
        require(msg.sender == bridgeAddress, "only bridge");
        lastRollupExitRoot = newRoot;
    }

    /// Called by the aggoracle (L1->L2 GER injection).
    function insertGlobalExitRoot(bytes32 _newRoot) external {
        require(msg.sender == globalExitRootUpdater, "only updater");
        require(globalExitRootMap[_newRoot] == 0, "GER already set");
        globalExitRootMap[_newRoot] = ++insertedGERCount;
        emit InsertGlobalExitRoot(_newRoot);
    }

    function removeGlobalExitRoots(bytes32[] calldata gersToRemove) external {
        require(msg.sender == globalExitRootUpdater, "only updater");
        for (uint256 i = 0; i < gersToRemove.length; i++) {
            bytes32 ger = gersToRemove[i];
            require(globalExitRootMap[ger] == insertedGERCount, "only last GER");
            insertedGERCount--;
            delete globalExitRootMap[ger];
            emit RemoveLastGlobalExitRoot(ger);
        }
    }
}
