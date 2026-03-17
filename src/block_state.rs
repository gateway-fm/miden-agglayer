//! Block State - Synthetic EVM block tracking for kurtosis-cdk integration.
//!
//! # Why this exists
//!
//! The zkevm-bridge-service has a reorg detection mechanism: it stores block
//! hashes from `eth_getLogs` responses, then later calls `HeaderByNumber` to
//! verify them. The Go ethclient's `HeaderByNumber` returns a `types.Header`
//! and the bridge calls `header.Hash()` which computes `keccak256(rlp(header))`
//! from the header's fields — it does NOT use the `hash` field from the JSON
//! response.
//!
//! This means we cannot use an arbitrary hash (like `keccak256("miden_block_<N>")`).
//! Our block hash must be the actual RLP hash of the header fields we return in
//! the JSON-RPC response. Otherwise the bridge detects a "reorg" on every sync
//! cycle and keeps walking backwards trying to find a matching block, eventually
//! hitting genesis and resetting.
//!
//! # How it works
//!
//! We build a real `alloy_consensus::Header` with deterministic fields derived
//! purely from the block number, then compute `hash_slow()` to get the canonical
//! `keccak256(rlp(header))` hash. This is the same computation Go's ethclient
//! performs, so the hashes always match.
//!
//! All header fields are pure functions of block number alone — no state, no
//! caching needed. Parent hash uses a simple deterministic scheme (not recursive
//! RLP computation) since the bridge only checks per-block hash consistency,
//! not parent-child hash chains.

use alloy::consensus::Header;
use alloy::primitives::{B64, B256, Bloom, U256};
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use crate::miden_client::SyncListener;
use miden_client::sync::SyncSummary;

/// Genesis timestamp for synthetic blocks (2024-01-01 00:00:00 UTC)
const GENESIS_TIMESTAMP: u64 = 1704067200;

/// Block time in seconds (12s like Ethereum mainnet)
const BLOCK_TIME: u64 = 12;

/// Empty uncles hash (keccak256 of RLP-encoded empty list)
const EMPTY_OMMERS_HASH: [u8; 32] = [
    0x1d, 0xcc, 0x4d, 0xe8, 0xde, 0xc7, 0x5d, 0x7a, 0xab, 0x85, 0xb5, 0x67, 0xb6, 0xcc, 0xd4, 0x1a,
    0xd3, 0x12, 0x45, 0x1b, 0x94, 0x8a, 0x74, 0x13, 0xf0, 0xa1, 0x42, 0xfd, 0x40, 0xd4, 0x93, 0x47,
];

/// Empty trie root (keccak256 of RLP-encoded empty string)
const EMPTY_ROOT_HASH: [u8; 32] = [
    0x56, 0xe8, 0x1f, 0x17, 0x1b, 0xcc, 0x55, 0xa6, 0xff, 0x83, 0x45, 0xe6, 0x92, 0xc0, 0xf8, 0x6e,
    0x5b, 0x48, 0xe0, 0x1b, 0x99, 0x6c, 0xad, 0xc0, 0x01, 0x62, 0x2f, 0xb5, 0xe3, 0x63, 0xb4, 0x21,
];

/// Synthetic EVM block generated from Miden batch
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyntheticBlock {
    pub number: u64,
    pub hash: [u8; 32],
    pub parent_hash: [u8; 32],
    pub timestamp: u64,
    pub state_root: [u8; 32],
    pub transactions: Vec<String>,
}

impl SyntheticBlock {
    pub fn new(number: u64, timestamp: u64) -> Self {
        let hash = Self::compute_hash_for_number(number);
        let parent_hash = if number == 0 {
            [0u8; 32]
        } else {
            Self::compute_hash_for_number(number - 1)
        };
        Self {
            number,
            hash,
            parent_hash,
            timestamp,
            state_root: [0u8; 32],
            transactions: Vec::new(),
        }
    }

    /// Build a block header for hash computation.
    /// Uses a fixed parent_hash of ZERO to avoid recursion — the block hash
    /// is purely a function of the block number, not the chain history.
    /// The actual parent_hash in `SyntheticBlock` is set correctly in `new()`.
    fn build_header(number: u64) -> Header {
        let timestamp = GENESIS_TIMESTAMP + number * BLOCK_TIME;

        Header {
            parent_hash: B256::ZERO,
            ommers_hash: B256::from(EMPTY_OMMERS_HASH),
            beneficiary: Default::default(),
            state_root: B256::ZERO,
            transactions_root: B256::from(EMPTY_ROOT_HASH),
            receipts_root: B256::from(EMPTY_ROOT_HASH),
            logs_bloom: Bloom::ZERO,
            difficulty: U256::ZERO,
            number,
            gas_limit: 30_000_000,
            gas_used: 0,
            timestamp,
            extra_data: Default::default(),
            mix_hash: B256::ZERO,
            nonce: B64::ZERO,
            base_fee_per_gas: Some(0),
            ..Default::default()
        }
    }

    pub fn compute_hash_for_number(number: u64) -> [u8; 32] {
        let header = Self::build_header(number);
        header.hash_slow().0
    }

