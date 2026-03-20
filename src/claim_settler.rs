//! ClaimSettler — background task that auto-claims settled L2→L1 deposits on L1.
//!
//! Polls the bridge-service REST API for deposits targeting watched addresses,
//! fetches Merkle proofs, builds `claimAsset` calldata, signs and sends to L1.

use crate::claim::claimAssetCall;
use alloy::primitives::{Address, FixedBytes, U256};
use alloy::providers::{Provider, ProviderBuilder};
use alloy::signers::local::PrivateKeySigner;
use alloy::sol_types::SolCall;
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::PathBuf;
use std::time::Duration;

const POLL_INTERVAL: Duration = Duration::from_secs(10);
const STARTUP_DELAY: Duration = Duration::from_secs(30);

/// Tracks which deposit_cnt values have already been claimed to avoid duplication.
struct ClaimSettlerTracker {
    claimed_deposits: RwLock<HashSet<u64>>,
    persistence_path: Option<PathBuf>,
}

#[derive(Serialize, Deserialize)]
struct SettlerState {
    claimed_deposits: Vec<u64>,
}

impl ClaimSettlerTracker {
    fn new(persistence_path: Option<PathBuf>) -> anyhow::Result<Self> {
        let claimed_deposits = if let Some(ref path) = persistence_path {
            if path.exists() {
                let data = std::fs::read_to_string(path)?;
                let state: SettlerState = serde_json::from_str(&data)?;
                state.claimed_deposits.into_iter().collect()
            } else {
                HashSet::new()
            }
        } else {
            HashSet::new()
        };
        Ok(Self {
            claimed_deposits: RwLock::new(claimed_deposits),
            persistence_path,
        })
    }

    fn is_claimed(&self, deposit_cnt: u64) -> bool {
        self.claimed_deposits.read().contains(&deposit_cnt)
    }

    fn mark_claimed(&self, deposit_cnt: u64) {
        self.claimed_deposits.write().insert(deposit_cnt);
        self.persist();
    }

    fn persist(&self) {
        let Some(ref path) = self.persistence_path else {
            return;
        };
        let deposits = self.claimed_deposits.read();
        let state = SettlerState {
            claimed_deposits: deposits.iter().copied().collect(),
        };
        drop(deposits);
        let Ok(data) = serde_json::to_string_pretty(&state) else {
            tracing::error!("ClaimSettlerTracker: failed to serialize state");
            return;
        };
        let tmp_path = path.with_extension("tmp");
        if let Err(e) = std::fs::write(&tmp_path, &data) {
            tracing::error!("ClaimSettlerTracker: write failed: {e}");
            return;
        }
        if let Err(e) = std::fs::rename(&tmp_path, path) {
            tracing::error!("ClaimSettlerTracker: rename failed: {e}");
        }
    }
}

// Bridge-service API response types

#[derive(Debug, Deserialize)]
struct BridgesResponse {
    deposits: Vec<Deposit>,
}

#[derive(Debug, Deserialize)]
struct Deposit {
    orig_net: u32,
    orig_addr: String,
    amount: String,
    dest_net: u32,
    dest_addr: String,
    deposit_cnt: u64,
    network_id: u32,
    ready_for_claim: bool,
    global_index: String,
    metadata: String,
}

#[derive(Debug, Deserialize)]
struct MerkleProofResponse {
    proof: MerkleProof,
}

#[derive(Debug, Deserialize)]
struct MerkleProof {
    merkle_proof: Vec<String>,
    rollup_merkle_proof: Vec<String>,
    main_exit_root: String,
    rollup_exit_root: String,
}

pub struct ClaimSettlerConfig {
    pub bridge_service_url: String,
    pub l1_rpc_url: String,
    pub bridge_address: Address,
    pub signer: PrivateKeySigner,
    pub watch_addresses: Vec<Address>,
    pub persistence_path: Option<PathBuf>,
}

pub struct ClaimSettler {
    config: ClaimSettlerConfig,
    tracker: ClaimSettlerTracker,
    http: reqwest::Client,
}

impl ClaimSettler {
    pub fn new(config: ClaimSettlerConfig) -> anyhow::Result<Self> {
        let tracker = ClaimSettlerTracker::new(config.persistence_path.clone())?;
        Ok(Self {
            config,
            tracker,
            http: reqwest::Client::new(),
        })
    }

