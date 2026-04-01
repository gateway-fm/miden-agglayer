//! Log Synthesis - Generate synthetic EVM logs for bridge service compatibility.
//!
//! Synthesizes ClaimEvent and UpdateHashChainValue logs from Miden transactions.

use serde::{Deserialize, Serialize};

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

/// Metadata stored for each seen GER (used by zkevm_getExitRootsByGER)
#[derive(Debug, Clone)]
pub struct GerEntry {
    pub mainnet_exit_root: Option<[u8; 32]>,
    pub rollup_exit_root: Option<[u8; 32]>,
    pub block_number: u64,
    pub timestamp: u64,
}

// LogStore has been replaced by the Store trait — see src/store/mod.rs

pub fn encode_claim_event_data(
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
    fn test_event_topic_hashes() {
        use sha3::{Digest, Keccak256};

        let claim_sig = "ClaimEvent(uint256,uint32,address,address,uint256)";
        let mut hasher = Keccak256::new();
        hasher.update(claim_sig.as_bytes());
        let claim_hash = format!("0x{}", hex::encode(<[u8; 32]>::from(hasher.finalize())));
        assert_eq!(CLAIM_EVENT_TOPIC, claim_hash);

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

    // LogStore-based tests (ger dedup, hash chain, log add/query, bridge event roundtrip)
    // have been moved to src/store/memory.rs tests.
}
