use crate::accounts_config::AccountsConfig;
use crate::faucet_ops;
use crate::miden_client::{MidenClient, MidenClientLib};
use crate::store::{FaucetEntry, Store};
use alloy::primitives::{BlockNumber, Bytes, FixedBytes};
use miden_base_agglayer::{
    ClaimNoteStorage, EthAddress, EthAmount, ExitRoot, GlobalIndex, LeafData, MetadataHash,
    ProofData, SmtNode,
};
use miden_client::transaction::{TransactionProver, TransactionRequestBuilder};
use miden_protocol::account::AccountId;
use miden_protocol::crypto::rand::FeltRng;
use miden_protocol::note::Note;
use miden_protocol::transaction::TransactionId;
use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex, OnceLock};

/// In-flight faucet-provisioning registry for **single-flight** first-claim
/// auto-creation (finding #10 / Cantina #10). Keyed by the canonical asset
/// identity `(origin_token_address, origin_network)`.
///
/// The service is single-process (multiple replicas are unsupported — see
/// `main.rs`/the synthetic projector), and every `Store` is a single shared
/// `Arc<dyn Store>`, so a process-global registry is a sound coordination
/// primitive. Unlike the previous per-origin mutex — where the loser still
/// ENTERED the critical section and re-read the store — this registry makes a
/// concurrent second first-claim STRUCTURALLY unable to reach the provisioning
/// path: it clones a `watch::Receiver` and AWAITS the winner's result. The
/// awaiter can never call [`provision_faucet`], never deploys a faucet, and
/// never touches the Miden client.
///
/// Keyed by the **`(address, network)` pair**, not the address alone: per
/// `bridge_config.masm`'s `store_faucet_registration` (agglayer #2860), the
/// on-chain `token_registry_map` is keyed on `hash(tokenAddress || origin_network)`,
/// so the `(origin_network, origin_token_address)` pair is the canonical asset
/// identity. The same address on two networks is TWO distinct assets with TWO
/// distinct on-chain registry leaves and no collision — they must therefore
/// single-flight independently (an address-only key would wrongly route a
/// concurrent claim for `(T, net=1)` awaiting a provisioner for `(T, net=0)` to
/// net-0's faucet).
///
/// The map value is a `watch::Receiver` seeded with `None`; the sole provisioner
/// holds the matching `Sender` and publishes `Some(Ok(summary))` on success or
/// `Some(Err(msg))` on failure. Awaiters clone the receiver and wait for the
/// first `Some(..)`. Concurrency invariant: the guarding `std::sync::Mutex` is
/// only ever held for the O(1) lookup/insert/remove — NEVER across an `.await`.
///
/// Bounded memory: the provisioner's [`FaucetInflightGuard`] REMOVES the entry
/// as soon as provisioning settles, so the map only ever holds *currently
/// in-flight* keys — never the full history of every asset seen. This is unlike
/// a per-origin lock registry, which would accrue one never-evicted entry per
/// attacker-controllable origin (an unbounded-growth / memory-DoS vector).
type FaucetInflightMap =
    Mutex<HashMap<([u8; 20], u32), tokio::sync::watch::Receiver<Option<Result<Faucet, String>>>>>;

static FAUCET_INFLIGHT: LazyLock<FaucetInflightMap> = LazyLock::new(|| Mutex::new(HashMap::new()));

/// RAII guard that clears a provisioner's in-flight registry entry on drop —
/// including on panic. If the provisioner future panics mid-flight its
/// `watch::Sender` is dropped (closing the channel) and this guard removes the
/// map key, so awaiters observe a closed channel and retry into a fresh cohort
/// rather than wedging the origin forever.
struct FaucetInflightGuard {
    key: ([u8; 20], u32),
}

impl Drop for FaucetInflightGuard {
    fn drop(&mut self) {
        FAUCET_INFLIGHT
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&self.key);
    }
}

/// Outcome of one single-flight coordination attempt for an origin address.
enum FaucetProvisionOutcome {
    /// This task either ran provisioning itself (as the sole provisioner) or
    /// awaited the winner's SUCCESSFUL result. Either way the value is terminal.
    Settled(anyhow::Result<Faucet>),
    /// This task awaited a peer provisioner that FAILED (published `Err`) or
    /// panicked (closed the channel without publishing). The in-flight entry has
    /// since been cleared, so the caller may retry from the top and become the
    /// new provisioner. Never returned to the provisioner itself.
    PeerFailedRetry,
}

/// Single-flight coordinator: for a given asset `key`
/// (`(origin_token_address, origin_network)`), run `provision` **exactly once**
/// across all concurrent callers. The first caller inserts a `watch` channel and
/// becomes the sole PROVISIONER; every concurrent caller clones the receiver and
/// becomes an AWAITER that NEVER calls `provision`.
///
/// This is the structural guarantee the finding-#10 fix now rests on: an
/// awaiter cannot reach `provision` — it only holds a `watch::Receiver` and
/// awaits — so a concurrent first-claim can never deploy a second faucet or
/// touch the Miden client. (The previous mutex design could not express this at
/// the type level: the loser still entered the critical section.)
async fn coordinate_faucet_provision<F, Fut>(
    key: ([u8; 20], u32),
    provision: F,
) -> FaucetProvisionOutcome
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = anyhow::Result<Faucet>>,
{
    enum Role {
        Provisioner(tokio::sync::watch::Sender<Option<Result<Faucet, String>>>),
        Awaiter(tokio::sync::watch::Receiver<Option<Result<Faucet, String>>>),
    }

    // Briefly hold the std-mutex to look up or insert. No `.await` under it.
    let role = {
        let mut map = FAUCET_INFLIGHT.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(rx) = map.get(&key) {
            Role::Awaiter(rx.clone())
        } else {
            let (tx, rx) = tokio::sync::watch::channel(None);
            map.insert(key, rx);
            Role::Provisioner(tx)
        }
    };

    match role {
        Role::Provisioner(tx) => {
            // Clear the in-flight entry on ALL exits (success, error, panic).
            let _guard = FaucetInflightGuard { key };
            let result = provision().await;
            // Publish a Clone-able summary to awaiters. On error, forward the
            // rendered message; awaiters map any `Err` to a retry.
            let published = match &result {
                Ok(faucet) => Ok(*faucet),
                Err(err) => Err(format!("{err:#}")),
            };
            // Awaiters hold cloned receivers, so this reaches them even though
            // `_guard` removes the map key immediately afterwards.
            let _ = tx.send(Some(published));
            FaucetProvisionOutcome::Settled(result)
        }
        Role::Awaiter(mut rx) => {
            // The AWAITER never provisions. Drop the unused closure now so it is
            // *impossible* for this path to reach `provision`, and so any borrows
            // it captured (e.g. the `&mut MidenClientLib`) are released before we
            // await.
            drop(provision);
            loop {
                // `borrow_and_update` returns the current value and marks it
                // seen; if the provisioner already published, return here.
                if let Some(published) = rx.borrow_and_update().clone() {
                    return match published {
                        Ok(faucet) => FaucetProvisionOutcome::Settled(Ok(faucet)),
                        Err(_) => FaucetProvisionOutcome::PeerFailedRetry,
                    };
                }
                // Wait for the provisioner to publish. `changed()` errors iff the
                // Sender was dropped without publishing (provisioner panicked):
                // treat as failure + retry rather than hang forever.
                if rx.changed().await.is_err() {
                    return FaucetProvisionOutcome::PeerFailedRetry;
                }
            }
        }
    }
}

pub const CLAIM_RECEIPT_EXPIRATION_BLOCKS_ENV: &str = "AGGLAYER_CLAIM_RECEIPT_EXPIRATION_BLOCKS";
pub const DEFAULT_CLAIM_RECEIPT_EXPIRATION_BLOCKS: u64 = 120;

pub fn claim_receipt_expiration_blocks() -> u64 {
    match std::env::var(CLAIM_RECEIPT_EXPIRATION_BLOCKS_ENV) {
        Ok(value) => match value.parse::<u64>() {
            Ok(blocks) if blocks >= 1 => blocks,
            Ok(blocks) => {
                tracing::warn!(
                    env = CLAIM_RECEIPT_EXPIRATION_BLOCKS_ENV,
                    value = blocks,
                    "{CLAIM_RECEIPT_EXPIRATION_BLOCKS_ENV} must be >= 1 block; using default {DEFAULT_CLAIM_RECEIPT_EXPIRATION_BLOCKS}"
                );
                DEFAULT_CLAIM_RECEIPT_EXPIRATION_BLOCKS
            }
            Err(err) => {
                tracing::warn!(
                    env = CLAIM_RECEIPT_EXPIRATION_BLOCKS_ENV,
                    value = %value,
                    error = %err,
                    "invalid {CLAIM_RECEIPT_EXPIRATION_BLOCKS_ENV}; using default {DEFAULT_CLAIM_RECEIPT_EXPIRATION_BLOCKS}"
                );
                DEFAULT_CLAIM_RECEIPT_EXPIRATION_BLOCKS
            }
        },
        Err(_) => DEFAULT_CLAIM_RECEIPT_EXPIRATION_BLOCKS,
    }
}

alloy_core::sol! {
    // https://github.com/agglayer/agglayer-contracts/blob/main/contracts/v2/PolygonZkEVMBridgeV2.sol#L556
    #[derive(Debug)]
    function claimAsset(
        bytes32[32] calldata smtProofLocalExitRoot,
        bytes32[32] calldata smtProofRollupExitRoot,
        uint256 globalIndex,
        bytes32 mainnetExitRoot,
        bytes32 rollupExitRoot,
        uint32 originNetwork,
        address originTokenAddress,
        uint32 destinationNetwork,
        address destinationAddress,
        uint256 amount,
        bytes calldata metadata
    );
}

alloy_core::sol! {
    // https://github.com/agglayer/agglayer-contracts/blob/main/contracts/v2/PolygonZkEVMBridgeV2.sol#L139
    #[derive(Debug)]
    event ClaimEvent(
        uint256 globalIndex,
        uint32 originNetwork,
        address originAddress,
        address destinationAddress,
        uint256 amount
    );
}

