//! Log Synthesis - Generate synthetic EVM logs for bridge service compatibility.
//!
//! Synthesizes ClaimEvent and UpdateHashChainValue logs from Miden transactions.

use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use sha3::{Digest, Keccak256};
use std::collections::HashMap;

/// ClaimEvent topic hash: keccak256("ClaimEvent(uint256,uint32,address,address,uint256)")
pub const CLAIM_EVENT_TOPIC: &str =
    "0x1df3f2a973a00d6635911755c260704e95e8a5876997546798770f76396fda4d";

/// BridgeEvent topic hash: keccak256("BridgeEvent(uint8,uint32,address,uint32,address,uint256,bytes,uint32)")
pub const BRIDGE_EVENT_TOPIC: &str =
    "0x501781209a1f8899323b96b4ef08b168df93e0a90c673d1e4cce39366cb62f9b";

/// UpdateHashChainValue topic hash: keccak256("UpdateHashChainValue(bytes32,bytes32)")
/// Emitted by L2 GlobalExitRootManagerL2SovereignChain when a GER is inserted
pub const UPDATE_HASH_CHAIN_VALUE_TOPIC: &str =
    "0x65d3bf36615f1f02a134d12dfa9ea6b1d4a52386e825973cd27ddb70895c2319";

/// L2 GlobalExitRoot contract address (receives GER updates from aggoracle)
pub const L2_GLOBAL_EXIT_ROOT_ADDRESS: &str = "0xa40D5f56745a118D0906a34E69aeC8C0Db1cB8fA";

/// Synthetic log entry for eth_getLogs
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyntheticLog {
    pub address: String,
    pub topics: Vec<String>,
    pub data: String,
    pub block_number: u64,
    pub block_hash: [u8; 32],
    pub transaction_hash: String,
    pub transaction_index: u64,
    pub log_index: u64,
    pub removed: bool,
}

impl SyntheticLog {
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "address": self.address,
            "topics": self.topics,
            "data": self.data,
            "blockNumber": format!("0x{:x}", self.block_number),
            "blockHash": format!("0x{}", hex::encode(self.block_hash)),
            "transactionHash": self.transaction_hash,
            "transactionIndex": format!("0x{:x}", self.transaction_index),
            "logIndex": format!("0x{:x}", self.log_index),
            "removed": self.removed
        })
    }
}

/// Log filter for eth_getLogs
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LogFilter {
    pub from_block: Option<String>,
    pub to_block: Option<String>,
    pub address: Option<AddressFilter>,
    pub topics: Option<Vec<Option<TopicFilter>>>,
    pub block_hash: Option<String>,
}

/// Address filter can be single or array
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum AddressFilter {
    Single(String),
    Multiple(Vec<String>),
}

/// Topic filter can be single or array (OR matching)
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum TopicFilter {
    Single(String),
    Multiple(Vec<String>),
}

fn parse_block_tag(s: &str, current_block: u64) -> u64 {
    match s.to_lowercase().as_str() {
        "earliest" => 0,
        "latest" | "pending" => current_block,
        hex if hex.starts_with("0x") => u64::from_str_radix(&hex[2..], 16).unwrap_or(current_block),
        decimal => decimal.parse::<u64>().unwrap_or(current_block),
    }
}

impl LogFilter {
    pub fn from_block_number(&self, current_block: u64) -> u64 {
        self.from_block
            .as_ref()
            .map(|s| parse_block_tag(s, current_block))
            .unwrap_or(current_block)
    }

    pub fn to_block_number(&self, current_block: u64) -> u64 {
        self.to_block
            .as_ref()
            .map(|s| parse_block_tag(s, current_block))
            .unwrap_or(current_block)
    }

    /// Check if the query's topic filter explicitly includes the given topic.
    fn query_includes_topic(&self, topic: Option<&str>) -> bool {
        let Some(topic) = topic else {
            return false;
        };
        let Some(ref topic_filters) = self.topics else {
            return false;
        };
        // Check if topic0 filter includes this topic
        if let Some(Some(filter)) = topic_filters.first() {
            let topic_lower = topic.to_lowercase();
            return match filter {
                TopicFilter::Single(t) => t.to_lowercase() == topic_lower,
                TopicFilter::Multiple(topics) => {
                    topics.iter().any(|t| t.to_lowercase() == topic_lower)
                }
            };
        }
        false
    }

