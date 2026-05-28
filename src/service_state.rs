use crate::block_state::BlockState;
use crate::store::Store;
use crate::*;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::Mutex;

/// Per-signer async mutex registry that serialises the
/// `nonce_get → handler → nonce_increment` critical section in
/// `eth_sendRawTransaction`.
///
/// Self-review of-the-fix follow-up — the original R4 commit checked the
/// nonce against `nonce_get` and incremented later, but the two operations
/// weren't atomic. Two concurrent valid txs at the same nonce both passed
/// the equality check before either incremented; for `claimAsset`, `try_claim`
/// dedupes by `globalIndex`, but the GER injection path could double-inject.
/// This mutex ensures the entire request-handling lifecycle for one signer
/// runs serially.
#[derive(Clone, Default)]
pub struct PerSignerLocks {
    inner: Arc<std::sync::Mutex<HashMap<alloy::primitives::Address, Arc<Mutex<()>>>>>,
}

impl PerSignerLocks {
    pub fn new() -> Self {
        Self::default()
    }

    /// Acquire the mutex for `signer`, creating it if needed. Returns an
    /// owned guard the caller must hold for the duration of the critical
    /// section.
    pub async fn lock(
        &self,
        signer: alloy::primitives::Address,
    ) -> tokio::sync::OwnedMutexGuard<()> {
        // Fetch (or create) the per-signer mutex under a quick std-mutex
        // (no `await`-points held). The actual critical section uses the
        // returned tokio mutex.
        let mu = {
            let mut map = self
                .inner
                .lock()
                .expect("PerSignerLocks std-mutex poisoned");
            map.entry(signer)
                .or_insert_with(|| Arc::new(Mutex::new(())))
                .clone()
        };
        mu.lock_owned().await
    }
}

#[derive(Clone)]
pub struct ServiceState {
    pub miden_client: Arc<MidenClient>,
    pub accounts: AccountsConfig,
    pub chain_id: u64,
    /// Rollup network ID from RollupManager (used for bridge's `networkID()` call).
    ///
    /// Stored as `u32` because the Solidity bridge contract types
    /// `originNetwork` / `destinationNetwork` as `uint32` (see
    /// `claimAssetCall` in `src/claim.rs`) and the on-chain `networkID()`
    /// return is `uint32`. The CLI flag is parsed as `u64` for backward
    /// compat but validated and narrowed at startup in `main.rs` via
    /// `u32::try_from`; reaching this struct with anything beyond `u32::MAX`
    /// is therefore impossible (RD-703).
    pub network_id: u32,
    pub store: Arc<dyn Store>,
    pub block_state: Arc<BlockState>,
    /// L1 RPC URL for resolving exit roots from the L1 GER contract
    pub l1_rpc_url: Option<String>,
    /// L1 GER contract address
    pub ger_l1_address: Option<String>,
    /// Miden client store directory (used by recovery commands; the long-lived
    /// `MidenClient` owns the sqlite handle so production submission paths do
    /// not need to re-derive this).
    pub miden_store_dir: PathBuf,
    /// CORS-allowed origins (R11). `None` = no cross-origin requests permitted (the
    /// safe default in production); `Some(list)` = explicit allowlist; the special
    /// single-entry `vec!["*"]` is reserved for dev-only wildcards.
    pub cors_allowed_origins: Option<Vec<String>>,
    /// Admin API key (R1). `None` = `admin_*` JSON-RPC methods are disabled
    /// entirely (the safe production default — fail closed). `Some(token)` =
    /// admin requests must carry `Authorization: Bearer <token>`.
    pub admin_api_key: Option<String>,
    /// Allow-list of EVM signer addresses (R2). `None` = `eth_sendRawTransaction`
    /// is OPEN to any signer (legacy default; only safe behind a private
    /// network-level boundary). `Some(list)` = inbound txs must be signed by an
    /// address in the list. Production must always set this — aggsender,
    /// aggoracle, and any operator-rescue tool are the only legitimate signers.
    pub allowed_signers: Option<Vec<alloy::primitives::Address>>,
    /// Per-signer async mutex registry (R4 follow-up) — serialises the
    /// nonce-check critical section so two concurrent same-nonce txs from one
    /// signer cannot both pass the equality check before either increments.
    pub per_signer_locks: PerSignerLocks,
    /// Per-IP rate limit (R13) — sustained rate (per second).
    pub rate_limit_per_second: u64,
    /// Per-IP rate limit burst (R13).
    pub rate_limit_burst: u32,
    /// Reject the address-mapper zero-padding fallback (C5). When `true`,
    /// claims targeting an EVM address with no explicit store mapping are
    /// rejected immediately instead of falling through to the structural
    /// reconstruction. Production posture; default false for backward
    /// compatibility with aggsender / aggoracle / hardhat dev flows.
    pub reject_zero_padding_addresses: bool,
    /// Reject the Hardhat default-account alias remap (Cantina MA#8).
    /// When `true`, the special-case remap of the well-known Hardhat
    /// address (`0xf39f...2266`) to `wallet_hardhat` is disabled. Required
    /// for production deployments — otherwise any L1 deposit targeting
    /// the Hardhat address is silently routed into the operator-owned
    /// wallet account. Enforced as a `--require-hardening` invariant in
    /// `main.rs::check_hardening_invariants`.
    pub reject_hardhat_alias: bool,
    /// Cantina #7 expected-MINT tracker, shared with the `BridgeOutScanner`.
    /// `publish_claim_internal` records each submitted CLAIM's NoteId here;
    /// the scanner ticks it each sync, marking entries Landed once it
    /// observes the CLAIM consumed by the bridge. Stale entries page on-call.
    pub expected_mints: Arc<crate::expected_mint_tracker::ExpectedMintTracker>,
    /// Optional `authorization: Bearer <key>` header value forwarded to every Miden gRPC
    /// call. `None` when talking to the node directly; `Some(...)` when fronted by a
    /// gateway that rate-limits unauthenticated traffic. Redact if you ever log this.
    pub miden_api_key: Option<String>,
}

const fn assert_sync<T: Send + Sync>() {}
const _: () = assert_sync::<ServiceState>();

impl ServiceState {
    pub fn new(
        miden_client: MidenClient,
        accounts: AccountsConfig,
        chain_id: u64,
        network_id: u32,
        store: Arc<dyn Store>,
        block_state: Arc<BlockState>,
    ) -> Self {
        Self {
            miden_client: Arc::new(miden_client),
            accounts,
            chain_id,
            network_id,
            store,
            block_state,
            l1_rpc_url: None,
            ger_l1_address: None,
            miden_store_dir: PathBuf::new(),
            cors_allowed_origins: None,
            admin_api_key: None,
            allowed_signers: None,
            per_signer_locks: PerSignerLocks::new(),
            rate_limit_per_second: crate::service::DEFAULT_RATE_LIMIT_PER_SECOND,
            rate_limit_burst: crate::service::DEFAULT_RATE_LIMIT_BURST,
            reject_zero_padding_addresses: false,
            reject_hardhat_alias: false,
            expected_mints: Arc::new(crate::expected_mint_tracker::ExpectedMintTracker::new()),
            miden_api_key: None,
        }
    }
}