impl From<claimAssetCall> for ClaimEvent {
    fn from(value: claimAssetCall) -> Self {
        Self {
            globalIndex: value.globalIndex,
            originNetwork: value.originNetwork,
            originAddress: value.originTokenAddress,
            destinationAddress: value.destinationAddress,
            amount: value.amount,
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct Faucet {
    id: AccountId,
    decimals: u8,
    origin_token_decimals: u8,
}

/// Look up a faucet for the given origin token, auto-creating one if not found.
///
/// On the first bridge of a new ERC-20 token, the faucet is created on Miden,
/// registered in the bridge, and saved to the Store — all automatically.
///
/// Concurrency (finding #10 / Cantina #10) — **single-flight**: the
/// check→deploy→bridge-register→persist sequence must run at most once per
/// `(origin_token_address, origin_network)` asset. Two concurrent first-bridge
/// claims for the same ERC-20 previously both passed the empty-local check, both
/// deployed a faucet and both registered in the bridge (whose
/// `(address, network)`-keyed route ends on the *second* faucet), while the local
/// write for the second faucet failed on the `(origin_address, origin_network)`
/// unique index — leaving the local registry pinned to faucet A and the bridge
/// routing by faucet B, so later bridge-outs of B-minted assets could not be
/// resolved and emitted no synthetic BridgeEvent.
///
/// `MidenClient::with(...)` queues each request on a size-1 channel and the
/// single client task awaits the WHOLE closure before taking the next, so today
/// this entire check→deploy→register→persist sequence already runs serialised
/// against every other claim on that task — in the current call graph two
/// first-claims cannot actually interleave here. The single-flight coordinator
/// ([`coordinate_faucet_provision`]) is therefore defense-in-depth: it does NOT
/// rely on that incidental full-closure serialisation but makes the per-asset
/// dedup EXPLICIT and structural, so the guarantee survives any refactor that
/// moves part of the check→provision path off the single client task (a store
/// fast-path read outside `.with()`, a second client, or concurrent
/// provisioning) — where two first-claims could otherwise each pass the
/// empty-store fast path and both reach provisioning. The first concurrent claim
/// becomes the sole PROVISIONER (runs [`provision_faucet`]); every other becomes
/// an AWAITER that clones a `watch::Receiver` and awaits the winner's result —
/// it can never reach [`provision_faucet`], so it structurally cannot deploy a
/// second faucet or touch the Miden client. If the provisioner fails, awaiters
/// get one retry (the in-flight entry is cleared, so a retrying awaiter can
/// become the new provisioner) before the error bubbles. `Store::register_faucet`'s
/// first-write-wins on the origin unique key remains the durable backstop (see
/// `store::postgres`/`store::memory`).
async fn find_or_create_faucet(
    token_address: alloy::primitives::Address,
    origin_network: u32,
    metadata: &Bytes,
    store: &dyn Store,
    client: &mut MidenClientLib,
    accounts: &AccountsConfig,
) -> anyhow::Result<Faucet> {
    let origin_address = token_address.0.0;
    // One retry is permitted: if the single-flight provisioner fails, an awaiter
    // may loop once and become the new provisioner (the entry has been cleared).
    let mut allow_retry = true;
    loop {
        // Fast path — a read-only lookup for the overwhelmingly-common
        // already-registered case. Re-run at the head of the loop so a retrying
        // awaiter also notices a peer cohort that succeeded before it looped.
        if let Some(entry) = store
            .get_faucet_by_origin(&origin_address, origin_network)
            .await?
        {
            return Ok(Faucet {
                id: entry.faucet_id,
                decimals: entry.miden_decimals,
                origin_token_decimals: entry.origin_decimals,
            });
        }

        // Single-flight: exactly one concurrent first-claim runs `provision_faucet`.
        // Keyed by the `(origin_address, origin_network)` asset identity so the
        // same address on two networks single-flights independently (two distinct
        // on-chain registry leaves — see `FaucetInflightMap` / agglayer #2860).
        // The `&mut *client` reborrow is captured only by the PROVISIONER path;
        // an AWAITER drops the closure unused (see `coordinate_faucet_provision`).
        let outcome = coordinate_faucet_provision((origin_address, origin_network), || {
            provision_faucet(
                token_address,
                origin_network,
                metadata,
                store,
                &mut *client,
                accounts,
            )
        })
        .await;

        match outcome {
            FaucetProvisionOutcome::Settled(result) => return result,
            FaucetProvisionOutcome::PeerFailedRetry => {
                if !allow_retry {
                    anyhow::bail!(
                        "faucet provisioning for token {token_address} (origin network \
                         {origin_network}) failed in a concurrent first-claim and the single \
                         retry was exhausted; refusing to loop"
                    );
                }
                allow_retry = false;
                tracing::warn!(
                    token_address = %token_address,
                    origin_network,
                    "finding #10: peer provisioner failed; retrying as the new provisioner"
                );
                continue;
            }
        }
    }
}

/// Provision a brand-new faucet for `token_address` — the single-flight
/// PROVISIONER body, run at most once per concurrent cohort by
/// [`coordinate_faucet_provision`]. AWAITERS never call this and never touch the
/// Miden client.
///
/// Ordering is load-bearing:
/// 1. Re-check the store — a cheap safety net (a prior cohort or the retry path
///    may already have persisted). The durable backstop remains
///    `Store::register_faucet`'s first-write-wins on the `(origin_address,
///    origin_network)` unique key.
/// 2. Parse metadata, deploy + bridge-register, persist. Note there is NO
///    cross-network refusal: per `bridge_config.masm`'s `store_faucet_registration`
///    (agglayer #2860) the on-chain `token_registry_map` is keyed on
///    `hash(tokenAddress || origin_network)`, so the same address on a different
///    network is a DISTINCT asset with its own registry leaf — no collision.
async fn provision_faucet(
    token_address: alloy::primitives::Address,
    origin_network: u32,
    metadata: &Bytes,
    store: &dyn Store,
    client: &mut MidenClientLib,
    accounts: &AccountsConfig,
) -> anyhow::Result<Faucet> {
    // 1. Re-check the store. If a racing first-claim already registered the
    //    faucet (or a prior failed cohort's retry did), reuse it rather than
    //    deploying a second one.
    if let Some(entry) = store
        .get_faucet_by_origin(&token_address.0.0, origin_network)
        .await?
    {
        tracing::info!(
            token_address = %token_address,
            origin_network,
            faucet_id = %crate::accounts_config::AccountIdBech32(entry.faucet_id),
            "finding #10: faucet already registered; reusing it (single-flight provisioner)"
        );
        return Ok(Faucet {
            id: entry.faucet_id,
            decimals: entry.miden_decimals,
            origin_token_decimals: entry.origin_decimals,
        });
    }

    // 2. No cross-network refusal. Per `bridge_config.masm`'s
    //    `store_faucet_registration` (agglayer #2860), the on-chain
    //    `token_registry_map` is keyed on `hash(tokenAddress || origin_network)` —
    //    the `(origin_network, origin_token_address)` pair is the canonical asset
    //    identity. The same token address on a different `origin_network` is a
    //    DISTINCT asset that gets its own registry leaf, so auto-creating a second
    //    faucet cannot overwrite the first registration on-chain. (The pre-#2860
    //    premise that the registry keyed by `hash(origin_token_address)` alone —
    //    and the "Cantina #1" refusal built on it — is stale and removed.)

    // 2b. Cantina #6 — recover an EXISTING on-chain faucet before deploying a
    //     replacement. The local row is missing (fresh DB / lost identity), but the
    //     faucet may still be registered on the bridge for this exact origin token.
    //     Deploying a second faucet here would create the split-brain the finding
    //     describes: two live generations for one (origin_address, origin_network),
    //     with the old generation's exits invisible forever (its faucet stays
    //     bridge-out-valid on Miden but unresolvable locally). Import the existing
    //     identity instead. Coexists with the Cantina #1 cross-network refusal above
    //     (which already rejected a different-network same-address collision).
    if let Some(bridge_account) = client.get_account(accounts.bridge.0).await.ok().flatten()
        && let Some((existing_id, conversion)) =
            crate::metadata_recovery::find_registered_faucet_for_origin(
                bridge_account.storage(),
                &token_address.0.0,
                origin_network,
            )
    {
        tracing::warn!(
            token_address = %token_address,
            origin_network,
            faucet_id = %existing_id,
            "Cantina #6: origin token already has a faucet registered on the bridge but no local \
             row — importing the existing faucet identity instead of deploying a replacement \
             (prevents split-brain)"
        );
        match faucet_ops::rebuild_faucet_entry_from_chain(
            client,
            &bridge_account,
            existing_id,
            &conversion,
            None,
        )
        .await
        {
            Ok(mut entry) => {
                // The claimAsset metadata IS the authoritative preimage for this token
                // (same abi.encode(name,symbol,decimals) whose keccak is the faucet's
                // MetadataHash), so prefer it over the on-chain recovery result — capped
                // exactly like the auto-create path (Cantina #13).
                entry.metadata = cap_stored_faucet_metadata(metadata, &token_address);
                let (miden_decimals, origin_token_decimals) =
                    (entry.miden_decimals, entry.origin_decimals);
                store.register_faucet(entry).await?;
                ::metrics::counter!("faucet_recovered_existing_total").increment(1);
                return Ok(Faucet {
                    id: existing_id,
                    decimals: miden_decimals,
                    origin_token_decimals,
                });
            }
            Err(e) => {
                // Fail soft: if we cannot import the existing identity (e.g. the faucet
                // account isn't fetchable), fall through to deploy rather than block the
                // claim. This can re-introduce a second generation, so surface it loudly.
                ::metrics::counter!("faucet_recover_existing_failed_total").increment(1);
                tracing::warn!(
                    faucet_id = %existing_id,
                    error = ?e,
                    "Cantina #6: failed to import existing faucet identity; falling back to deploy \
                     (WARNING: may create a second generation)"
                );
            }
        }
    }

    // 3. Auto-create: parse token metadata from claimAsset call.
    //    `parse_token_metadata` already rejects `origin_decimals >
    //    MAX_ORIGIN_DECIMALS` (26), so a > 26-decimal (poisoned) token never
    //    reaches faucet creation — its route would be unclaimable (finding #17).
    let (symbol, origin_decimals) = faucet_ops::parse_token_metadata(metadata, &token_address)?;
    // Finding #17 — the local faucet decimals are `min(origin_decimals,
    // MIDEN_DECIMALS)` — capped at 8, never derived higher (mirrors main's
    // historical `min(origin, 8)` scheme). A low-decimal token (e.g. 6-decimal
    // USDC/USDT) gets a faucet matching its own decimals (scale 0); a high-decimal
    // token is pinned to 8 and downscaled. The factor `scale = origin_decimals -
    // min(origin_decimals, 8)` fits MAX_SCALING_FACTOR (18) for every
    // `origin_decimals <= 26`, which `parse_token_metadata` already guarantees.
    // `miden_decimals <= origin_decimals` by construction, so the checked_sub can
    // never underflow — it stays only as a defensive invariant guard.
    let miden_decimals: u8 = origin_decimals.min(faucet_ops::MIDEN_DECIMALS);
    let scale = origin_decimals.checked_sub(miden_decimals).ok_or_else(|| {
        anyhow::anyhow!(
            "internal invariant violated: miden_decimals {miden_decimals} > origin_decimals \
             {origin_decimals} for token {token_address} (finding #17)"
        )
    })?;

    tracing::info!(
        token_address = %token_address,
        symbol = %symbol,
        origin_decimals,
        scale,
        "auto-creating faucet for new ERC-20 token"
    );

    // 3. Create faucet on Miden, deploy, register in bridge. The faucet's stored
    //    metadata_hash must match the CLAIM note's leaf_data.metadata_hash, which is
    //    keccak256(metadata) (both empty for native ETH and abi.encode(name,symbol,decimals)
    //    for ERC-20s). Using MetadataHash::from_abi_encoded on the raw metadata bytes matches
    //    the L1 bridge contract exactly.
    let metadata_hash = MetadataHash::from_abi_encoded(metadata.as_ref());

    let faucet_account = faucet_ops::create_and_register_faucet(
        client,
        &symbol,
        miden_decimals,
        &token_address.0.0,
        origin_network,
        scale,
        accounts.service.0,
        accounts.bridge.0,
        metadata_hash,
    )
    .await?;

    // 4. Save to store
    //
    // Cantina #13 — the metadata is attacker-controlled L1 `claimAsset` calldata
    // and is otherwise unbounded. Cap it at storage: if it exceeds the bridge-out
    // emit cap, persist EMPTY rather than the oversized blob. Empty-for-oversized
    // is safe — the bridge-out emit site already gates oversized metadata, and the
    // (#91) Layer-2 recovery re-derives bounded metadata from L1 — so we never need
    // to keep the giant preimage around.
    let stored_metadata = cap_stored_faucet_metadata(metadata, &token_address);
    let entry = FaucetEntry {
        faucet_id: faucet_account.id(),
        origin_address: token_address.0.0,
        origin_network,
        symbol,
        origin_decimals,
        miden_decimals,
        scale,
        // Cantina #13 — store the raw ABI metadata preimage (same bytes whose
        // keccak256 is the faucet's MetadataHash) so a future bridge-out emits
        // the real metadata in its synthetic BridgeEvent. Empty for native ETH
        // and for oversized blobs (capped above).
        metadata: stored_metadata,
    };
    store.register_faucet(entry).await?;

    Ok(Faucet {
        id: faucet_account.id(),
        decimals: miden_decimals,
        origin_token_decimals: origin_decimals,
    })
}

fn bytes32_array_to_smt_nodes(values: [FixedBytes<32>; 32]) -> [SmtNode; 32] {
    values.map(|v| SmtNode::new(v.0))
}

/// Cantina #13 — cap attacker-controlled L1 `claimAsset` metadata before it is
/// persisted in the faucet registry. The calldata is otherwise unbounded; an
/// oversized blob would bloat storage and (without the bridge-out emit-site
/// guard) drive a huge allocation when synthesizing a BridgeEvent.
///
/// If the metadata exceeds the bridge-out emit cap we persist EMPTY rather than
/// the oversized blob. Empty-for-oversized is safe: the bridge-out emit site
/// already gates oversized metadata, and the (#91) Layer-2 recovery re-derives
/// bounded metadata from L1 — so we never need to keep the giant preimage.
fn cap_stored_faucet_metadata(
    metadata: &Bytes,
    token_address: &alloy::primitives::Address,
) -> Vec<u8> {
    if metadata.len() > crate::bridge_out::MAX_BRIDGE_EVENT_METADATA_BYTES {
        ::metrics::counter!("faucet_metadata_too_large_at_store_total").increment(1);
        tracing::warn!(
            token_address = %token_address,
            metadata_len = metadata.len(),
            cap = crate::bridge_out::MAX_BRIDGE_EVENT_METADATA_BYTES,
            "claim: ERC-20 metadata exceeds cap; storing empty metadata for faucet (Cantina #13)"
        );
        Vec::new()
    } else {
        metadata.to_vec()
    }
}

/// Decode the agglayer mainnet flag from a `globalIndex` U256.
///
/// GlobalIndex layout (per miden-agglayer's `eth_types::global_index`):
///   - bytes 0..20  : zero (top 160 bits of the U256)
///   - bytes 20..24 : mainnet flag (limb 5; value = 1 for mainnet, 0 for rollup)
///   - bytes 24..28 : rollup index (limb 6; must be 0 for mainnet deposits)
///   - bytes 28..32 : leaf index (limb 7)
///
/// `GlobalIndexExt::is_mainnet()` is gated behind upstream's `testing` feature so we
/// decode the flag inline.
fn is_mainnet_global_index(global_index_bytes: &[u8; 32]) -> bool {
    let flag = u32::from_be_bytes([
        global_index_bytes[20],
        global_index_bytes[21],
        global_index_bytes[22],
        global_index_bytes[23],
    ]);
    flag == 1
}

/// Build the CLAIM note's `ProofData`, canonicalising the rollup-side fields that the
/// upstream MASM mainnet branch genuinely doesn't read (Cantina #11).
///
/// Self-review of-the-fix history
/// ------------------------------
/// The original Cantina #11 commit zeroed *both* `smt_proof_rollup_exit_root` (256
/// felts) AND `rollup_exit_root` (8 felts) for mainnet claims, on the assumption
/// that neither was read by `bridge_in.masm`'s mainnet branch. That assumption
/// matched the SMT proof but was wrong about the exit root: the dynamic-ERC20
/// e2e (and any second-and-later mainnet claim against a non-zero on-chain
/// `PolygonZkEVMGlobalExitRootV2.rollupExitRoot`) failed with
/// `ERR_GER_NOT_FOUND` (assertion code `0xDF0E804B375D0B3B`).
///
/// Trace: `bridge_in.masm::verify_leaf` (line 532-553) calls `compute_ger`
/// (line 385-391) BEFORE the mainnet/rollup branch split. `compute_ger` is
/// `keccak256(mainnet_exit_root || rollup_exit_root)` and the result is looked
/// up in `GER_MAP_STORAGE_SLOT` by `assert_valid_ger`
/// (`bridge_config.masm::101`). The map is populated by `update_ger`
/// (`bridge_config.masm::48`) when an `UpdateGerNote` is consumed; aggkit
/// `ger.rs::141` injects the *real* L1 GER digest verbatim. Zeroing
/// `rollup_exit_root` on the CLAIM side made the recomputed key
/// `keccak256(mainnet_real || 0)` instead of `keccak256(mainnet_real ||
/// rollup_real)` whenever the L1 contract had advanced
/// `rollupExitRoot` past zero — the lookup then missed and the assertion
/// fired.
///
/// The original Cantina #11 NoteId-determinism property is preserved without
/// the over-zero: for a given mainnet leaf at a given GER, both
/// `mainnet_exit_root` and `rollup_exit_root` are fixed by the L1 contract
/// state, so they are NOT attacker-tunable in the way the SMT rollup proof
/// path bytes (256 felts of merkle siblings — only the SMT *path*-derived
/// root needs to match anything; the rest is attacker-supplied padding) and
/// the rollup-index bytes of `globalIndex` (must be 0 per the upstream layout
/// spec, but unread/unasserted) genuinely were.
///
/// Current canonicalisation (mainnet only):
/// - `smt_proof_rollup_exit_root` → all-zero (256 felts); unread by mainnet branch
/// - `globalIndex` bytes 24..28 (rollup index) → zero per layout spec
///
/// `rollup_exit_root` is left as-is — it IS read by `compute_ger`.
fn build_canonical_proof_data(params: &claimAssetCall) -> ProofData {
    let mut global_index_bytes = params.globalIndex.to_be_bytes::<32>();
    let is_mainnet = is_mainnet_global_index(&global_index_bytes);
    if is_mainnet {
        // Rollup index (limb 6 = bytes 24..28) must be 0 for mainnet deposits.
        // Zero it explicitly so attacker-supplied garbage in those bytes can't
        // change the resulting NoteId.
        global_index_bytes[24..28].fill(0);
    }
    ProofData {
        smt_proof_local_exit_root: bytes32_array_to_smt_nodes(params.smtProofLocalExitRoot),
        smt_proof_rollup_exit_root: if is_mainnet {
            [SmtNode::new([0u8; 32]); 32]
        } else {
            bytes32_array_to_smt_nodes(params.smtProofRollupExitRoot)
        },
        global_index: GlobalIndex::new(global_index_bytes),
        mainnet_exit_root: ExitRoot::new(params.mainnetExitRoot.0),
        // rollup_exit_root MUST NOT be zeroed: bridge_in's compute_ger feeds
        // it through keccak256 to derive the GER lookup key, which must match
        // the digest the GER manager injected (the *real* L1 root pair).
        // See the docstring above for the diagnosis trail.
        rollup_exit_root: ExitRoot::new(params.rollupExitRoot.0),
    }
}

/// Scales an L1 deposit amount into a Miden fungible-token amount using the faucet's
/// decimal layout. Sub-unit wei are truncated (the full value is still preserved in
/// `leaf_data.amount`); the only hard failure is exceeding `FungibleAsset::MAX_AMOUNT`.
fn scale_claim_amount(
    amount: &EthAmount,
    faucet: Faucet,
) -> Result<miden_protocol::Felt, anyhow::Error> {
    let scale_byte = faucet
        .origin_token_decimals
        .checked_sub(faucet.decimals)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "faucet {} has miden_decimals ({}) > origin_token_decimals ({}); \
                 invariant violated, refusing to compute scale",
                faucet.id,
                faucet.decimals,
                faucet.origin_token_decimals,
            )
        })?;
    let scale = u32::from(scale_byte);
    amount
        .scale_to_token_amount(scale)
        .map_err(|e| anyhow::anyhow!("claim amount is not representable on Miden: {e}"))
}