    pub fn matches(&self, log: &SyntheticLog, current_block: u64) -> bool {
        if let Some(ref block_hash) = self.block_hash {
            let log_hash = format!("0x{}", hex::encode(log.block_hash));
            if log_hash.to_lowercase() != block_hash.to_lowercase() {
                return false;
            }
        } else {
            let from = self.from_block_number(current_block);
            let to = self.to_block_number(current_block);
            if log.block_number < from || log.block_number > to {
                return false;
            }
        }

        if let Some(ref addr_filter) = self.address {
            let log_addr = log.address.to_lowercase();
            let matches_addr = match addr_filter {
                AddressFilter::Single(a) => a.to_lowercase() == log_addr,
                AddressFilter::Multiple(addrs) => {
                    addrs.iter().any(|a| a.to_lowercase() == log_addr)
                }
            };

            // SPECIAL CASE: The bridge-service filters logs by the Bridge contract address.
            // However, it ALSO needs UpdateHashChainValue logs which are emitted by the
            // GlobalExitRoot contract. If the query includes a topic filter that
            // explicitly requests a passthrough topic, we allow it through even if
            // the address doesn't match.
            //
            // Without the topic filter guard, queries by address only (like aggkit's
            // L2BridgeSyncer) would receive UpdateHashChainValue logs that they can't
            // decode, causing "input too short" errors.
            let is_passthrough = if !matches_addr {
                let topic0 = log.topics.first().map(|t| t.to_lowercase());
                let is_passthrough_topic = topic0
                    .as_ref()
                    .map(|t| {
                        t == &UPDATE_HASH_CHAIN_VALUE_TOPIC.to_lowercase()
                            || t == &BRIDGE_EVENT_TOPIC.to_lowercase()
                    })
                    .unwrap_or(false);

                // Only passthrough if the query's topic filter explicitly includes
                // this log's topic. This prevents leaking GER logs to consumers
                // that query by address only.
                is_passthrough_topic && self.query_includes_topic(topic0.as_deref())
            } else {
                false // address matches, no passthrough needed
            };

            if !matches_addr && !is_passthrough {
                return false;
            }
        }

        if let Some(ref topic_filters) = self.topics {
            for (i, topic_filter) in topic_filters.iter().enumerate() {
                if let Some(filter) = topic_filter {
                    if i >= log.topics.len() {
                        return false;
                    }
                    let log_topic = log.topics[i].to_lowercase();
                    let matches_topic = match filter {
                        TopicFilter::Single(t) => t.to_lowercase() == log_topic,
                        TopicFilter::Multiple(topics) => {
                            topics.iter().any(|t| t.to_lowercase() == log_topic)
                        }
                    };
                    if !matches_topic {
                        return false;
                    }
                }
            }
        }

        true
    }
}

/// Log store for synthetic logs
pub struct LogStore {
    logs_by_block: RwLock<HashMap<u64, Vec<SyntheticLog>>>,
    logs_by_tx: RwLock<HashMap<String, Vec<SyntheticLog>>>,
    log_counter: RwLock<u64>,
    seen_gers: RwLock<HashMap<[u8; 32], u64>>,
    hash_chain_value: RwLock<[u8; 32]>,
    pending_events: RwLock<Vec<SyntheticLog>>,
}

impl LogStore {
    pub fn new() -> Self {
        Self {
            logs_by_block: RwLock::new(HashMap::new()),
            logs_by_tx: RwLock::new(HashMap::new()),
            log_counter: RwLock::new(0),
            seen_gers: RwLock::new(HashMap::new()),
            hash_chain_value: RwLock::new([0u8; 32]),
            pending_events: RwLock::new(Vec::new()),
        }
    }

    pub fn has_seen_ger(&self, ger: &[u8; 32]) -> bool {
        self.seen_gers.read().contains_key(ger)
    }

    pub fn mark_ger_seen(&self, ger: &[u8; 32], block_number: u64) -> bool {
        let mut seen = self.seen_gers.write();
        if seen.contains_key(ger) {
            false
        } else {
            seen.insert(*ger, block_number);
            true
        }
    }

    pub fn add_log(&self, mut log: SyntheticLog) {
        let mut counter = self.log_counter.write();
        log.log_index = *counter;
        *counter += 1;

        let block_num = log.block_number;
        // Normalize tx hash key to lowercase for case-insensitive lookup
        let tx_hash = log.transaction_hash.to_lowercase();

        tracing::debug!(
            tx_hash = %tx_hash,
            block_number = block_num,
            topic0 = log.topics.first().map(|t| &t[..20.min(t.len())]).unwrap_or("none"),
            "LogStore: storing log"
        );

        self.logs_by_block
            .write()
            .entry(block_num)
            .or_default()
            .push(log.clone());

        self.logs_by_tx
            .write()
            .entry(tx_hash)
            .or_default()
            .push(log.clone());

        self.pending_events.write().push(log);
    }