    pub async fn run(self) {
        tracing::info!(
            watch_addresses = ?self.config.watch_addresses,
            "ClaimSettler: starting ({}s delay)", STARTUP_DELAY.as_secs(),
        );
        tokio::time::sleep(STARTUP_DELAY).await;
        tracing::info!("ClaimSettler: polling loop started");
        loop {
            if let Err(e) = self.poll_once().await {
                tracing::warn!("ClaimSettler: poll error: {e:#}");
            }
            tokio::time::sleep(POLL_INTERVAL).await;
        }
    }

    async fn poll_once(&self) -> anyhow::Result<()> {
        for addr in &self.config.watch_addresses {
            self.process_address(*addr).await?;
        }
        Ok(())
    }

    async fn process_address(&self, addr: Address) -> anyhow::Result<()> {
        let url = format!("{}/bridges/{addr}", self.config.bridge_service_url);
        let resp = self.http.get(&url).send().await?;
        if !resp.status().is_success() {
            tracing::warn!(
                status = %resp.status(),
                address = %addr,
                "ClaimSettler: bridge-service returned non-success status (may not be ready yet)"
            );
            return Ok(());
        }
        let body: BridgesResponse = resp.json().await?;

        for dep in &body.deposits {
            if dep.network_id != 1 || dep.dest_net != 0 || !dep.ready_for_claim {
                continue;
            }
            if self.tracker.is_claimed(dep.deposit_cnt) {
                continue;
            }
            tracing::info!(
                deposit_cnt = dep.deposit_cnt, amount = %dep.amount, dest = %dep.dest_addr,
                "ClaimSettler: claiming deposit"
            );
            match self.claim_deposit(dep).await {
                Ok(()) => {
                    self.tracker.mark_claimed(dep.deposit_cnt);
                    tracing::info!(deposit_cnt = dep.deposit_cnt, "ClaimSettler: claimed");
                }
                Err(e) => {
                    tracing::error!(deposit_cnt = dep.deposit_cnt, "ClaimSettler: {e:#}");
                }
            }
        }
        Ok(())
    }

    async fn claim_deposit(&self, dep: &Deposit) -> anyhow::Result<()> {
        let proof_url = format!(
            "{}/merkle-proof?deposit_cnt={}&net_id=1",
            self.config.bridge_service_url, dep.deposit_cnt
        );
        let proof_resp: MerkleProofResponse =
            self.http.get(&proof_url).send().await?.json().await?;

        let smt_local = parse_proof_array(&proof_resp.proof.merkle_proof)?;
        let smt_rollup = parse_proof_array(&proof_resp.proof.rollup_merkle_proof)?;
        let main_exit_root = parse_bytes32(&proof_resp.proof.main_exit_root)?;
        let rollup_exit_root = parse_bytes32(&proof_resp.proof.rollup_exit_root)?;

        let global_index = parse_u256(&dep.global_index)?;
        let orig_addr: Address = dep.orig_addr.parse()?;
        let dest_addr: Address = dep.dest_addr.parse()?;
        let amount = parse_u256(&dep.amount)?;
        let metadata = if dep.metadata.is_empty() || dep.metadata == "0x" {
            alloy::primitives::Bytes::new()
        } else {
            alloy::primitives::Bytes::from(hex::decode(dep.metadata.trim_start_matches("0x"))?)
        };

        let call = claimAssetCall {
            smtProofLocalExitRoot: smt_local,
            smtProofRollupExitRoot: smt_rollup,
            globalIndex: global_index,
            mainnetExitRoot: main_exit_root,
            rollupExitRoot: rollup_exit_root,
            originNetwork: dep.orig_net,
            originTokenAddress: orig_addr,
            destinationNetwork: dep.dest_net,
            destinationAddress: dest_addr,
            amount,
            metadata,
        };

        let wallet = alloy::network::EthereumWallet::from(self.config.signer.clone());
        let provider = ProviderBuilder::new()
            .wallet(wallet)
            .connect_http(self.config.l1_rpc_url.parse()?);
        let tx = alloy_rpc_types_eth::TransactionRequest::default()
            .to(self.config.bridge_address)
            .input(call.abi_encode().into());

        let pending = provider.send_transaction(tx).await?;
        let tx_hash = *pending.tx_hash();
        tracing::info!(deposit_cnt = dep.deposit_cnt, %tx_hash, "ClaimSettler: tx sent");

        let receipt = pending.get_receipt().await?;
        if !receipt.status() {
            anyhow::bail!("L1 claim tx {tx_hash} reverted");
        }
        tracing::info!(deposit_cnt = dep.deposit_cnt, %tx_hash, "ClaimSettler: confirmed");
        Ok(())
    }
}