async fn create_claim(
    params: claimAssetCall,
    faucet: Faucet,
    accounts: &AccountsConfig,
    store: &dyn Store,
    rng: &mut impl FeltRng,
    reject_zero_padding: bool,
) -> anyhow::Result<Note> {
    let sender = accounts.service.0;

    let _dest_account = crate::address_mapper::resolve_address_with_policy(
        store,
        params.destinationAddress,
        accounts,
        reject_zero_padding,
    )
    .await?;

    let proof_data = build_canonical_proof_data(&params);

    let leaf_data = LeafData {
        origin_network: params.originNetwork,
        origin_token_address: EthAddress::new(params.originTokenAddress.0.0),
        destination_network: params.destinationNetwork,
        destination_address: EthAddress::new(params.destinationAddress.0.0),
        amount: EthAmount::new(params.amount.to_be_bytes::<32>()),
        metadata_hash: MetadataHash::from_abi_encoded(params.metadata.as_ref()),
    };

    let miden_claim_amount = scale_claim_amount(&leaf_data.amount, faucet)?;
    let storage = ClaimNoteStorage {
        proof_data,
        leaf_data,
        miden_claim_amount,
    };

    // CLAIM notes now target the bridge account (0.14.x). The bridge validates the proof and
    // produces a MINT note targeted at the faucet. The faucet then creates the final P2ID note
    // for the destination wallet (derived from leaf_data.destination_address).
    let note = miden_base_agglayer::ClaimNote::create(storage, accounts.bridge.0, sender, rng)?;
    Ok(note)
}

/// Build the on-chain [`ClaimNoteStorage`] for a decoded `claimAsset` call —
/// the exact storage `create_claim` puts on a CLAIM note (same
/// [`build_canonical_proof_data`] canonicalisation, same [`LeafData`] mapping,
/// same amount scaling), factored so `bridge-out-tool`'s foreign-bridge e2e
/// mode (`--submit-foreign-claim`) can construct a byte-identical CLAIM
/// against a SECOND agglayer deployment on the same chain.
///
/// `scale_exp` is `origin_token_decimals - miden_decimals` for the faucet the
/// consuming bridge has registered for `(originTokenAddress, originNetwork)`
/// (10 for the standard 18→8 ETH faucet). Errors if the amount does not scale
/// to a representable Miden token amount.
pub fn claim_storage_from_call(
    params: &claimAssetCall,
    scale_exp: u32,
) -> anyhow::Result<ClaimNoteStorage> {
    let proof_data = build_canonical_proof_data(params);
    let leaf_data = LeafData {
        origin_network: params.originNetwork,
        origin_token_address: EthAddress::new(params.originTokenAddress.0.0),
        destination_network: params.destinationNetwork,
        destination_address: EthAddress::new(params.destinationAddress.0.0),
        amount: EthAmount::new(params.amount.to_be_bytes::<32>()),
        metadata_hash: MetadataHash::from_abi_encoded(params.metadata.as_ref()),
    };
    let miden_claim_amount = leaf_data
        .amount
        .scale_to_token_amount(scale_exp)
        .map_err(|e| anyhow::anyhow!("claim amount is not representable on Miden: {e}"))?;
    Ok(ClaimNoteStorage {
        proof_data,
        leaf_data,
        miden_claim_amount,
    })
}

#[derive(Debug, Clone)]
pub struct PublishClaimTxn {
    pub txn_id: TransactionId,
    pub expires_at: BlockNumber,
    /// Hex `details_commitment()` of the on-chain CLAIM note — the key the
    /// SyntheticProjector uses to recover the real claim eth-tx for the
    /// consumed note (see `record_tx_note_link` / `get_tx_for_note`).
    pub note_commitment: String,
}