    #[allow(clippy::too_many_arguments)]
    pub fn add_claim_event(
        &self,
        bridge_address: &str,
        block_number: u64,
        block_hash: [u8; 32],
        tx_hash: &str,
        global_index: &[u8; 32],
        origin_network: u32,
        origin_address: &[u8; 20],
        destination_address: &[u8; 20],
        amount: u64,
    ) {
        let log = SyntheticLog {
            address: bridge_address.to_string(),
            topics: vec![CLAIM_EVENT_TOPIC.to_string()],
            data: encode_claim_event_data(
                global_index,
                origin_network,
                origin_address,
                destination_address,
                amount,
            ),
            block_number,
            block_hash,
            transaction_hash: tx_hash.to_string(),
            transaction_index: 0,
            log_index: 0,
            removed: false,
        };
        self.add_log(log);
    }

    /// Record an UpdateHashChainValue log for a GER injection.
    /// Caller is responsible for dedup (check `has_seen_ger` first).
    pub fn add_ger_update_event(
        &self,
        block_number: u64,
        block_hash: [u8; 32],
        tx_hash: &str,
        global_exit_root: &[u8; 32],
    ) {
        self.mark_ger_seen(global_exit_root, block_number);

        let new_hash_chain = {
            let mut hash_chain = self.hash_chain_value.write();
            let mut hasher = Keccak256::new();
            hasher.update(*hash_chain);
            hasher.update(global_exit_root);
            let result: [u8; 32] = hasher.finalize().into();
            *hash_chain = result;
            result
        };

        let log = SyntheticLog {
            address: L2_GLOBAL_EXIT_ROOT_ADDRESS.to_string(),
            topics: vec![
                UPDATE_HASH_CHAIN_VALUE_TOPIC.to_string(),
                format!("0x{}", hex::encode(global_exit_root)),
                format!("0x{}", hex::encode(new_hash_chain)),
            ],
            data: "0x".to_string(),
            block_number,
            block_hash,
            transaction_hash: tx_hash.to_string(),
            transaction_index: 0,
            log_index: 0,
            removed: false,
        };
        self.add_log(log);
    }

    /// Record a BridgeEvent log for a bridge-out (L2 → L1) deposit.
    #[allow(clippy::too_many_arguments)]
    pub fn add_bridge_event(
        &self,
        bridge_address: &str,
        block_number: u64,
        block_hash: [u8; 32],
        tx_hash: &str,
        leaf_type: u8,
        origin_network: u32,
        origin_address: &[u8; 20],
        destination_network: u32,
        destination_address: &[u8; 20],
        amount: u128,
        deposit_count: u32,
    ) {
        let log = SyntheticLog {
            address: bridge_address.to_string(),
            topics: vec![BRIDGE_EVENT_TOPIC.to_string()],
            data: crate::bridge_out::encode_bridge_event_data(
                leaf_type,
                origin_network,
                origin_address,
                destination_network,
                destination_address,
                amount,
                deposit_count,
            ),
            block_number,
            block_hash,
            transaction_hash: tx_hash.to_string(),
            transaction_index: 0,
            log_index: 0,
            removed: false,
        };
        self.add_log(log);
    }

    pub fn get_logs(&self, filter: &LogFilter, current_block: u64) -> Vec<SyntheticLog> {
        let mut result = Vec::new();

        let from = filter.from_block_number(current_block);
        let to = filter.to_block_number(current_block);

        // Drain pending events: events in-range are already in logs_by_block,
        // events too old are returned at their original block (the normal scan
        // won't find them, so we include them directly), future events stay pending.
        {
            let mut pending = self.pending_events.write();
            let mut remaining = Vec::new();
            for evt in pending.drain(..) {
                if evt.block_number <= to {
                    // In range or older — already in logs_by_block, will be found by scan.
                    // If older than `from`, the scan still covers it via logs_by_block
                    // since add_log() stored it at the original block_number.
                } else {
                    // Future event — keep pending for next query.
                    remaining.push(evt);
                }
            }
            *pending = remaining;
        }

        // Normal block-range scan
        let logs_by_block = self.logs_by_block.read();
        for block_num in from..=to {
            if let Some(logs) = logs_by_block.get(&block_num) {
                for log in logs {
                    if filter.matches(log, current_block) {
                        result.push(log.clone());
                    }
                }
            }
            if result.len() >= 1000 {
                break;
            }
        }

        result
    }