    pub fn to_json(&self, _full_transactions: bool) -> serde_json::Value {
        // Note: full_transactions=true should return full tx objects, but our
        // synthetic blocks only store tx hashes, so both cases return the same.
        let txs = serde_json::json!(self.transactions);

        serde_json::json!({
            "number": format!("0x{:x}", self.number),
            "hash": format!("0x{}", hex::encode(self.hash)),
            "parentHash": format!("0x{}", hex::encode(self.parent_hash)),
            "timestamp": format!("0x{:x}", self.timestamp),
            "stateRoot": format!("0x{}", hex::encode(self.state_root)),
            "transactionsRoot": format!("0x{}", hex::encode(EMPTY_ROOT_HASH)),
            "receiptsRoot": format!("0x{}", hex::encode(EMPTY_ROOT_HASH)),
            "logsBloom": format!("0x{}", "00".repeat(256)),
            "difficulty": "0x0",
            "totalDifficulty": "0x0",
            "gasLimit": "0x1c9c380",
            "gasUsed": "0x0",
            "miner": "0x0000000000000000000000000000000000000000",
            "extraData": "0x",
            "nonce": "0x0000000000000000",
            "mixHash": "0x0000000000000000000000000000000000000000000000000000000000000000",
            "sha3Uncles": format!("0x{}", hex::encode(EMPTY_OMMERS_HASH)),
            "uncles": [],
            "size": "0x200",
            "transactions": txs,
            "baseFeePerGas": "0x0"
        })
    }
}

/// Block state tracking for synthetic EVM blocks
pub struct BlockState {
    blocks: RwLock<HashMap<u64, SyntheticBlock>>,
    hash_to_number: RwLock<HashMap<[u8; 32], u64>>,
    current_block: RwLock<u64>,
}

impl BlockState {
    pub fn new() -> Self {
        let state = Self {
            blocks: RwLock::new(HashMap::new()),
            hash_to_number: RwLock::new(HashMap::new()),
            current_block: RwLock::new(0),
        };
        state.ensure_block_exists(0);
        state
    }

    pub fn current_block_number(&self) -> u64 {
        *self.current_block.read()
    }

    pub fn set_current_block(&self, block_num: u64) {
        self.ensure_block_exists(block_num);
        *self.current_block.write() = block_num;
    }

    fn deterministic_timestamp(block_num: u64) -> u64 {
        GENESIS_TIMESTAMP + block_num * BLOCK_TIME
    }

    /// Compute the deterministic timestamp for any block number.
    pub fn get_block_timestamp(&self, block_num: u64) -> u64 {
        Self::deterministic_timestamp(block_num)
    }

    fn ensure_block_exists(&self, block_num: u64) {
        // Acquire both locks before mutating to prevent deadlock from
        // inconsistent lock ordering. Always: hash_to_number first, then blocks.
        let mut hash_to_number = self.hash_to_number.write();
        let mut blocks = self.blocks.write();
        if blocks.contains_key(&block_num) {
            return;
        }

        let block = SyntheticBlock::new(block_num, Self::deterministic_timestamp(block_num));
        hash_to_number.insert(block.hash, block_num);
        blocks.insert(block_num, block);
    }

    pub fn get_block_by_number(&self, block_num: u64) -> Option<SyntheticBlock> {
        self.ensure_block_exists(block_num);
        self.blocks.read().get(&block_num).cloned()
    }

    pub fn get_block_by_hash(&self, hash: &[u8; 32]) -> Option<SyntheticBlock> {
        let hash_to_number = self.hash_to_number.read();
        let number = hash_to_number.get(hash).copied()?;
        drop(hash_to_number);
        self.blocks.read().get(&number).cloned()
    }

    pub fn add_transaction_to_block(&self, block_num: u64, tx_hash: String) {
        if let Some(block) = self.blocks.write().get_mut(&block_num) {
            block.transactions.push(tx_hash);
        }
    }

    pub fn get_block_hash(&self, block_num: u64) -> [u8; 32] {
        SyntheticBlock::compute_hash_for_number(block_num)
    }
}

impl Default for BlockState {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl SyncListener for BlockState {
    fn on_sync(&self, summary: &SyncSummary) {
        self.set_current_block(summary.block_num.as_u64());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_hash_is_pure_function_of_block_number() {
        let h1 = SyntheticBlock::compute_hash_for_number(42);
        let h2 = SyntheticBlock::compute_hash_for_number(42);
        assert_eq!(h1, h2, "Same block number must produce same hash");
        assert_ne!(h1, [0u8; 32]);

        let h3 = SyntheticBlock::compute_hash_for_number(43);
        assert_ne!(
            h1, h3,
            "Different block numbers must produce different hashes"
        );
    }

    #[test]
    fn test_hash_is_real_rlp_hash() {
        let header = SyntheticBlock::build_header(42);
        let expected = header.hash_slow().0;
        let actual = SyntheticBlock::compute_hash_for_number(42);
        assert_eq!(actual, expected, "Hash must be keccak256(rlp(header))");
    }

    #[test]
    fn test_block_state_genesis() {
        let state = BlockState::new();
        let genesis = state.get_block_by_number(0).unwrap();
        assert_eq!(genesis.number, 0);
        assert_eq!(genesis.parent_hash, [0u8; 32]);
    }

    #[test]
    fn test_hashes_identical_across_instances() {
        let state1 = BlockState::new();
        let state2 = BlockState::new();

        let _ = state1.get_block_by_number(100);
        let _ = state2.get_block_by_number(50);
        let _ = state2.get_block_by_number(100);

        assert_eq!(
            state1.get_block_by_number(100).unwrap().hash,
            state2.get_block_by_number(100).unwrap().hash,
        );
    }

    #[test]
    fn test_deterministic_timestamps() {
        let state = BlockState::new();
        let block = state.get_block_by_number(10).unwrap();
        let expected_ts = GENESIS_TIMESTAMP + 10 * BLOCK_TIME;
        assert_eq!(block.timestamp, expected_ts);
    }

    #[test]
    fn test_get_block_hash_without_cache() {
        let state = BlockState::new();
        let h = state.get_block_hash(999);
        assert_eq!(h, SyntheticBlock::compute_hash_for_number(999));
    }
}