#[allow(clippy::too_many_arguments)]
async fn publish_claim_internal(
    params: claimAssetCall,
    client: &mut MidenClientLib,
    accounts: &AccountsConfig,
    store: &dyn Store,
    latest_block_num: BlockNumber,
    reject_zero_padding: bool,
    expected_mints: Option<&Arc<crate::expected_mint_tracker::ExpectedMintTracker>>,
    // Opt-in local prover used as a fallback when the remote prover
    // configured on the surrounding `MidenClient` fails. `None` when
    // either (a) no remote prover is configured (the active prover IS
    // already local) or (b) `--miden-prover-fallback-to-local` was not
    // set. See `MidenClient::local_prover_fallback` for the full
    // selection logic and `metrics::meter_proof_with_fallback` for how
    // the two prove attempts are split across the outcome label.
    local_prover_fallback: Option<Arc<dyn TransactionProver + Send + Sync>>,
) -> anyhow::Result<PublishClaimTxn> {
    let faucet = find_or_create_faucet(
        params.originTokenAddress,
        params.originNetwork,
        &params.metadata,
        store,
        client,
        accounts,
    )
    .await?;

    tracing::info!(
        global_index = %params.globalIndex,
        origin_network = %params.originNetwork,
        dest_address = %params.destinationAddress,
        amount = %params.amount,
        faucet_id = %crate::accounts_config::AccountIdBech32(faucet.id),
        mainnet_exit_root = %alloy::hex::encode(params.mainnetExitRoot.0),
        rollup_exit_root = %alloy::hex::encode(params.rollupExitRoot.0),
        "creating CLAIM note"
    );

    let claim_note = create_claim(
        params.clone(),
        faucet,
        accounts,
        store,
        client.rng(),
        reject_zero_padding,
    )
    .await?;
    let claim_note_id = claim_note.id().to_string();
    // The note's details-commitment, encoded identically to how the projector
    // keys consumed notes (`InputNoteRecord::details_commitment()`). This ties
    // the real claim eth-tx to the on-chain CLAIM note so the SyntheticProjector
    // can emit the ClaimEvent under the REAL tx hash (which carries the
    // `claimAsset` calldata aggkit decodes for the claim's GER boundary) instead
    // of a derived hash whose synthetic tx has empty calldata.
    let note_commitment = hex::encode(
        miden_protocol::note::NoteDetails::from(&claim_note)
            .commitment()
            .as_bytes(),
    );

    let expires_at = latest_block_num + claim_receipt_expiration_blocks();

    // Cantina #21 — the GER→bridge-account propagation wait now happens ONCE,
    // synchronously, at injection time (`ger::insert_ger` blocks in
    // `wait_for_ger_on_bridge` until the bridge account's GER map reflects the
    // GER). The CLAIM note's FPI calls `assert_valid_ger`, which checks that same
    // bridge-account GER storage, so by the time a CLAIM referencing that GER
    // reaches here the account already carries it and this bounded poll early-exits
    // on its FIRST iteration (~0s) — replacing the old unconditional 5×3s = 15s
    // sleep that fired on EVERY claim (manual and auto-claim paths alike) even when
    // the GER was already present.
    //
    // We keep a short bounded safety poll for the rare case where THIS process did
    // not inject the GER (e.g. it was injected by a prior instance and is not yet
    // reflected in this client's synced view of the bridge account): the poll
    // early-exits the instant the condition holds. On timeout we submit anyway —
    // the on-chain MASM `assert_valid_ger` is the hard gate, so a CLAIM without the
    // GER fails closed with `ERR_GER_NOT_FOUND` rather than minting (identical to
    // the old best-effort behaviour, which also submitted unconditionally after the
    // pad).
    let claim_ger = crate::ger::combined_ger(&params.mainnetExitRoot.0, &params.rollupExitRoot.0);
    tracing::info!(
        "Cantina #21: awaiting GER on bridge account before submitting CLAIM (early-exits when \
         already present)..."
    );
    match crate::ger::wait_for_ger_on_bridge(
        client,
        accounts.bridge.0,
        ExitRoot::new(claim_ger),
        15,
        std::time::Duration::from_secs(1),
    )
    .await
    {
        Ok(true) => {
            tracing::info!("Cantina #21: GER present on bridge account; submitting CLAIM note")
        }
        Ok(false) => {
            ::metrics::counter!("rpc_claim_ger_wait_timeout_total").increment(1);
            tracing::warn!(
                "Cantina #21: GER not observed on bridge account within wait budget; submitting \
                 CLAIM anyway (fails closed with ERR_GER_NOT_FOUND if genuinely absent)"
            );
        }
        Err(e) => {
            tracing::warn!(
                error = ?e,
                "Cantina #21: error awaiting GER on bridge account; submitting CLAIM anyway"
            );
        }
    }

    let txn_request = TransactionRequestBuilder::new()
        .own_output_notes(vec![claim_note])
        .build()?;

    // Execute and check the output notes before submission. `ExecutedTransaction` still
    // produces `RawOutputNote::{Full, Partial}`, but the proven transaction now produces
    // `OutputNote::{Public, Private}` — 0.14.x renamed the final-form variants.
    let tx_result = client
        .execute_transaction(accounts.service.0, txn_request)
        .await?;
    let exec_tx = tx_result.executed_transaction();
    for (i, note) in exec_tx.output_notes().iter().enumerate() {
        let variant = match note {
            miden_protocol::transaction::RawOutputNote::Full(_) => "Full",
            miden_protocol::transaction::RawOutputNote::Partial(_) => "Partial",
        };
        tracing::info!(note_idx = i, variant = %variant, "executed tx output note");
    }

    // The CLAIM hot path is the ONLY site that calls `prove_transaction`
    // explicitly (every other site goes through `submit_new_transaction`,
    // which performs execute+prove+submit+sync as one unit). That makes
    // this the only site where we can wire the remote-prover →
    // local-prover fallback cleanly: a failed `prove_transaction` doesn't
    // mutate any node state, so we can safely retry against a different
    // prover before calling `submit_proven_transaction`.
    //
    // When `--miden-prover-fallback-to-local` is set, the surrounding
    // `MidenClient` builds and exposes a single shared
    // `LocalTransactionProver` (`local_prover_fallback` parameter,
    // plumbed in from `attempt_publish_claim`). When it's unset, the
    // parameter is `None` and the retry block is skipped — matching the
    // bali OOM-fix default (fail rather than silently double the prover
    // workload).
    //
    // The fallback is wired inline (rather than through a combined
    // `meter_proof_with_fallback` helper) because both attempts need
    // `&mut client` and the borrow checker won't accept two closures
    // capturing the same mutable reference, even though they execute
    // sequentially. `record_primary_attempt` / `record_fallback_attempt`
    // centralise the metric label set in `metrics.rs`.
    let has_fallback = local_prover_fallback.is_some();
    let primary_start = std::time::Instant::now();
    let primary_res = client.prove_transaction(&tx_result).await;
    let primary_elapsed = primary_start.elapsed().as_secs_f64();
    let (primary_res, retry_outcome) = crate::metrics::record_primary_attempt(
        crate::metrics::ProofKind::Claim,
        primary_res,
        primary_elapsed,
        has_fallback,
    );
    let proven_tx = match primary_res {
        Ok(p) => p,
        Err(e) => {
            if let (Some(prover), Some(failure)) = (local_prover_fallback, retry_outcome) {
                tracing::warn!(
                    error = %e,
                    primary_outcome = failure.as_label(),
                    "remote prover failed, retrying CLAIM proof against local fallback",
                );
                let fb_start = std::time::Instant::now();
                let fb_res = client.prove_transaction_with(&tx_result, prover).await;
                let fb_elapsed = fb_start.elapsed().as_secs_f64();
                crate::metrics::record_fallback_attempt(
                    crate::metrics::ProofKind::Claim,
                    fb_res,
                    fb_elapsed,
                )?
            } else {
                return Err(e.into());
            }
        }
    };
    for (i, note) in proven_tx.output_notes().iter().enumerate() {
        let variant = match note {
            miden_protocol::transaction::OutputNote::Public(_) => "Public",
            miden_protocol::transaction::OutputNote::Private(_) => "Private",
        };
        tracing::info!(note_idx = i, variant = %variant, "proven tx output note");
    }

    let txn_id = tx_result.executed_transaction().id();
    // `--read-only` guard: the CLAIM hot path is the one site that submits
    // via `submit_proven_transaction` instead of the guarded
    // `miden_client::submit_new_transaction` wrapper (it needs the explicit
    // prove step for the remote→local prover fallback above), so it must
    // call the chokepoint check itself before touching the node.
    crate::miden_client::ensure_writable(accounts.service.0)?;
    let _submission_height = client
        .submit_proven_transaction(proven_tx, &tx_result)
        .await?;
    client
        .apply_transaction(&tx_result, _submission_height)
        .await?;
    tracing::info!("submitted claim note txn: {txn_id}, claim_note_id: {claim_note_id}");

    // Cantina #7: record the submitted CLAIM in the expected-MINT tracker
    // BEFORE awaiting commit. If wait_for_transaction_commit times out (20s)
    // and we bail!, the entry remains in the tracker. The bridge_out
    // scanner's tick path then escalates to StaleAlert per global_index,
    // giving on-call a list of stuck CLAIMs by L1 leaf. On successful
    // commit (the next code block) we mark_landed to drop the entry.
    if let Some(tracker) = expected_mints {
        let global_index_bytes: [u8; 32] = params.globalIndex.to_be_bytes();
        let claim_id_bytes: [u8; 32] = tx_result
            .executed_transaction()
            .output_notes()
            .iter()
            .map(|n| match n {
                miden_protocol::transaction::RawOutputNote::Full(full) => full.id().as_bytes(),
                miden_protocol::transaction::RawOutputNote::Partial(partial) => {
                    partial.id().as_bytes()
                }
            })
            .next()
            .unwrap_or_default();
        if claim_id_bytes != [0u8; 32]
            && let Err(e) = tracker
                .record_expected(global_index_bytes, claim_id_bytes)
                .await
        {
            // RD-913: tracker is now store-backed. A store hiccup here
            // means we won't get a StaleAlert later if the MINT is
            // censored — log it loudly, but don't fail the CLAIM
            // submission itself (the claim has been submitted at this
            // point; refusing to return would just mean the user can't
            // get a receipt for a tx that already went on-chain).
            tracing::warn!(
                target: "claim",
                global_index = ?global_index_bytes,
                error = ?e,
                "RD-913: expected-MINT record store failure; no staleness alert will fire"
            );
        }
    }

    let committed = crate::miden_client::wait_for_transaction_commit(
        client,
        txn_id,
        20,
        std::time::Duration::from_secs(1),
    )
    .await?;
    if committed {
        tracing::info!("claim tx {txn_id} committed to block");
        // Cantina #7: mark Landed once `wait_for_transaction_commit`
        // confirms the CLAIM tx was committed. Aggkit's miden-client
        // operates on the proxy's service account — it CANNOT observe
        // the bridge account's consumption of our CLAIM via
        // NoteFilter::Consumed (the consumed-set returned by miden-client
        // is restricted to our tracked accounts, not the bridge's). The
        // commit confirmation is the right closure point: from there,
        // the bridge's MINT emission is deterministic, and tracking
        // longer would only fire spurious StaleAlerts.
        //
        // We still keep the record_expected → tick path useful: any
        // CLAIM that fails to commit (tx not in block within 20s) does
        // NOT reach this branch, so the tracker entry remains and the
        // tick eventually escalates to StaleAlert with the global_index
        // for operator triage.
        if let Some(tracker) = expected_mints {
            let global_index_bytes: [u8; 32] = params.globalIndex.to_be_bytes();
            if let Err(e) = tracker.mark_landed(global_index_bytes).await {
                tracing::warn!(
                    target: "claim",
                    global_index = ?global_index_bytes,
                    error = ?e,
                    "RD-913: mark_landed store failure; staleness tick will eventually \
                     time the entry out (one-shot StaleAlert)"
                );
            }
        }
    } else {
        anyhow::bail!("claim tx {txn_id} was submitted but not committed within 20s");
    }

    Ok(PublishClaimTxn {
        txn_id,
        expires_at,
        note_commitment,
    })
}