    /// Return all stored tx hash keys (for diagnostics).
    pub fn logs_by_tx_keys(&self) -> Vec<String> {
        self.logs_by_tx.read().keys().cloned().collect()
    }

    pub fn get_logs_for_tx(&self, tx_hash: &str) -> Vec<SyntheticLog> {
        let key = tx_hash.to_lowercase();
        let map = self.logs_by_tx.read();
        let result = map.get(&key).cloned().unwrap_or_default();
        if result.is_empty() {
            let stored_keys: Vec<&String> = map.keys().collect();
            tracing::debug!(
                lookup_key = %key,
                stored_count = stored_keys.len(),
                stored_keys = ?stored_keys.iter().take(10).collect::<Vec<_>>(),
                "LogStore: get_logs_for_tx miss"
            );
        }
        result
    }
}

impl Default for LogStore {
    fn default() -> Self {
        Self::new()
    }
}

fn encode_claim_event_data(
    global_index: &[u8; 32],
    origin_network: u32,
    origin_address: &[u8; 20],
    destination_address: &[u8; 20],
    amount: u64,
) -> String {
    let mut data = Vec::with_capacity(160);

    // globalIndex (uint256)
    data.extend_from_slice(global_index);

    // originNetwork (uint32 padded to 32 bytes)
    data.extend_from_slice(&[0u8; 28]);
    data.extend_from_slice(&origin_network.to_be_bytes());

    // originAddress (address padded to 32 bytes)
    data.extend_from_slice(&[0u8; 12]);
    data.extend_from_slice(origin_address);

    // destinationAddress (address padded to 32 bytes)
    data.extend_from_slice(&[0u8; 12]);
    data.extend_from_slice(destination_address);

    // amount (uint256)
    data.extend_from_slice(&[0u8; 24]);
    data.extend_from_slice(&amount.to_be_bytes());

    format!("0x{}", hex::encode(data))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_log_filter_block_range() {
        let filter = LogFilter {
            from_block: Some("0x10".to_string()),
            to_block: Some("0x20".to_string()),
            ..Default::default()
        };

        assert_eq!(filter.from_block_number(100), 16);
        assert_eq!(filter.to_block_number(100), 32);
    }

    #[test]
    fn test_log_filter_latest() {
        let filter = LogFilter {
            from_block: Some("latest".to_string()),
            to_block: Some("latest".to_string()),
            ..Default::default()
        };

        assert_eq!(filter.from_block_number(500), 500);
        assert_eq!(filter.to_block_number(500), 500);
    }

    #[test]
    fn test_log_filter_topic_match() {
        let log = SyntheticLog {
            address: "0x1234".to_string(),
            topics: vec![CLAIM_EVENT_TOPIC.to_string()],
            data: "0x".to_string(),
            block_number: 100,
            block_hash: [0u8; 32],
            transaction_hash: "0xabc".to_string(),
            transaction_index: 0,
            log_index: 0,
            removed: false,
        };

        let filter = LogFilter {
            from_block: Some("0x0".to_string()),
            to_block: Some("0x200".to_string()),
            topics: Some(vec![Some(TopicFilter::Single(
                CLAIM_EVENT_TOPIC.to_string(),
            ))]),
            ..Default::default()
        };

        assert!(filter.matches(&log, 500));
    }

    #[test]
    fn test_ger_dedup_tracking() {
        let store = LogStore::new();
        let ger = [0x11; 32];

        assert!(!store.has_seen_ger(&ger));
        store.add_ger_update_event(0, [0u8; 32], "0xTx1", &ger);
        assert!(store.has_seen_ger(&ger), "GER should be marked as seen");

        let filter = LogFilter {
            from_block: Some("0x0".to_string()),
            to_block: Some("0x100".to_string()),
            ..Default::default()
        };
        let logs = store.get_logs(&filter, 100);
        assert_eq!(logs.len(), 1);
    }

    #[test]
    fn test_hash_chain_incremental() {
        let store = LogStore::new();

        let ger1 = [0x11; 32];
        let ger2 = [0x22; 32];

        store.add_ger_update_event(0, [0u8; 32], "0xTx1", &ger1);
        let hash1 = *store.hash_chain_value.read();

        store.add_ger_update_event(1, [1u8; 32], "0xTx2", &ger2);
        let hash2 = *store.hash_chain_value.read();

        // hash1 = keccak256([0u8;32] || ger1)
        let mut hasher = Keccak256::new();
        hasher.update([0u8; 32]);
        hasher.update(ger1);
        let expected1: [u8; 32] = hasher.finalize().into();
        assert_eq!(hash1, expected1);

        // hash2 = keccak256(hash1 || ger2) — must chain from hash1, not from zero
        let mut hasher = Keccak256::new();
        hasher.update(expected1);
        hasher.update(ger2);
        let expected2: [u8; 32] = hasher.finalize().into();
        assert_eq!(hash2, expected2);
        assert_ne!(hash1, hash2);
    }

    #[test]
    fn test_log_store_add_and_query() {
        let store = LogStore::new();

        store.add_claim_event(
            "0xBridge",
            100,
            [0xAA; 32],
            "0xTxHash",
            &[0x11; 32],
            1,
            &[0x22; 20],
            &[0x33; 20],
            1000,
        );

        let filter = LogFilter {
            from_block: Some("0x0".to_string()),
            to_block: Some("0x200".to_string()),
            ..Default::default()
        };

        let logs = store.get_logs(&filter, 500);
        assert_eq!(logs.len(), 1);
        assert_eq!(logs[0].block_number, 100);
    }

    #[test]
    fn test_bridge_event_roundtrip_lookup() {
        // Simulates the exact flow: BridgeOutScanner stores a BridgeEvent,
        // then eth_getTransactionByHash looks it up via get_logs_for_tx.
        use alloy::primitives::TxHash;
        use std::str::FromStr;

        let store = LogStore::new();

        // Step 1: Simulate BridgeOutScanner creating a synthetic tx hash
        let note_id_str = "0x1234abcd5678ef90";
        let tx_hash = {
            let mut hasher = Keccak256::new();
            hasher.update(b"miden-bridge-out-");
            hasher.update(note_id_str.as_bytes());
            let hash: [u8; 32] = hasher.finalize().into();
            format!("0x{}", hex::encode(hash))
        };

        // Step 2: Store the BridgeEvent (as BridgeOutScanner does)
        store.add_bridge_event(
            "0xc8cbebf950b9df44d987c8619f092bea980ff038",
            100,
            [0xBB; 32],
            &tx_hash,
            0,
            0,
            &[0u8; 20],
            1,
            &[0x42; 20],
            1000,
            0,
        );

        // Step 3: Verify eth_getLogs returns the event with the correct transactionHash
        let filter = LogFilter {
            from_block: Some("0x0".to_string()),
            to_block: Some("0x200".to_string()),
            ..Default::default()
        };
        let logs = store.get_logs(&filter, 500);
        assert_eq!(logs.len(), 1, "BridgeEvent should appear in eth_getLogs");
        let log_tx_hash = &logs[0].transaction_hash;

        // Step 4: Simulate eth_getTransactionByHash lookup
        // The service parses the hash from the RPC param, then formats it back
        let parsed_hash = TxHash::from_str(log_tx_hash).expect("should parse tx hash from log");
        let lookup_key = format!("{parsed_hash:#x}");
        let found = store.get_logs_for_tx(&lookup_key);
        assert!(
            !found.is_empty(),
            "get_logs_for_tx should find the log. stored key from add_bridge_event: {tx_hash}, lookup key from TxHash round-trip: {lookup_key}"
        );
        assert_eq!(found[0].block_number, 100);
    }

    #[test]
    fn test_event_topic_hashes() {
        // Verify topic constants match keccak256 of event signatures
        let claim_sig = "ClaimEvent(uint256,uint32,address,address,uint256)";
        let mut hasher = Keccak256::new();
        hasher.update(claim_sig.as_bytes());
        let claim_hash = format!("0x{}", hex::encode(<[u8; 32]>::from(hasher.finalize())));
        assert_eq!(CLAIM_EVENT_TOPIC, claim_hash);

        // Cross-check with alloy's sol! macro
        use crate::claim::ClaimEvent;
        use alloy_core::sol_types::SolEvent;
        assert_eq!(
            CLAIM_EVENT_TOPIC,
            format!("{:#x}", ClaimEvent::SIGNATURE_HASH)
        );

        let bridge_sig = "BridgeEvent(uint8,uint32,address,uint32,address,uint256,bytes,uint32)";
        let mut hasher2 = Keccak256::new();
        hasher2.update(bridge_sig.as_bytes());
        let bridge_hash = format!("0x{}", hex::encode(<[u8; 32]>::from(hasher2.finalize())));
        assert_eq!(BRIDGE_EVENT_TOPIC, bridge_hash);
    }
}