fn parse_proof_array(proofs: &[String]) -> anyhow::Result<[FixedBytes<32>; 32]> {
    if proofs.len() != 32 {
        anyhow::bail!("expected 32 proof elements, got {}", proofs.len());
    }
    let mut result = [FixedBytes::ZERO; 32];
    for (i, hex_str) in proofs.iter().enumerate() {
        result[i] = parse_bytes32(hex_str)?;
    }
    Ok(result)
}

fn parse_bytes32(hex_str: &str) -> anyhow::Result<FixedBytes<32>> {
    let bytes = hex::decode(hex_str.trim_start_matches("0x"))?;
    if bytes.len() != 32 {
        anyhow::bail!("expected 32 bytes, got {}", bytes.len());
    }
    Ok(FixedBytes::from_slice(&bytes))
}

fn parse_u256(s: &str) -> anyhow::Result<U256> {
    let stripped = s.trim_start_matches("0x");
    let radix = if s.starts_with("0x") { 16 } else { 10 };
    Ok(U256::from_str_radix(stripped, radix)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_bytes32_valid() {
        let hex_str = format!("0x{}", "aa".repeat(32));
        let result = parse_bytes32(&hex_str).unwrap();
        assert_eq!(result, FixedBytes::from([0xaa; 32]));
    }

    #[test]
    fn test_parse_bytes32_no_prefix() {
        let hex_str = "bb".repeat(32);
        let result = parse_bytes32(&hex_str).unwrap();
        assert_eq!(result, FixedBytes::from([0xbb; 32]));
    }

    #[test]
    fn test_parse_bytes32_wrong_length() {
        assert!(parse_bytes32("0xaabb").is_err());
    }

    #[test]
    fn test_parse_bytes32_invalid_hex() {
        assert!(parse_bytes32("0xzzzz").is_err());
    }

    #[test]
    fn test_parse_u256_decimal() {
        let result = parse_u256("12345").unwrap();
        assert_eq!(result, U256::from(12345u64));
    }

    #[test]
    fn test_parse_u256_hex() {
        let result = parse_u256("0xff").unwrap();
        assert_eq!(result, U256::from(255u64));
    }

    #[test]
    fn test_parse_u256_zero() {
        assert_eq!(parse_u256("0").unwrap(), U256::ZERO);
        assert_eq!(parse_u256("0x0").unwrap(), U256::ZERO);
    }

    #[test]
    fn test_parse_proof_array_valid() {
        let proofs: Vec<String> = (0..32).map(|i| format!("0x{}", format!("{:02x}", i).repeat(32))).collect();
        let result = parse_proof_array(&proofs).unwrap();
        assert_eq!(result[0], parse_bytes32(&proofs[0]).unwrap());
        assert_eq!(result[31], parse_bytes32(&proofs[31]).unwrap());
    }

    #[test]
    fn test_parse_proof_array_wrong_count() {
        let proofs: Vec<String> = (0..31).map(|_| format!("0x{}", "00".repeat(32))).collect();
        assert!(parse_proof_array(&proofs).is_err());
    }

    #[test]
    fn test_claim_settler_tracker_in_memory() {
        let tracker = ClaimSettlerTracker::new(None).unwrap();
        assert!(!tracker.is_claimed(42));
        tracker.mark_claimed(42);
        assert!(tracker.is_claimed(42));
        assert!(!tracker.is_claimed(43));
    }

    #[test]
    fn test_claim_settler_tracker_persistence() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settler_state.json");

        // Create and persist
        {
            let tracker = ClaimSettlerTracker::new(Some(path.clone())).unwrap();
            tracker.mark_claimed(10);
            tracker.mark_claimed(20);
        }

        // Reload and verify
        {
            let tracker = ClaimSettlerTracker::new(Some(path)).unwrap();
            assert!(tracker.is_claimed(10));
            assert!(tracker.is_claimed(20));
            assert!(!tracker.is_claimed(30));
        }
    }
}