/// Publish a claim through the long-lived `MidenClient` event loop.
///
/// All Miden submissions — claim publishes and aggoracle `insert_ger` pushes
/// alike — funnel through `MidenClient::with(...)`, which serialises every
/// request through a `mpsc::channel::<Request>(1)` (see `miden_client.rs:126`).
/// That FIFO serialisation is what makes this design correct on bali:
///
///   - **No concurrent submissions for the same account.** The Miden node
///     rejects a second tx that builds atop the same `init_commitment` as a
///     pending mempool tx with `AddTransactionError::IncorrectAccountInitialCommitment`
///     (`code: 'Client specified an invalid argument', message: "transaction
///     conflicts with current mempool state"`). The bali production incident
///     fired this 189 times over 2026-05-11 → 2026-05-14 because the previous
///     fresh-per-call code path raced aggoracle's `insert_ger` against
///     claim publishes on the same `bridge`/`service` account. The channel-of-1
///     makes that race structurally impossible.
///
///   - **Single in-memory account cache.** Building a fresh `Client` against
///     the same `store.sqlite3` produced a divergent in-memory commitment
///     cache between the two clients (the long-lived one's cache stayed at
///     the pre-claim commitment until its next `sync_state()` tick, ~5s
///     later). Routing through the long-lived client eliminates the second
///     cache entirely.
///
///   - **TOCTOU safety for first-bridge faucet creation** (finding #10
///     non-atomic registration). Today `with()` awaits the whole closure on the
///     single client task, so the check→deploy→register→persist sequence is
///     already serialised — but that is incidental. The single-flight coordinator
///     ([`coordinate_faucet_provision`]) around `find_or_create_faucet` makes the
///     per-`(address, network)` dedup EXPLICIT and refactor-proof: a concurrent
///     second first-claim awaits the winner's result and never reaches the
///     provisioning path, even if faucet creation is ever moved off this
///     serialised task. See the `find_or_create_faucet` docstring and
///     `FAUCET_INFLIGHT`.
///
/// Recording the PENDING claim receipt (`txn_begin`) + the note↔tx link happens
/// inside the same closure, before the caller receives a response, so they are
/// durable even if the HTTP client disconnects (cancellation-safe). The
/// SyntheticProjector emits the `ClaimEvent` and finalises this receipt (at the
/// Miden consumption block) when it observes the CLAIM note consumed — no
/// synthetic log, tip advance, or receipt completion happens in this path.
#[allow(clippy::too_many_arguments)]
pub async fn publish_claim(
    params: claimAssetCall,
    client: &MidenClient,
    accounts: crate::AccountsConfig,
    store: Arc<dyn Store>,
    latest_block_num: BlockNumber,
    txn_hash: alloy::primitives::TxHash,
    txn_envelope: alloy::consensus::TxEnvelope,
    signer: alloy::primitives::Address,
    reject_zero_padding: bool,
    expected_mints: Option<Arc<crate::expected_mint_tracker::ExpectedMintTracker>>,
) -> anyhow::Result<PublishClaimTxn> {
    // Submit with runtime self-heal, mirroring the pattern in
    // `src/ger.rs::insert_ger`. If the inner Miden submission rejects with
    // `AccountDataNotFound` (local sqlite row missing — typically after a
    // `--reset-miden-store`) or `IncorrectAccountInitialCommitment` (local
    // commitment stale vs. the node), reimport every account from
    // `bridge_accounts.toml` and retry the publish once. Defense in depth
    // alongside the structural fix in `e3e3e2a` that routes through
    // `MidenClient::with(...)` and eliminates mempool-conflict IAIC.
    //
    // The claim flow touches several accounts (`bridge` for the CLAIM note,
    // `service` and dynamically-created faucets for first-bridge token
    // registration), so we reimport the whole bridge_accounts set rather
    // than guess which account was the culprit from the error message.
    // `reimport_known_accounts` is best-effort and idempotent — accounts
    // not on chain (e.g. `wallet_hardhat`) fail benignly.
    match attempt_publish_claim(
        params.clone(),
        client,
        accounts.clone(),
        store.clone(),
        latest_block_num,
        txn_hash,
        txn_envelope.clone(),
        signer,
        reject_zero_padding,
        expected_mints.clone(),
    )
    .await
    {
        Ok(value) => Ok(value),
        Err(err) if crate::account_recovery::is_recoverable_account_error(&err) => {
            tracing::warn!(
                err = %err,
                eth_tx = %txn_hash,
                "publish_claim: recoverable account error, reimporting known accounts and retrying"
            );
            crate::account_recovery::reimport_known_accounts(client, &accounts.0).await;
            attempt_publish_claim(
                params,
                client,
                accounts,
                store,
                latest_block_num,
                txn_hash,
                txn_envelope,
                signer,
                reject_zero_padding,
                expected_mints,
            )
            .await
        }
        Err(err) => Err(err),
    }
}

#[allow(clippy::too_many_arguments)]
async fn attempt_publish_claim(
    params: claimAssetCall,
    client: &MidenClient,
    accounts: crate::AccountsConfig,
    store: Arc<dyn Store>,
    latest_block_num: BlockNumber,
    txn_hash: alloy::primitives::TxHash,
    txn_envelope: alloy::consensus::TxEnvelope,
    signer: alloy::primitives::Address,
    reject_zero_padding: bool,
    expected_mints: Option<Arc<crate::expected_mint_tracker::ExpectedMintTracker>>,
) -> anyhow::Result<PublishClaimTxn> {
    // Snapshot the opt-in local-prover fallback BEFORE entering the
    // `client.with(...)` closure — the closure receives a
    // `&mut MidenClientLib` (the inner client), not the outer
    // `MidenClient` that owns the fallback Arc. Reading it here once and
    // moving the `Option<Arc<_>>` into the closure keeps the proof-call
    // site cancellation-safe and avoids any per-claim allocation of a new
    // `LocalTransactionProver`.
    let local_prover_fallback = client.local_prover_fallback();
    let result = Arc::new(OnceLock::<PublishClaimTxn>::new());
    let result_inner = result.clone();
    client
        .with(move |client| {
            Box::new(async move {
                let value = publish_claim_internal(
                    params,
                    client,
                    &accounts.0,
                    &*store,
                    latest_block_num,
                    reject_zero_padding,
                    expected_mints.as_ref(),
                    local_prover_fallback,
                )
                .await?;
                // The SyntheticProjector is the sole synthetic-event producer AND the
                // sole finaliser of this receipt: when it observes the CLAIM note
                // consumed it emits the ClaimEvent AND `txn_commit`s this tx at that
                // Miden block — so the receipt block == the log block. This path
                // records ONLY a PENDING receipt (txn_begin) + the tx↔note link below.
                store
                    .txn_begin(
                        txn_hash,
                        crate::store::TxnEntry {
                            // id: None hides this tx from the StoreSyncListener's
                            // commit-pending sweep (which finalises by Miden tx id at
                            // the note's CREATION block); the projector finalises it
                            // at the CONSUMPTION block instead.
                            id: None,
                            envelope: txn_envelope,
                            signer,
                            expires_at: Some(value.expires_at),
                            logs: vec![],
                        },
                    )
                    .await?;
                // Tie the real claim eth-tx to the on-chain CLAIM note so the
                // SyntheticProjector emits the ClaimEvent under THIS tx hash —
                // whose tx carries the `claimAsset` calldata aggkit decodes for the
                // claim's GER boundary — instead of a derived hash with empty
                // calldata (which made aggkit's L2BridgeSyncer fail
                // "input too short: 0 bytes" and stall certificate settlement).
                store
                    .record_tx_note_link(&format!("{txn_hash:#x}"), &value.note_commitment)
                    .await?;
                tracing::info!(
                    eth_tx = %txn_hash,
                    "claim tx recorded pending + note↔tx link; projector finalises \
                     receipt + ClaimEvent on consumption (cancellation-safe)"
                );
                let _ = result_inner.set(value);
                Ok(())
            })
        })
        .await?;
    result
        .get()
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("publish_claim: closure completed but result was not set"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::memory::InMemoryStore;
    use crate::test_helpers::seed_test_faucets;
    use alloy::primitives::address;

    #[test]
    fn test_metadata_hash_empty() {
        // Empty metadata → keccak256("") → 0xc5d246...a470. This is what the L1 bridge
        // contract puts in `leaf_data.metadata_hash` for native ETH deposits.
        let hash = MetadataHash::from_abi_encoded(&[]);
        let expected =
            hex::decode("c5d2460186f7233c927e7db2dcc703c0e500b653ca82273b7bfad8045d85a470")
                .unwrap();
        assert_eq!(hash.as_bytes(), expected.as_slice());
    }

    #[tokio::test]
    async fn test_find_faucet_eth_from_store() {
        let store = InMemoryStore::new();
        seed_test_faucets(&store).await;
        let entry = store
            .get_faucet_by_origin(&[0u8; 20], 0)
            .await
            .unwrap()
            .expect("ETH faucet should be registered");
        assert_eq!(entry.origin_decimals, 18);
        assert_eq!(entry.miden_decimals, 8);
        assert_eq!(entry.symbol, "ETH");
    }

    fn faucet_entry(faucet_id: AccountId, origin: [u8; 20], network: u32) -> FaucetEntry {
        FaucetEntry {
            faucet_id,
            origin_address: origin,
            origin_network: network,
            symbol: "TKN".into(),
            origin_decimals: 18,
            miden_decimals: 8,
            scale: 10,
            metadata: Vec::new(),
        }
    }

    /// Finding #10 / Cantina #10 — **single-flight** regression. N concurrent
    /// first-claims for the SAME origin token must run the provisioning path
    /// EXACTLY ONCE. This drives the real coordinator
    /// [`coordinate_faucet_provision`] with a counting fake provisioner (no live
    /// Miden node): the "deploy" is stubbed by a short yield + `register_faucet`.
    ///
    /// The mutex design could not make this guarantee at the type level — the
    /// loser still ENTERED the critical section and re-read the store. Under
    /// single-flight, exactly one caller becomes the PROVISIONER (its closure
    /// runs once, `provision_calls == 1`); every other caller is an AWAITER that
    /// never runs the closure and resolves to the winner's published faucet.
    #[tokio::test]
    async fn finding_10_concurrent_first_claims_deploy_single_faucet() {
        use crate::store::memory::InMemoryStore;
        use std::sync::atomic::{AtomicUsize, Ordering};

        let store: Arc<InMemoryStore> = Arc::new(InMemoryStore::new());
        // Unique origin so the process-global FAUCET_INFLIGHT map can't be
        // perturbed by another test using the same key.
        let origin = [0x9Au8; 20];
        let network = 0u32;
        let provision_calls = Arc::new(AtomicUsize::new(0));

        // Two DISTINCT faucet ids, alternated across the N concurrent claims: an
        // awaiter that (wrongly) returned its OWN id instead of the winner's
        // would then diverge from the single persisted faucet and trip the
        // final assertion.
        let id_a = AccountId::from_hex("0xac0000000000dd110000ee000000fc").unwrap();
        let id_b = AccountId::from_hex("0xaa0000000000bc110000bc000000de").unwrap();

        const N: usize = 8;
        let mut handles = Vec::with_capacity(N);
        for i in 0..N {
            let store = store.clone();
            let calls = provision_calls.clone();
            let my_id = if i % 2 == 0 { id_a } else { id_b };
            handles.push(tokio::spawn(async move {
                coordinate_faucet_provision((origin, network), move || async move {
                    // The PROVISIONING path — recorded exactly once by single-flight.
                    calls.fetch_add(1, Ordering::SeqCst);
                    // Yield long enough for the other first-claims to register as
                    // awaiters and contend, guaranteeing the race is exercised.
                    tokio::time::sleep(std::time::Duration::from_millis(25)).await;
                    store
                        .register_faucet(faucet_entry(my_id, origin, network))
                        .await
                        .unwrap();
                    Ok(Faucet {
                        id: my_id,
                        decimals: 8,
                        origin_token_decimals: 18,
                    })
                })
                .await
            }));
        }

        let mut resolved_ids = Vec::with_capacity(N);
        for h in handles {
            match h.await.unwrap() {
                FaucetProvisionOutcome::Settled(Ok(f)) => resolved_ids.push(f.id),
                FaucetProvisionOutcome::Settled(Err(e)) => {
                    panic!("no first-claim should fail here: {e:#}")
                }
                FaucetProvisionOutcome::PeerFailedRetry => {
                    panic!("no provisioner failed, so no caller should be told to retry")
                }
            }
        }

        // EXACTLY ONE provisioning attempt across all N concurrent first-claims.
        assert_eq!(
            provision_calls.load(Ordering::SeqCst),
            1,
            "the provisioning path must run exactly once (single-flight)"
        );
        // Exactly one faucet deployed+registered for this origin.
        let all = store.list_faucets().await.unwrap();
        assert_eq!(
            all.len(),
            1,
            "only one faucet must be created for the origin"
        );
        let winner = all[0].faucet_id;
        // Every concurrent claim resolved to the SAME winning faucet — awaiters
        // got the winner's published summary, not their own id.
        assert_eq!(resolved_ids.len(), N);
        assert!(
            resolved_ids.iter().all(|id| *id == winner),
            "all awaiters must resolve to the single winning faucet {winner}, got {resolved_ids:?}"
        );
        // The bridge-out resolve path finds the canonical faucet.
        let resolved = store.get_faucet_by_origin(&origin, network).await.unwrap();
        assert_eq!(resolved.unwrap().faucet_id, winner);
    }

    /// Finding #10 — PROVISIONER-FAILS case. When the sole provisioner fails,
    /// concurrent awaiters must NOT hang: they observe [`FaucetProvisionOutcome::PeerFailedRetry`]
    /// (a sound, terminal signal). The in-flight entry is then cleared, so a
    /// subsequent provisioner — the retry `find_or_create_faucet` performs on
    /// `PeerFailedRetry` — can run and succeed.
    #[tokio::test]
    async fn finding_10_provisioner_failure_awaiters_retry_no_hang() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        // Unique origin (distinct from the exactly-once test) for isolation.
        let origin = [0x9Bu8; 20];
        let attempts = Arc::new(AtomicUsize::new(0));

        // Cohort 1: the sole provisioner FAILS.
        const N: usize = 6;
        let mut handles = Vec::with_capacity(N);
        for _ in 0..N {
            let attempts = attempts.clone();
            handles.push(tokio::spawn(async move {
                coordinate_faucet_provision((origin, 0u32), move || async move {
                    attempts.fetch_add(1, Ordering::SeqCst);
                    // Yield so the peers register as awaiters before we fail.
                    tokio::time::sleep(std::time::Duration::from_millis(25)).await;
                    Err::<Faucet, _>(anyhow::anyhow!("simulated deploy/bridge-register failure"))
                })
                .await
            }));
        }

        let mut settled_err = 0usize;
        let mut retry = 0usize;
        for h in handles {
            // Each handle resolving at all proves no awaiter hung.
            match h.await.unwrap() {
                FaucetProvisionOutcome::Settled(Ok(_)) => panic!("provision was rigged to fail"),
                FaucetProvisionOutcome::Settled(Err(_)) => settled_err += 1,
                FaucetProvisionOutcome::PeerFailedRetry => retry += 1,
            }
        }

        // Exactly one provisioner ran (and surfaced its own error); every awaiter
        // got a retry signal rather than blocking forever.
        assert_eq!(
            attempts.load(Ordering::SeqCst),
            1,
            "only one provisioner runs"
        );
        assert_eq!(
            settled_err, 1,
            "the sole provisioner surfaces its own error"
        );
        assert_eq!(
            retry,
            N - 1,
            "every awaiter gets a retry signal, not a hang"
        );

        // The in-flight entry was cleared, so a fresh provisioner can now run and
        // succeed — this is exactly the retry `find_or_create_faucet` takes.
        let ok_id = AccountId::from_hex("0xac0000000000dd110000ee000000fc").unwrap();
        let outcome = coordinate_faucet_provision((origin, 0u32), || async move {
            Ok(Faucet {
                id: ok_id,
                decimals: 8,
                origin_token_decimals: 18,
            })
        })
        .await;
        match outcome {
            FaucetProvisionOutcome::Settled(Ok(f)) => assert_eq!(f.id, ok_id),
            FaucetProvisionOutcome::Settled(Err(e)) => {
                panic!("retry provisioner must succeed, got error: {e:#}")
            }
            FaucetProvisionOutcome::PeerFailedRetry => {
                panic!("in-flight entry was not cleared after provisioner failure")
            }
        }
    }

    #[tokio::test]
    async fn test_find_faucet_unknown_returns_none() {
        let store = InMemoryStore::new();
        seed_test_faucets(&store).await;
        // Address not registered in the test seed
        let entry = store.get_faucet_by_origin(&[0xBB; 20], 0).await.unwrap();
        assert!(entry.is_none());
    }

    /// Finding #10 (post-agglayer #2860) — the single-flight coordinator keys by
    /// the `(origin_address, origin_network)` asset identity, NOT the address
    /// alone. Concurrent first-claims for the SAME 20-byte token address on TWO
    /// different origin networks are TWO DISTINCT assets: per
    /// `bridge_config.masm`'s `store_faucet_registration`, the on-chain
    /// `token_registry_map` is keyed on `hash(tokenAddress || origin_network)`
    /// (the agglayer #2860 fix). So each network must run its OWN provisioner and
    /// deploy its OWN faucet — no cross-network leakage, and NO refusal (the
    /// obsolete pre-#2860 "Cantina #1" refusal, which assumed address-only
    /// keying, is removed).
    ///
    /// This inverts the old `cantina_1_*_refuses_cross_network_collision` test
    /// (which asserted a bail!): an address-only single-flight key would let a
    /// net-1 caller awaiting the net-0 provisioner receive net-0's faucet (the
    /// wrong network's faucet). Here we assert the provision closure runs EXACTLY TWICE (once
    /// per network), two distinct faucet_ids persist, and every caller resolves
    /// to its own network's faucet.
    #[tokio::test]
    async fn finding_10_concurrent_same_address_different_network_two_distinct_faucets() {
        use crate::store::memory::InMemoryStore;
        use std::sync::atomic::{AtomicUsize, Ordering};

        let store: Arc<InMemoryStore> = Arc::new(InMemoryStore::new());
        // Unique origin so the process-global FAUCET_INFLIGHT map can't be
        // perturbed by another test using the same address.
        let origin = [0x9Cu8; 20];
        let provision_calls = Arc::new(AtomicUsize::new(0));

        // One distinct faucet id per origin network — the two assets the
        // `(address, network)`-keyed registry keeps apart on-chain (#2860).
        let id_net0 = AccountId::from_hex("0xac0000000000dd110000ee000000fc").unwrap();
        let id_net1 = AccountId::from_hex("0xaa0000000000bc110000bc000000de").unwrap();

        const PER_NET: usize = 4;
        let mut handles = Vec::with_capacity(PER_NET * 2);
        // Interleave the two networks' callers so both cohorts contend on the map.
        for i in 0..(PER_NET * 2) {
            let network = (i % 2) as u32; // 0, 1, 0, 1, ...
            let my_id = if network == 0 { id_net0 } else { id_net1 };
            let store = store.clone();
            let calls = provision_calls.clone();
            handles.push(tokio::spawn(async move {
                let outcome = coordinate_faucet_provision((origin, network), move || async move {
                    // PROVISIONING path — must run once PER NETWORK (twice total).
                    calls.fetch_add(1, Ordering::SeqCst);
                    // Yield so the peer callers register as awaiters and contend.
                    tokio::time::sleep(std::time::Duration::from_millis(25)).await;
                    store
                        .register_faucet(faucet_entry(my_id, origin, network))
                        .await
                        .unwrap();
                    Ok(Faucet {
                        id: my_id,
                        decimals: 8,
                        origin_token_decimals: 18,
                    })
                })
                .await;
                (network, outcome)
            }));
        }

        let mut net0_ids = Vec::new();
        let mut net1_ids = Vec::new();
        for h in handles {
            let (network, outcome) = h.await.unwrap();
            match outcome {
                FaucetProvisionOutcome::Settled(Ok(f)) => {
                    if network == 0 {
                        net0_ids.push(f.id)
                    } else {
                        net1_ids.push(f.id)
                    }
                }
                FaucetProvisionOutcome::Settled(Err(e)) => {
                    panic!("no first-claim should fail here: {e:#}")
                }
                FaucetProvisionOutcome::PeerFailedRetry => {
                    panic!("no provisioner failed, so no caller should be told to retry")
                }
            }
        }

        // EXACTLY TWO provisioning attempts — one per (address, network) asset.
        // The address-only key of the pre-#2860 design would have run it once and
        // routed the second network's callers to the wrong faucet.
        assert_eq!(
            provision_calls.load(Ordering::SeqCst),
            2,
            "provisioning must run once per origin network (two distinct assets)"
        );

        // Two distinct faucets persisted, one per network.
        let all = store.list_faucets().await.unwrap();
        assert_eq!(all.len(), 2, "one faucet per (address, network) asset");
        let by_network: std::collections::BTreeMap<u32, AccountId> = all
            .iter()
            .map(|f| (f.origin_network, f.faucet_id))
            .collect();
        assert_eq!(by_network.get(&0), Some(&id_net0));
        assert_eq!(by_network.get(&1), Some(&id_net1));
        assert_ne!(
            id_net0, id_net1,
            "the two networks must get distinct faucets"
        );

        // No cross-network leakage: every net-0 caller resolved to net-0's faucet,
        // every net-1 caller to net-1's — awaiters got THEIR network's winner.
        assert_eq!(net0_ids.len(), PER_NET);
        assert_eq!(net1_ids.len(), PER_NET);
        assert!(
            net0_ids.iter().all(|id| *id == id_net0),
            "net-0 callers must all resolve to the net-0 faucet, got {net0_ids:?}"
        );
        assert!(
            net1_ids.iter().all(|id| *id == id_net1),
            "net-1 callers must all resolve to the net-1 faucet, got {net1_ids:?}"
        );

        // The bridge-out resolve path returns each network's own faucet.
        assert_eq!(
            store
                .get_faucet_by_origin(&origin, 0)
                .await
                .unwrap()
                .unwrap()
                .faucet_id,
            id_net0
        );
        assert_eq!(
            store
                .get_faucet_by_origin(&origin, 1)
                .await
                .unwrap()
                .unwrap()
                .faucet_id,
            id_net1
        );
    }

    #[test]
    fn test_metadata_hash_non_empty() {
        // Non-empty raw bytes → keccak256(bytes). Sanity check that
        // `MetadataHash::from_abi_encoded` is just keccak256, not ABI-aware.
        let hash = MetadataHash::from_abi_encoded(&[0x01, 0x02, 0x03]);
        let expected =
            hex::decode("f1885eda54b7a053318cd41e2093220dab15d65381b1157a3633a83bfd5c9239")
                .unwrap();
        assert_eq!(hash.as_bytes(), expected.as_slice());
    }

    /// Cantina #13 — bounded metadata is stored verbatim (preimage preserved so
    /// a later bridge-out emits the real ABI metadata).
    #[test]
    fn cap_stored_faucet_metadata_keeps_bounded() {
        let token = address!("00000000000000000000000000000000000000aa");
        let small = Bytes::from(vec![0xABu8; 128]);
        let stored = cap_stored_faucet_metadata(&small, &token);
        assert_eq!(stored, small.to_vec(), "bounded metadata must be preserved");

        // Exactly at the cap is still kept (boundary).
        let at_cap = Bytes::from(vec![
            0u8;
            crate::bridge_out::MAX_BRIDGE_EVENT_METADATA_BYTES
        ]);
        assert_eq!(
            cap_stored_faucet_metadata(&at_cap, &token).len(),
            crate::bridge_out::MAX_BRIDGE_EVENT_METADATA_BYTES,
            "metadata exactly at the cap must be stored, not dropped"
        );

        // Empty (native ETH) stays empty.
        assert!(cap_stored_faucet_metadata(&Bytes::new(), &token).is_empty());
    }

    /// Cantina #13 — oversized attacker-controlled metadata is stored as EMPTY,
    /// never the giant blob.
    #[test]
    fn cap_stored_faucet_metadata_drops_oversized() {
        let token = address!("00000000000000000000000000000000000000aa");
        let huge = Bytes::from(vec![
            0x42u8;
            crate::bridge_out::MAX_BRIDGE_EVENT_METADATA_BYTES + 1
        ]);
        let stored = cap_stored_faucet_metadata(&huge, &token);
        assert!(
            stored.is_empty(),
            "oversized metadata must be replaced with empty, got {} bytes",
            stored.len()
        );
    }

    #[test]
    fn test_claim_event_from_claim_asset_call() {
        use alloy::primitives::{Address, U256};

        let call = claimAssetCall {
            smtProofLocalExitRoot: [FixedBytes::ZERO; 32],
            smtProofRollupExitRoot: [FixedBytes::ZERO; 32],
            globalIndex: U256::from(42u64),
            mainnetExitRoot: FixedBytes::ZERO,
            rollupExitRoot: FixedBytes::ZERO,
            originNetwork: 1,
            originTokenAddress: Address::ZERO,
            destinationNetwork: 2,
            destinationAddress: address!("1234567890abcdef1234567890abcdef12345678"),
            amount: U256::from(1000u64),
            metadata: Default::default(),
        };
        let event = ClaimEvent::from(call);
        assert_eq!(event.globalIndex, U256::from(42u64));
        assert_eq!(event.originNetwork, 1);
        assert_eq!(event.amount, U256::from(1000u64));
    }

    /// `claim_storage_from_call` (the foreign-bridge e2e's storage builder)
    /// must produce storage that round-trips through the SAME decoders the
    /// projector uses on consumed CLAIM notes — pinning it byte-compatible
    /// with what `create_claim` puts on-chain.
    #[test]
    fn claim_storage_from_call_roundtrips_through_watcher_decoder() {
        use alloy::primitives::{Address, U256};
        use miden_protocol::note::NoteStorage;

        // Mainnet deposit: globalIndex = 2^64 (mainnet flag) + leaf 7, with
        // garbage in the rollup-index bytes that canonicalisation must zero.
        let mut gi = U256::from(7u64) + (U256::from(1u64) << 64);
        gi += U256::from(0xDEADu64) << 32; // rollup-index garbage (bytes 24..28)
        let call = claimAssetCall {
            smtProofLocalExitRoot: [FixedBytes::ZERO; 32],
            smtProofRollupExitRoot: [FixedBytes::from([0x11; 32]); 32],
            globalIndex: gi,
            mainnetExitRoot: FixedBytes::from([0xAA; 32]),
            rollupExitRoot: FixedBytes::from([0xBB; 32]),
            originNetwork: 0,
            originTokenAddress: Address::ZERO,
            destinationNetwork: 2,
            destinationAddress: address!("1234567890abcdef1234567890abcdef12345678"),
            amount: U256::from(10_000_000_000_000u64), // 10^13 wei
            metadata: Default::default(),
        };

        let storage = claim_storage_from_call(&call, 10).expect("storage builds");
        // 18→8 scaling: 10^13 / 10^10 = 1000 Miden units.
        assert_eq!(storage.miden_claim_amount.as_canonical_u64(), 1_000);

        let note_storage = NoteStorage::try_from(storage).expect("valid CLAIM storage layout");
        let decoded = crate::claim_watcher::parse_claim_event_from_storage(&note_storage)
            .expect("projector decoder accepts the storage");

        // Canonical global index: mainnet flag kept, rollup-index bytes zeroed.
        let mut expected_gi = [0u8; 32];
        expected_gi[23] = 1; // mainnet flag (limb 5)
        expected_gi[31] = 7; // leaf index
        assert_eq!(decoded.global_index, expected_gi);
        assert_eq!(decoded.origin_network, 0);
        assert_eq!(
            decoded.destination_address, call.destinationAddress.0.0,
            "leaf destination must round-trip"
        );
        assert_eq!(decoded.amount, 10_000_000_000_000u64);
    }

    #[test]
    fn test_bytes32_array_to_smt_nodes_converts() {
        let mut values = [FixedBytes::ZERO; 32];
        values[0] = FixedBytes::from([0xAA; 32]);
        values[31] = FixedBytes::from([0xBB; 32]);
        let nodes = bytes32_array_to_smt_nodes(values);
        // Verify we get back 32 nodes (basic structural check)
        assert_eq!(nodes.len(), 32);
    }

    mod scale_claim_amount {
        use super::*;
        use alloy::primitives::U256;
        use miden_protocol::Felt;
        use miden_protocol::account::AccountId;
        use std::ops::{Add, Mul};

        const DUMMY_ACCOUNT_HEX: &str = "0xac0000000000dd110000ee000000fc";

        fn faucet(origin_decimals: u8, miden_decimals: u8) -> Faucet {
            Faucet {
                id: AccountId::from_hex(DUMMY_ACCOUNT_HEX).unwrap(),
                decimals: miden_decimals,
                origin_token_decimals: origin_decimals,
            }
        }

        fn eth_amount(wei: U256) -> EthAmount {
            EthAmount::new(wei.to_be_bytes::<32>())
        }

        #[test]
        fn accepts_amount_above_old_u32_ceiling() {
            let wei = U256::from(43u64).mul(U256::from(10u64).pow(U256::from(18u64)));
            let amount = scale_claim_amount(&eth_amount(wei), faucet(18, 8)).unwrap();
            assert_eq!(amount, Felt::try_from(4_300_000_000u64).unwrap());
        }

        #[test]
        fn truncates_sub_unit_wei_remainder() {
            let wei = U256::from(42u64)
                .mul(U256::from(10u64).pow(U256::from(18u64)))
                .add(U256::from(1u64));
            let amount = scale_claim_amount(&eth_amount(wei), faucet(18, 8)).unwrap();
            assert_eq!(amount, Felt::try_from(4_200_000_000u64).unwrap());
        }

        #[test]
        fn rejects_amount_above_max_fungible_asset() {
            let err = scale_claim_amount(&eth_amount(U256::MAX), faucet(18, 8)).unwrap_err();
            let msg = format!("{err}");
            assert!(
                msg.contains("claim amount is not representable on Miden"),
                "unexpected error message: {msg}"
            );
        }

        /// Cantina #12 — repro+regression. The on-chain MASM
        /// `verify_u256_to_native_amount_conversion` advertises a 2^128 outer gate
        /// but the inner verifier algebra only succeeds for x < ~2^123; values in
        /// [2^123, 2^128) panic later with `ERR_UNDERFLOW`. Aggkit's scaling path
        /// goes through `EthAmount::scale_to_token_amount` which enforces the
        /// real protocol cap (`FungibleAsset::MAX_AMOUNT = 2^63 - 2^31`), so any
        /// amount that falls in the upstream gap is rejected here BEFORE we
        /// build a CLAIM note that would panic on Miden. This test pins that
        /// boundary so a future regression that loosens the cap (e.g. switches
        /// to a tighter or looser `try_from` path) is caught immediately.
        #[test]
        fn cantina_12_amount_cap_pins_fungible_asset_max() {
            use miden_client::asset::FungibleAsset;

            // Boundary: an amount that scales to exactly MAX_AMOUNT must succeed.
            let max_native = u64::from(FungibleAsset::MAX_AMOUNT); // 2^63 - 2^31
            // For an 18→8 decimal layout, scale = 10. Pre-image wei = max_native * 10^10.
            let wei_at_max = U256::from(max_native).mul(U256::from(10u64).pow(U256::from(10u64)));
            let amount = scale_claim_amount(&eth_amount(wei_at_max), faucet(18, 8))
                .expect("exact MAX_AMOUNT must be accepted");
            assert_eq!(amount, Felt::try_from(max_native).unwrap());

            // Off-by-one above MAX_AMOUNT must be rejected.
            let wei_just_over =
                U256::from(max_native + 1).mul(U256::from(10u64).pow(U256::from(10u64)));
            assert!(
                scale_claim_amount(&eth_amount(wei_just_over), faucet(18, 8)).is_err(),
                "MAX_AMOUNT + 1 must be rejected"
            );

            // An amount that would fall in the upstream MASM's [2^123, 2^128) gap
            // must also be rejected here. With scale=10, even 2^123 wei scales to
            // 2^123 / 10^10 ≈ 2^90, well above MAX_AMOUNT (2^63 - 2^31), so we
            // catch it before any MASM path could panic.
            let wei_in_gap = U256::from(1u64) << 123;
            assert!(
                scale_claim_amount(&eth_amount(wei_in_gap), faucet(18, 8)).is_err(),
                "2^123 wei must be rejected client-side (Cantina #12 gap)"
            );
        }

        #[test]
        fn passes_through_when_decimals_match() {
            let wei = U256::from(1_234_567u64);
            let amount = scale_claim_amount(&eth_amount(wei), faucet(6, 6)).unwrap();
            assert_eq!(amount, Felt::try_from(1_234_567u64).unwrap());
        }

        #[test]
        fn rejects_faucet_with_inverted_decimals() {
            let err = scale_claim_amount(&eth_amount(U256::from(1u64)), faucet(6, 8)).unwrap_err();
            let msg = format!("{err}");
            assert!(
                msg.contains("invariant violated"),
                "unexpected error: {msg}"
            );
        }
    }

    /// Cantina #11 — repro+regression. The on-chain CLAIM verifier's mainnet branch
    /// does not constrain `smt_proof_rollup_exit_root` (256 felts) or `rollup_exit_root`
    /// (8 felts) — any garbage prover supplies still folds into the note's RECIPIENT
    /// digest and PROOF_DATA_KEY, so equivalent mainnet claims with different rollup-side
    /// bytes produce different NoteIds. Aggkit canonicalises by zeroing those fields
    /// when the globalIndex's mainnet flag is set.
    mod cantina_11_canonical_mainnet_proof_data {
        use super::*;
        use alloy::primitives::{Address, Bytes, FixedBytes, U256};

        /// Build a claimAssetCall with a chosen mainnet flag and rollup-side garbage.
        fn make_call(mainnet: bool, rollup_garbage_byte: u8) -> claimAssetCall {
            // GlobalIndex layout: bytes 0..20 zero, bytes 20..24 mainnet flag (BE),
            // bytes 24..28 rollup index, bytes 28..32 leaf index.
            let mut gi = [0u8; 32];
            if mainnet {
                gi[23] = 1; // BE-low byte of the flag word
            } // else flag stays 0 = rollup
            gi[31] = 42; // leaf index = 42

            let smt_local: [FixedBytes<32>; 32] =
                std::array::from_fn(|i| FixedBytes([i as u8; 32]));
            let smt_rollup: [FixedBytes<32>; 32] =
                std::array::from_fn(|_| FixedBytes([rollup_garbage_byte; 32]));

            claimAssetCall {
                smtProofLocalExitRoot: smt_local,
                smtProofRollupExitRoot: smt_rollup,
                globalIndex: U256::from_be_bytes(gi),
                mainnetExitRoot: FixedBytes([0xAAu8; 32]),
                rollupExitRoot: FixedBytes([rollup_garbage_byte; 32]),
                originNetwork: 0,
                originTokenAddress: Address::ZERO,
                destinationNetwork: 1,
                destinationAddress: Address::ZERO,
                amount: U256::from(0u64),
                metadata: Bytes::new(),
            }
        }

        #[test]
        fn mainnet_claim_zeroes_rollup_proof_path_only() {
            let call = make_call(true, 0xCC);
            let proof = build_canonical_proof_data(&call);

            let zero_node = SmtNode::new([0u8; 32]);
            for n in proof.smt_proof_rollup_exit_root.iter() {
                assert_eq!(*n, zero_node, "mainnet smt_proof_rollup must be zeroed");
            }
            // rollup_exit_root MUST be preserved — it's read by `compute_ger` in
            // bridge_in.masm to derive the GER lookup key. Zeroing it broke the
            // dynamic-ERC20 e2e with ERR_GER_NOT_FOUND. See claim.rs docstring.
            assert_eq!(
                proof.rollup_exit_root,
                ExitRoot::new([0xCCu8; 32]),
                "mainnet rollup_exit_root must NOT be zeroed (load-bearing for compute_ger)"
            );
            // Mainnet exit root is preserved.
            assert_eq!(proof.mainnet_exit_root, ExitRoot::new([0xAAu8; 32]));
        }

        #[test]
        fn mainnet_claim_note_id_invariant_to_smt_proof_garbage() {
            // Two mainnet claims for the same leaf, but different smt_proof_rollup
            // garbage AND identical real (mainnet,rollup)-exit-root pair. Post-fix
            // the canonicalised ProofData must be byte-identical wrt the SMT proof
            // path (the only field genuinely unread by the bridge's mainnet branch
            // and therefore safely zeroable for NoteId determinism).
            //
            // We CANNOT canonicalise rollup_exit_root because it's used in the GER
            // keccak; the test pins the now-correct subset of the determinism
            // property.
            let mut call_a = make_call(true, 0x00);
            let mut call_b = make_call(true, 0xFF);
            // Force rollup_exit_root to match for both — this is what real claims
            // look like (the L1 GER manager dictates the value).
            call_a.rollupExitRoot = FixedBytes([0x33u8; 32]);
            call_b.rollupExitRoot = FixedBytes([0x33u8; 32]);

            let a = build_canonical_proof_data(&call_a);
            let b = build_canonical_proof_data(&call_b);
            assert_eq!(
                a.smt_proof_rollup_exit_root, b.smt_proof_rollup_exit_root,
                "rollup smt_proof must be canonical for mainnet (zeroed)"
            );
            assert_eq!(
                a.rollup_exit_root, b.rollup_exit_root,
                "rollup_exit_root must be preserved verbatim (=L1 GER manager value)"
            );
        }

        /// Regression for the dynamic-ERC20 e2e fix. ERR_GER_NOT_FOUND fired
        /// because the original canonicalisation zeroed `rollup_exit_root` for
        /// mainnet claims, but `compute_ger` in `bridge_in.masm` keccaks
        /// `mainnet_exit_root || rollup_exit_root` to derive the GER lookup key.
        /// This test pins that the canonicalised proof_data preserves whatever
        /// `rollup_exit_root` the caller supplied — including non-zero values
        /// from a live L1 GER manager.
        #[test]
        fn mainnet_claim_preserves_nonzero_rollup_exit_root_for_ger_lookup() {
            let mut call = make_call(true, 0);
            call.rollupExitRoot = FixedBytes([0x77u8; 32]); // simulates live L1
            let proof = build_canonical_proof_data(&call);
            assert_eq!(
                proof.rollup_exit_root,
                ExitRoot::new([0x77u8; 32]),
                "rollup_exit_root must reach the bridge unchanged (else ERR_GER_NOT_FOUND)"
            );
        }

        /// Self-review of-the-fix follow-up — the original Cantina #11 fix
        /// preserved the full `globalIndex` u256, including bytes 24..28 (limb
        /// 6 = rollup index). A malicious caller setting *both* the mainnet
        /// flag AND non-zero rollup-index bytes could still produce different
        /// NoteIds for the same mainnet leaf. This test pins the tightening:
        /// the rollup-index bytes are zeroed when the mainnet flag is set.
        #[test]
        fn mainnet_claim_zeroes_rollup_index_bytes_in_global_index() {
            // Build two mainnet claims where the rollup-index bytes (24..28)
            // differ, everything else identical.
            let mut gi_a = [0u8; 32];
            gi_a[23] = 1; // mainnet flag
            gi_a[24] = 0xAA; // attacker-supplied rollup-index garbage
            gi_a[31] = 42;

            let mut gi_b = gi_a;
            gi_b[24] = 0xBB;
            gi_b[25] = 0xCC;

            let mut call_a = make_call(true, 0);
            call_a.globalIndex = U256::from_be_bytes(gi_a);
            let mut call_b = make_call(true, 0);
            call_b.globalIndex = U256::from_be_bytes(gi_b);

            let a = build_canonical_proof_data(&call_a);
            let b = build_canonical_proof_data(&call_b);

            // After canonicalisation the GlobalIndex bytes must match — the
            // rollup-index garbage was zeroed for both.
            assert_eq!(
                a.global_index, b.global_index,
                "globalIndex rollup-index bytes must be zeroed for mainnet"
            );
        }

        #[test]
        fn rollup_claim_preserves_rollup_proof() {
            // Non-mainnet claims must NOT be canonicalised — those fields are load-bearing
            // for rollup-leaf verification AND the GER keccak.
            let call = make_call(false, 0xCC);
            let proof = build_canonical_proof_data(&call);

            let cc_node = SmtNode::new([0xCCu8; 32]);
            for n in proof.smt_proof_rollup_exit_root.iter() {
                assert_eq!(*n, cc_node, "rollup smt_proof must be preserved verbatim");
            }
            assert_eq!(proof.rollup_exit_root, ExitRoot::new([0xCCu8; 32]));
        }

        #[test]
        fn is_mainnet_global_index_decodes_layout() {
            let mut gi = [0u8; 32];
            assert!(!is_mainnet_global_index(&gi), "all-zero is rollup");
            gi[23] = 1;
            assert!(is_mainnet_global_index(&gi), "flag at byte 23 → mainnet");
            // Garbage outside the flag bytes must not flip the result.
            gi[20] = 0;
            gi[21] = 0;
            gi[22] = 0;
            gi[24] = 0xFF; // rollup index garbage
            gi[31] = 0xFF; // leaf index garbage
            assert!(is_mainnet_global_index(&gi));
            // Flag = 2 is technically out of spec but our decoder must only treat 1 as mainnet.
            gi[23] = 2;
            assert!(!is_mainnet_global_index(&gi), "flag must be exactly 1");
        }
    }

    /// Finding #17 — first-claim auto-creation derived `miden_decimals`
    /// dynamically (bumping it above 8 for 27..30-decimal tokens), which let it
    /// persist routes whose scale crossed the shared `MAX_SCALING_FACTOR` (18)
    /// limit and were UNCLAIMABLE. The audit-aligned fix (auditor cergyk
    /// recommended rejecting origin decimals above 26) caps faucet decimals at
    /// `MIDEN_DECIMALS` (8) via `min(origin, 8)` — so low-decimal tokens
    /// (6-decimal USDC/USDT) still route at scale 0 — and rejects any origin
    /// token whose decimals exceed `MAX_ORIGIN_DECIMALS` (26).
    mod finding_17_decimal_derivation {
        use crate::faucet_ops::{
            MAX_ORIGIN_DECIMALS, MAX_SCALING_FACTOR, MIDEN_DECIMALS, parse_token_metadata,
        };
        use alloy::primitives::{Address, Bytes, U256};
        use miden_base_agglayer::{EthAmount, EthAmountError};

        fn eth_amount(wei: U256) -> EthAmount {
            EthAmount::new(wei.to_be_bytes::<32>())
        }

        /// Build a minimal valid ABI metadata blob (`abi.encode(name, symbol,
        /// decimals)`) carrying a chosen `decimals` byte — enough for
        /// `parse_token_metadata` to reach (and either accept or reject at) the
        /// decimals gate.
        fn metadata_with_decimals(decimals: u8) -> Bytes {
            let mut data = vec![0u8; 224];
            data[31] = 0x60; // name offset
            data[63] = 0xa0; // symbol offset
            data[95] = decimals;
            data[127] = 1; // name = "T"
            data[128] = b'T';
            data[191] = 3; // symbol = "TKN"
            data[192..195].copy_from_slice(b"TKN");
            Bytes::from(data)
        }

        /// Documents the boundary the bug crossed: a 27-decimal token routes at
        /// `scale = 27 - min(27, 8) = 19`, which the shared scale gate
        /// (`EthAmount::scale_to_token_amount`) rejects. Under the audit-aligned
        /// fix such a token is refused up-front (27 > 26) instead of persisting an
        /// unclaimable route.
        #[test]
        fn service_scale_19_crosses_limit() {
            // 10^27 fits in U256; the only failure under test is the scale gate.
            let amount = eth_amount(U256::from(10u64).pow(U256::from(27u64)));
            let service_scale = 27u32 - u32::from(MIDEN_DECIMALS); // 27 - min(27,8) = 19
            assert_eq!(service_scale, 19);
            assert!(matches!(
                amount.scale_to_token_amount(service_scale),
                Err(EthAmountError::ScaleTooLarge)
            ));
        }

        /// Cap-at-`MIDEN_DECIMALS` (8) invariant, full-domain coverage. The local
        /// faucet declares `min(origin_decimals, 8)` decimals, so EVERY origin
        /// token in `0..=MAX_ORIGIN_DECIMALS (26)` gets a route, and each
        /// round-trips `10^d wei -> 10^min(d,8) token units` through the runtime
        /// `EthAmount` gate (no precision loss at the boundary):
        ///
        /// - `d in 0..=8`: faucet decimals = d, `scale = 0` — a 6-decimal
        ///   USDC/USDT token routes 1:1 (the regression this fix restores);
        /// - `d in 9..=26`: faucet decimals = 8, `scale = d - 8` fits
        ///   MAX_SCALING_FACTOR (18);
        /// - `d > 26`: `parse_token_metadata` rejects up-front (unclaimable /
        ///   overflow-prone) — checked explicitly for d in {27, 30, 255}.
        #[test]
        fn capped_miden_decimals_routes_0_to_26_and_rejects_above() {
            use miden_protocol::Felt;

            assert_eq!(MIDEN_DECIMALS, 8);
            assert_eq!(MAX_ORIGIN_DECIMALS, 26);
            let addr = Address::ZERO;

            for d in 0u8..=MAX_ORIGIN_DECIMALS {
                // Faucet decimals are capped at 8, never derived higher.
                let miden_decimals = d.min(MIDEN_DECIMALS);
                // `miden_decimals <= d` by construction, so this never underflows.
                let scale = d - miden_decimals;

                // Cap semantics: <= 8 routes 1:1 (scale 0); > 8 pins to 8.
                if d <= MIDEN_DECIMALS {
                    assert_eq!(miden_decimals, d, "d={d}: low-decimal token must route 1:1");
                    assert_eq!(scale, 0, "d={d}: scale must be 0");
                } else {
                    assert_eq!(
                        miden_decimals, MIDEN_DECIMALS,
                        "d={d}: high-decimal token pins to 8"
                    );
                    assert_eq!(scale, d - MIDEN_DECIMALS);
                }
                assert!(scale <= MAX_SCALING_FACTOR, "d={d}: scale {scale} > cap");

                // parse_token_metadata accepts the whole 0..=26 range and reports
                // the origin decimals unchanged — i.e. a route IS created.
                let (_symbol, parsed) = parse_token_metadata(&metadata_with_decimals(d), &addr)
                    .unwrap_or_else(|e| panic!("d={d} must parse: {e}"));
                assert_eq!(parsed, d);

                // Boundary round-trip: 10^d wei scales cleanly to exactly
                // 10^min(d,8) token units through the runtime gate. 10^d fits U256
                // (d <= 26) and 10^8 fits FungibleAsset::MAX_AMOUNT.
                let wei = U256::from(10u64).pow(U256::from(u64::from(d)));
                let token = eth_amount(wei)
                    .scale_to_token_amount(u32::from(scale))
                    .unwrap_or_else(|e| {
                        panic!("d={d}: scale {scale} rejected by EthAmount gate: {e}")
                    });
                let expected = 10u64.pow(u32::from(miden_decimals));
                assert_eq!(
                    token,
                    Felt::try_from(expected).unwrap(),
                    "d={d}: 10^{d} wei did not round-trip to 10^{miden_decimals} token units"
                );
            }

            // Above MAX_ORIGIN_DECIMALS the route would be unclaimable (scale > 18)
            // and overflow-prone: parse_token_metadata must reject rather than
            // persist a poisoned route. 27 = boundary, 30 = old dynamic cap, 255 =
            // u8 extreme.
            for d in [MAX_ORIGIN_DECIMALS + 1, 30, 255] {
                let err = parse_token_metadata(&metadata_with_decimals(d), &addr)
                    .expect_err(&format!("d={d} must be rejected"));
                assert!(
                    err.to_string().contains("decimals out of range"),
                    "d={d}: unexpected error: {err}"
                );
            }
        }
    }
}
