//! Admin RPC endpoint for explicit faucet registration.
//!
//! `admin_registerFaucet` creates a faucet on Miden, registers it in the bridge,
//! and saves its metadata to the Store. This is an alternative to auto-creation
//! during the first claim — useful for pre-staging tokens.

use crate::faucet_ops;
use crate::service_state::ServiceState;
use crate::store::FaucetEntry;
use miden_base_agglayer::MetadataHash;
use miden_protocol::account::AccountId;
use miden_standards::interop::eth::EthAddress;
use serde::Deserialize;
use std::sync::{Arc, OnceLock};

// Mirror of the upstream `miden-agglayer` `SolTokenMetadata` struct (its
// `encode_token_metadata` is `pub(crate)` so we can't call it directly). Encoding
// this with `abi_encode_params` reproduces Solidity's `abi.encode(string name,
// string symbol, uint8 decimals)` byte-for-byte, so `keccak256(bytes)` equals the
// faucet's `MetadataHash` (Cantina #13). A plain tuple `.abi_encode()` won't do —
// `u8` doesn't implement `SolValue`, and a dynamic tuple's `abi_encode` would add an
// extra offset word.
alloy_core::sol! {
    struct AdminTokenMetadata {
        string name;
        string symbol;
        uint8 decimals;
    }
}

#[derive(Debug, Deserialize)]
pub struct RegisterFaucetParams {
    pub symbol: String,
    pub origin_token_address: String,
    pub origin_network: u32,
    pub origin_decimals: u8,
    /// DEPRECATED / IGNORED. The local faucet decimals are computed as
    /// `min(origin_decimals, `[`faucet_ops::MIDEN_DECIMALS`]` (8))` — capped at 8
    /// (finding #17). The field is retained only for request-shape compatibility;
    /// whatever value a caller sends is discarded. Routability is decided purely
    /// by `origin_decimals` (must be `<= MAX_ORIGIN_DECIMALS (26)`).
    ///
    /// Accepted (via `serde`) for request-shape compatibility but deliberately
    /// never read — the value is discarded in favour of the fixed constant.
    #[serde(default)]
    #[allow(dead_code)]
    pub miden_decimals: u8,
    /// Token display name used when computing the `MetadataHash`. Optional —
    /// defaults to the symbol if not provided.
    #[serde(default)]
    pub name: Option<String>,
}

pub async fn admin_register_faucet(
    state: ServiceState,
    params: RegisterFaucetParams,
) -> anyhow::Result<String> {
    let origin_address = parse_eth_address(&params.origin_token_address)?;

    // Check if already registered FIRST — before any parameter validation.
    // `admin_registerFaucet` is strictly register-if-absent / return-existing-if-present:
    // an existing route for this `(origin_address, origin_network)` is ALWAYS returned
    // idempotently, regardless of the params supplied on the re-register call. Validating
    // before this lookup would break idempotency — an idempotent re-register that happened
    // to carry imperfect decimals would surface a validation error instead of the route
    // that already exists. Validation therefore gates ONLY new-route creation (below).
    // There is deliberately no live "replace" path — swapping a route would DELETE the old
    // (origin_address, origin_network) row and orphan any holder still carrying
    // balances in the old faucet, re-creating the Cantina finding #6 split-brain
    // (their bridge-outs would resolve to an "unknown faucet ID" and quarantine,
    // burning funds on L2 with no L1 claim). Disaster-recovery route repair, if
    // ever needed, is a purpose-built throwaway image, not a standing endpoint.
    if let Some(existing) = state
        .store
        .get_faucet_by_origin(&origin_address, params.origin_network)
        .await?
    {
        let id = existing.faucet_id.to_hex();
        tracing::info!(
            faucet_id = %id,
            "admin_registerFaucet: faucet already exists for this origin"
        );
        return Ok(id);
    }

    // New-route creation path only. The faucet decimals are capped at
    // `MIDEN_DECIMALS` (8): `miden_decimals = min(origin_decimals, 8)` (finding
    // #17). The caller's `params.miden_decimals` is IGNORED (a route can never be
    // created with a caller-chosen decimal count). A low-decimal origin token
    // (e.g. 6-decimal USDC/USDT) routes 1:1 at scale 0; a high-decimal token pins
    // to 8. Routability then reduces to a single check on the origin token: the
    // downscaling factor `scale = origin_decimals - min(origin_decimals, 8)` must
    // fit MAX_SCALING_FACTOR (18, enforced at runtime by
    // `EthAmount::scale_to_asset_amount`), i.e. `origin_decimals <= 26`. Reject
    // unclaimable routes up-front so a poisoned entry is never persisted.
    let miden_decimals = params.origin_decimals.min(faucet_ops::MIDEN_DECIMALS);
    // `miden_decimals <= origin_decimals` by construction — this never underflows;
    // it stays only as a defensive invariant guard.
    let scale = params
        .origin_decimals
        .checked_sub(miden_decimals)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "internal invariant violated: miden_decimals ({}) > origin_decimals ({})",
                miden_decimals,
                params.origin_decimals,
            )
        })?;

    if scale > faucet_ops::MAX_SCALING_FACTOR {
        anyhow::bail!(
            "scale ({scale} = origin_decimals {} - capped miden_decimals {}) exceeds the shared limit \
             of {} (route would be unclaimable). origin_decimals must be <= {} (= {} + {}).",
            params.origin_decimals,
            miden_decimals,
            faucet_ops::MAX_SCALING_FACTOR,
            faucet_ops::MAX_ORIGIN_DECIMALS,
            faucet_ops::MIDEN_DECIMALS,
            faucet_ops::MAX_SCALING_FACTOR,
        );
    }

    let accounts = &state.accounts.0;
    let service_id = accounts.service.0;
    let bridge_id = accounts.bridge.0;

    // Compute the raw ABI metadata preimage `abi.encode(name, symbol, decimals)` ONCE and
    // reuse it for both the on-Miden `MetadataHash` and the stored `FaucetEntry.metadata`
    // (Cantina #13). Using `abi_encode_params` (not `abi_encode`) matches Solidity's
    // `abi.encode(string, string, uint8)` exactly — a plain `abi_encode` of a dynamic tuple
    // would prepend an extra 32-byte offset word and diverge from the L1 bridge's
    // `getTokenMetadata` encoding. Deriving the hash via `from_abi_encoded(&metadata_bytes)`
    // guarantees `keccak256(stored_metadata) == faucet MetadataHash`, so a later bridge-out
    // emits metadata whose hash matches Miden's bridge state. Callers that skip the `name`
    // field get `name = symbol`.
    use alloy_core::sol_types::SolValue;
    let metadata_name = params.name.clone().unwrap_or_else(|| params.symbol.clone());
    let metadata_bytes = AdminTokenMetadata {
        name: metadata_name.clone(),
        symbol: params.symbol.clone(),
        decimals: params.origin_decimals,
    }
    .abi_encode_params();
    let metadata_hash = MetadataHash::from_abi_encoded(&metadata_bytes);

    // Create, deploy, register in bridge (using OnceLock pattern like publish_claim)
    let result = Arc::new(OnceLock::<AccountId>::new());
    let result_inner = result.clone();
    // Cantina #6 — set once the closure RECOVERED an existing on-chain faucet
    // (and already persisted its local row), so the post-closure create-path
    // registration is skipped.
    let recovered = Arc::new(OnceLock::<()>::new());
    let recovered_inner = recovered.clone();
    let store_for_closure = state.store.clone();
    let symbol_clone = params.symbol.clone();
    // `miden_decimals` was capped to min(origin_decimals, 8) above; reuse it (the
    // caller's params value is deliberately ignored).
    let origin_network = params.origin_network;
    // The admin-supplied metadata IS the authoritative preimage for this token;
    // prefer it over on-chain recovery when importing an existing faucet.
    let metadata_for_recovery = metadata_bytes.clone();

    state
        .miden_client
        .with(move |client| {
            Box::new(async move {
                // Cantina #6 — recover an EXISTING on-chain faucet for this origin
                // token before deploying a replacement generation. Mirrors the live
                // claim path: the local row is missing but the faucet may still be
                // registered on the bridge.
                if let Some(bridge_account) = client.get_account(bridge_id).await.ok().flatten()
                    && let Some((existing_id, conversion)) =
                        crate::metadata_recovery::find_registered_faucet_for_origin(
                            bridge_account.storage(),
                            &origin_address,
                            origin_network,
                        )
                {
                    tracing::warn!(
                        faucet_id = %existing_id,
                        origin_network,
                        "admin_registerFaucet: origin token already has a faucet registered on \
                         the bridge but no local row — importing the existing identity instead \
                         of deploying a replacement (Cantina #6)"
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
                            entry.metadata = metadata_for_recovery;
                            store_for_closure.register_faucet(entry).await?;
                            ::metrics::counter!("faucet_recovered_existing_total").increment(1);
                            let _ = result_inner.set(existing_id);
                            let _ = recovered_inner.set(());
                            return Ok(());
                        }
                        Err(e) => {
                            ::metrics::counter!("faucet_recover_existing_failed_total")
                                .increment(1);
                            tracing::warn!(
                                faucet_id = %existing_id,
                                error = ?e,
                                "admin_registerFaucet: failed to import existing faucet identity; \
                                 falling back to deploy (WARNING: may create a second generation)"
                            );
                        }
                    }
                }

                let account = faucet_ops::create_and_register_faucet(
                    client,
                    &symbol_clone,
                    miden_decimals,
                    &origin_address,
                    origin_network,
                    scale,
                    service_id,
                    bridge_id,
                    metadata_hash,
                    false, // admin_registerFaucet: bridge-owned mint/burn (not Miden-native)
                )
                .await?;
                let _ = result_inner.set(account.id());
                Ok(())
            })
        })
        .await?;

    let faucet_id = *result.get().ok_or_else(|| {
        anyhow::anyhow!("admin_registerFaucet: closure completed but result not set")
    })?;

    // Save to store — UNLESS the closure already recovered + persisted an existing
    // faucet identity (Cantina #6), in which case the row is already written. This
    // path is only reached when no route existed for the origin (existing routes
    // returned early above), so a plain insert is correct — there is deliberately no
    // live "replace" path. `miden_decimals` here is the capped local (finding #17),
    // NOT the ignored `params.miden_decimals`.
    if recovered.get().is_none() {
        state
            .store
            .register_faucet(FaucetEntry {
                faucet_id,
                origin_address,
                origin_network: params.origin_network,
                symbol: params.symbol,
                origin_decimals: params.origin_decimals,
                miden_decimals,
                scale,
                metadata: metadata_bytes,
            })
            .await?;
    }

    let id_hex = faucet_id.to_hex();
    tracing::info!(
        faucet_id = %id_hex,
        "admin_registerFaucet: faucet created and registered"
    );
    Ok(id_hex)
}

#[derive(Debug, Deserialize)]
pub struct RegisterNativeFaucetParams {
    /// The EXISTING, externally-deployed Miden faucet account id (hex) to allowlist as
    /// native. The proxy does NOT create it — an external party (e.g. the bridge-out
    /// app's `--create-native-faucet`) deploys + mints it first.
    pub faucet_id: String,
    /// The 20-byte origin token address the bridge records for this native faucet (its
    /// canonical L1/agglayer-side representation).
    pub origin_token_address: String,
    pub symbol: String,
    pub decimals: u8,
    #[serde(default)]
    pub name: Option<String>,
}

/// Authoritative token metadata read from a deployed Miden faucet account
/// (`token_name` / `symbol` / `decimals`). This is the ONLY source from which the
/// registered metadata-hash preimage can be reconstructed after database loss, so
/// it — not caller-supplied params — is what `admin_registerNativeFaucet` persists.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AuthoritativeFaucetMetadata {
    pub name: String,
    pub symbol: String,
    pub decimals: u8,
}

/// Resolved native-faucet metadata to persist/emit — always the deployed faucet
/// account's authoritative values.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ResolvedNativeMetadata {
    pub name: String,
    pub symbol: String,
    pub decimals: u8,
}

/// Issue #149 — validate caller-supplied native-faucet metadata against the
/// deployed Miden faucet account's authoritative values and RESOLVE the metadata
/// to persist/emit from that authoritative state.
///
/// The proxy persists + emits a metadata hash whose preimage is
/// `abi.encode(name, symbol, decimals)`; after database loss, recovery
/// reconstructs that preimage ONLY from the deployed faucet account
/// (`metadata_recovery::miden_faucet_candidate`). If the registered preimage came
/// from caller-supplied params that differ from the deployed faucet, the hash's
/// preimage is unreconstructable and its poison leaf halts restore fail-closed.
///
/// Rules (each mismatch rejected INDEPENDENTLY, before any state change):
/// - `symbol` must equal the faucet's actual symbol.
/// - `decimals` must equal the faucet's actual decimals.
/// - `name`, if supplied, must equal the faucet's actual token name EXACTLY — a
///   custom `name != symbol` is valid and preserved, never normalized to symbol.
/// - `name` omitted → resolve to the faucet's actual token name (so an omitted
///   name always succeeds with the authoritative name).
///
/// Pure (no I/O) so the whole decision is unit-testable; the caller supplies the
/// authoritative triple read from the faucet account.
pub(crate) fn resolve_native_faucet_metadata(
    requested_name: Option<&str>,
    requested_symbol: &str,
    requested_decimals: u8,
    authoritative: &AuthoritativeFaucetMetadata,
) -> anyhow::Result<ResolvedNativeMetadata> {
    if requested_symbol != authoritative.symbol {
        anyhow::bail!(
            "admin_registerNativeFaucet: symbol mismatch — requested {requested_symbol:?}, \
             deployed faucet account has {:?}; the deployed faucet is authoritative. No \
             registry row was written; re-register with the faucet's actual symbol.",
            authoritative.symbol
        );
    }
    if requested_decimals != authoritative.decimals {
        anyhow::bail!(
            "admin_registerNativeFaucet: decimals mismatch — requested {requested_decimals}, \
             deployed faucet account has {}; the deployed faucet is authoritative. No \
             registry row was written; re-register with the faucet's actual decimals.",
            authoritative.decimals
        );
    }
    if let Some(name) = requested_name
        && name != authoritative.name
    {
        anyhow::bail!(
            "admin_registerNativeFaucet: name mismatch — requested {name:?}, deployed faucet \
             account has {:?}; a custom name must match the on-chain token name exactly and \
             is never normalized. No registry row was written; re-register with the faucet's \
             actual name or omit `name` to adopt it.",
            authoritative.name
        );
    }
    // Omitted name resolves to the authoritative name (which may legitimately differ
    // from the symbol — a valid custom name is preserved, never collapsed to symbol).
    Ok(ResolvedNativeMetadata {
        name: authoritative.name.clone(),
        symbol: authoritative.symbol.clone(),
        decimals: authoritative.decimals,
    })
}

/// `admin_registerNativeFaucet` — allowlist an EXTERNALLY-deployed Miden-ORIGINATED
/// (native lock/unlock) faucet on the bridge. Faucet bridging is a PERMISSIONED
/// ALLOWLIST: only the bridge admin can register, and the admin IS this proxy's
/// `service` account — so the external party deploys the faucet, the PROXY registers it.
///
/// This is the REQUEST side (mirrors `ger.rs::insert_ger`): it sends the admin
/// `ConfigAggBridgeNote` with `is_native = true` and persists the proxy-store row. The
/// faucet-registry DISCOVERY module is the decoupled read side — it reconciles entries
/// registered by anyone (including a different admin, adopted with a `warn!`) and after
/// a restart, independent of and order-agnostic to this request.
///
/// Native means the token ORIGINATES on this Miden network, so the origin network is
/// this proxy's CONFIGURED `network_id` (never hardcoded 1): `is_native` is derivable as
/// `origin_network == service.network_id`. There is no L1<->Miden decimal scaling for a
/// native token (`scale = 0`, `origin_decimals == miden_decimals`).
pub async fn admin_register_native_faucet(
    state: ServiceState,
    params: RegisterNativeFaucetParams,
) -> anyhow::Result<String> {
    let faucet_id = AccountId::from_hex(&params.faucet_id)
        .map_err(|e| anyhow::anyhow!("bad faucet_id {}: {e:?}", params.faucet_id))?;
    let origin_address = parse_eth_address(&params.origin_token_address)?;
    let origin_network = state.network_id;
    let scale = 0u8;

    // Shared single-flight with the public path (REGISTER_NATIVE_LOCK). The admin
    // path is trusted, so it WAITS on the lock rather than shedding — but it must
    // hold the SAME lock so a concurrent permissionless registration cannot
    // interleave between this path's checks and its config-note emission. Held to
    // end of function.
    let _guard = REGISTER_NATIVE_LOCK.lock().await;

    // Idempotent: an existing native route for this (origin_address, origin_network) is
    // returned as-is (register-if-absent), matching admin_registerFaucet's contract.
    if let Some(existing) = state
        .store
        .get_faucet_by_origin(&origin_address, origin_network)
        .await?
    {
        // Return the EXISTING route's faucet_id, not the caller-supplied one: the origin
        // is already bound to `existing.faucet_id`, and echoing the caller's (possibly
        // different) id would falsely imply THAT faucet is the registered route.
        // PR#164 re-review: confirm the cached row against the AUTHORITATIVE
        // bridge binding before reporting an existing route. A stale row (earlier
        // false-success, partial restore) would otherwise be echoed back forever
        // as a live route the bridge never had.
        match preflight_bridge_binding(&state, existing.faucet_id, origin_address, origin_network)
            .await?
        {
            BridgeBinding::AlreadyBound => {
                tracing::info!(
                    origin_network,
                    existing_faucet_id = %existing.faucet_id.to_hex(),
                    requested_faucet_id = %faucet_id.to_hex(),
                    "admin_registerNativeFaucet: a route already exists for this origin (confirmed \
                     on-chain) — returning the existing route's faucet"
                );
                return Ok(existing.faucet_id.to_hex());
            }
            BridgeBinding::Unbound | BridgeBinding::FaucetBoundToDifferentOrigin(_) => {
                // Review 0814: a stale row for a DIFFERENT faucet must be
                // rejected HERE, before any bridge mutation. Falling through
                // would bind the requested faucet on-chain, then the
                // faucet_id-guarded upsert preserves the stale row and the
                // read-back errors only AFTER the bridge changed — a
                // registry/bridge split manufactured by the API itself.
                // Same-faucet stale rows (false-success remnants) still
                // re-drive: the upsert guard passes for the same id.
                if existing.faucet_id != faucet_id {
                    anyhow::bail!(
                        "admin_registerNativeFaucet: origin 0x{} (network {origin_network}) is \
                         held by a STALE registry row for faucet {} (the bridge has no such \
                         binding), while this call requests faucet {}. Registering would split \
                         the registry from the bridge. Remove/repair the stale row, then retry. \
                         No note emitted, no state changed.",
                        hex::encode(origin_address),
                        existing.faucet_id.to_hex(),
                        faucet_id.to_hex(),
                    );
                }
                ::metrics::counter!("faucet_registry_stale_row_redriven_total").increment(1);
                tracing::warn!(
                    origin_network,
                    existing_faucet_id = %existing.faucet_id.to_hex(),
                    "admin_registerNativeFaucet: local row claims this origin but the bridge has \
                     no matching binding (stale row) — re-driving registration instead of \
                     reporting a route that does not exist"
                );
            }
            BridgeBinding::OriginBoundToOther(other) => {
                // Review 0814: returning Ok(other) here previously reported
                // success while the LOCAL row kept pointing at the stale
                // faucet — a registry/bridge split presented as a route. There
                // is no safe automatic repair (the authoritative faucet's row
                // needs its own metadata read + guarded persist, which the
                // stale row blocks) — fail loudly instead.
                anyhow::bail!(
                    "admin_registerNativeFaucet: origin 0x{} (network {origin_network}) is bound \
                     ON-CHAIN to faucet {} while the local registry row records faucet {} — a \
                     stale/split registry state this call cannot safely repair. Remove/repair \
                     the local row (the bridge binding is the authority), then retry. No note \
                     emitted, no state changed.",
                    hex::encode(origin_address),
                    other.to_hex(),
                    existing.faucet_id.to_hex(),
                );
            }
        }
    }

    // #149 — read the deployed faucet account's AUTHORITATIVE metadata BEFORE any
    // state change. The persisted + emitted metadata-hash preimage
    // (`abi.encode(name, symbol, decimals)`) must be reconstructable from chain
    // state after database loss: recovery derives its only candidate from the
    // faucet account (`metadata_recovery::miden_faucet_candidate`). So the
    // registered preimage MUST come from the faucet account, not caller-supplied
    // params — otherwise the hash is unrecoverable and its poison leaf halts
    // restore. `with()` returning before this populates the slot (or erroring) is
    // fail-closed: we bail before touching the bridge or the registry.
    let authoritative = read_authoritative_faucet_metadata(&state, faucet_id).await?;

    // #149 — validate the REQUESTED metadata against the AUTHORITATIVE faucet
    // account BEFORE the idempotency preflight. A caller supplying wrong metadata
    // for an already-bound faucet must still be rejected (the mismatch is a hard
    // error), so this MUST precede the "already bound → no-op" short-circuit;
    // otherwise a wrong-metadata request would return success just because the
    // faucet happens to be on the bridge. `register_native_validated` re-runs this
    // pure resolve, which is cheap and keeps it the single source of truth.
    resolve_native_faucet_metadata(
        params.name.as_deref(),
        &params.symbol,
        params.decimals,
        &authoritative,
    )?;

    // Review 0814 — the SYMMETRIC pre-mutation conflict, mirrored from the
    // public path: the requested faucet may already exist locally under a
    // DIFFERENT origin. Without this check the bridge preflight (keyed on the
    // ORIGIN) can be Unbound, the irreversible ConfigAggBridgeNote is emitted,
    // and only then the faucet_id primary-key conflict fails persistence —
    // after the bridge already mutated.
    if let Some(by_id) = state.store.get_faucet_by_id(faucet_id).await?
        && (by_id.origin_address != origin_address || by_id.origin_network != origin_network)
    {
        anyhow::bail!(
            "admin_registerNativeFaucet: faucet {} is already registered locally with a \
             different origin identity (0x{} network {}), which this call would not update. \
             Registering it for origin 0x{} would split the registry from the bridge. No note \
             emitted, no state changed.",
            faucet_id.to_hex(),
            hex::encode(by_id.origin_address),
            by_id.origin_network,
            hex::encode(origin_address),
        );
    }

    // Authoritative on-chain preflight UNDER THE LOCK, after validation and before
    // any note is emitted (same guarantee as the public path). The local registry
    // can be empty after DB loss while the bridge still binds this faucet or its
    // origin; refuse to emit a rebinding note and return the existing binding
    // idempotently.
    match preflight_bridge_binding(&state, faucet_id, origin_address, origin_network).await? {
        BridgeBinding::OriginBoundToOther(other) => {
            // Review 0814d: NEVER auto-adopt. An on-chain faucet the registry
            // does not know is exactly the FaucetRegistryReconciler SECURITY
            // TRIPWIRE (possible external use of the admin key) — only a
            // verified `--restore` is sanctioned to import it, and this
            // authenticated call asked for a DIFFERENT faucet anyway.
            // Fail before any mutation, naming the authoritative binding so
            // the operator can explicitly retry for it or run restore.
            return Err(anyhow::anyhow!(
                "{}",
                unknown_onchain_binding_error(other, faucet_id, origin_address, origin_network)
            ));
        }
        BridgeBinding::FaucetBoundToDifferentOrigin(bound) => {
            anyhow::bail!(
                "admin_registerNativeFaucet: faucet {} is already bound on the bridge to origin \
                 0x{}, which differs from the requested origin 0x{}. Rebinding a native faucet to \
                 a new origin is not supported (it would double-bind the faucet). No note emitted, \
                 no state changed.",
                faucet_id.to_hex(),
                hex::encode(bound),
                hex::encode(origin_address),
            );
        }
        BridgeBinding::AlreadyBound => {
            // Review 0814: reconcile the local row NOW instead of deferring to
            // "restore/rebuild". The bridge binding is authoritative and
            // already exists; leaving the registry without (or with a stale
            // copy of) this row keeps serving split state until an unrelated
            // recovery happens to run. Same validation + guarded persist +
            // read-back as a fresh registration — only the bridge note is
            // skipped.
            tracing::info!(
                faucet_id = %faucet_id.to_hex(),
                "admin_registerNativeFaucet: faucet already registered on the bridge at this \
                 origin — no note emitted; reconciling the local registry row"
            );
            let resolved = resolve_native_faucet_metadata(
                params.name.as_deref(),
                &params.symbol,
                params.decimals,
                &authoritative,
            )?;
            persist_and_verify_native_row(
                &state,
                faucet_id,
                origin_address,
                origin_network,
                scale,
                &resolved,
            )
            .await?;
            return Ok(faucet_id.to_hex());
        }
        BridgeBinding::Unbound => { /* safe to register below */ }
    }

    // #149 — validate + register from AUTHORITATIVE state. Split out so the
    // "successful read → mismatch → NO bridge ConfigAggBridgeNote" path is
    // executable in a unit test (PR #150): the test calls `register_native_validated`
    // with a materialized authoritative triple and asserts the bridge-register
    // `with()` is never reached on a mismatch (test_call_count()==0 vs 1 on a match).
    register_native_validated(
        &state,
        faucet_id,
        origin_address,
        origin_network,
        scale,
        &params,
        &authoritative,
    )
    .await
}

/// #149 — the validate-then-register tail of `admin_register_native_faucet`, split
/// out so it is unit-testable with a caller-supplied authoritative triple (the
/// production caller reads that triple from the deployed faucet account first).
///
/// Validates the requested metadata against `authoritative` and, only if it passes,
/// emits the bridge `ConfigAggBridgeNote` (via `register_faucet_in_bridge`) and
/// persists the registry row — both built from the RESOLVED (authoritative) values.
/// A mismatch returns `Err` BEFORE the `register_faucet_in_bridge` `with()` call, so
/// no bridge note and no registry row are produced (no partial state).
async fn register_native_validated(
    state: &ServiceState,
    faucet_id: AccountId,
    origin_address: [u8; 20],
    origin_network: u32,
    scale: u8,
    params: &RegisterNativeFaucetParams,
    authoritative: &AuthoritativeFaucetMetadata,
) -> anyhow::Result<String> {
    let resolved = resolve_native_faucet_metadata(
        params.name.as_deref(),
        &params.symbol,
        params.decimals,
        authoritative,
    )?;

    // abi.encode(name, symbol, decimals) — same preimage the bridge/L2 wrapped-token
    // metadata hashes to (Cantina #13); reused for the on-Miden MetadataHash + the
    // stored FaucetEntry.metadata so a later bridge-out emits matching metadata. Built
    // from the AUTHORITATIVE (resolved) values so the preimage is always recoverable.
    use alloy_core::sol_types::SolValue;
    let metadata_bytes = AdminTokenMetadata {
        name: resolved.name.clone(),
        symbol: resolved.symbol.clone(),
        decimals: resolved.decimals,
    }
    .abi_encode_params();
    let metadata_hash = MetadataHash::from_abi_encoded(&metadata_bytes);

    let accounts = &state.accounts.0;
    let service_id = accounts.service.0;
    let bridge_id = accounts.bridge.0;
    let symbol_clone = resolved.symbol.clone();

    // REQUEST: admin ConfigAggBridgeNote registering the EXISTING faucet as native
    // (is_native = true). Register only — the faucet was deployed externally.
    state
        .miden_client
        .with(move |client| {
            Box::new(async move {
                let origin_addr = EthAddress::new(origin_address);
                faucet_ops::register_faucet_in_bridge(
                    client,
                    service_id,
                    bridge_id,
                    faucet_id,
                    &origin_addr,
                    origin_network,
                    scale,
                    metadata_hash,
                    &symbol_clone,
                    true, // is_native — Miden-ORIGINATED lock/unlock faucet
                )
                .await
            })
        })
        .await?;

    persist_and_verify_native_row(
        state,
        faucet_id,
        origin_address,
        origin_network,
        scale,
        &resolved,
    )
    .await?;

    let id_hex = faucet_id.to_hex();
    tracing::info!(
        faucet_id = %id_hex,
        origin_network,
        "admin_registerNativeFaucet: native faucet allowlisted on the bridge (persisted binding \
         verified)"
    );
    Ok(id_hex)
}

/// Review 0814d — the fail-closed message for an origin bound ON-CHAIN to a
/// faucet the registry does not know. Pure so the endpoint branch is pinned by
/// a unit test: auto-adopting here would launder the FaucetRegistryReconciler
/// security tripwire (unknown bridge faucet = possible external admin-key use;
/// only verified `--restore` may import it).
fn unknown_onchain_binding_error(
    onchain_faucet: AccountId,
    requested_faucet: AccountId,
    origin_address: [u8; 20],
    origin_network: u32,
) -> String {
    format!(
        "admin_registerNativeFaucet: origin 0x{} (network {origin_network}) is already bound \
         ON-CHAIN to faucet {}, which the local registry does not record — and this call \
         requested faucet {}. An unknown on-chain binding is the registry security tripwire \
         (possible external admin-key use); refusing to adopt it implicitly. Either retry \
         explicitly for faucet {} after operator review, or run the verified `--restore` \
         recovery to import on-chain state. No note emitted, no state changed.",
        hex::encode(origin_address),
        onchain_faucet.to_hex(),
        requested_faucet.to_hex(),
        onchain_faucet.to_hex(),
    )
}

/// Read a deployed faucet account's AUTHORITATIVE metadata (import on demand +
/// classify as native). Shared by registration (reads the REQUESTED faucet)
/// and the OriginBoundToOther reconcile (reads the ON-CHAIN faucet, review
/// 0814c). Fail-closed: any read/classify failure aborts before any state
/// change.
async fn read_authoritative_faucet_metadata(
    state: &ServiceState,
    faucet_id: AccountId,
) -> anyhow::Result<AuthoritativeFaucetMetadata> {
    let slot = Arc::new(std::sync::Mutex::new(None::<AuthoritativeFaucetMetadata>));
    let slot_write = slot.clone();
    state
        .miden_client
        .with(move |client| {
            Box::new(async move {
                // Native faucets are externally deployed and NOT in
                // bridge_accounts.toml, so import them on demand before reading.
                if client.get_account(faucet_id).await.ok().flatten().is_none()
                    && let Err(e) = client.import_account_by_id(faucet_id).await
                {
                    anyhow::bail!(
                        "admin_registerNativeFaucet: cannot import faucet account \
                         {faucet_id} from node: {e}"
                    );
                }
                let faucet_account = client
                    .get_account(faucet_id)
                    .await
                    .map_err(|e| anyhow::anyhow!("get_account({faucet_id}): {e}"))?
                    .ok_or_else(|| {
                        anyhow::anyhow!(
                            "admin_registerNativeFaucet: faucet account {faucet_id} not \
                             found after import"
                        )
                    })?;
                // Both supported kinds expose the standard FungibleFaucet metadata, but
                // admin_registerNativeFaucet is for operator-owned NATIVE faucets only.
                // Guard against an operator accidentally reclassifying an already-deployed
                // AggLayer-owned (bridge mint/burn) faucet as native (PR #150 review).
                let (kind, faucet) = faucet_ops::classify_faucet_account(&faucet_account)?;
                if kind != faucet_ops::FaucetKind::NativeFungible {
                    anyhow::bail!(
                        "admin_registerNativeFaucet: faucet account {faucet_id} is an \
                         AggLayer-owned (bridge mint/burn) faucet, not an operator-owned \
                         native FungibleFaucet — refusing to register it as native; no \
                         registry row written"
                    );
                }
                *slot_write.lock().unwrap() = Some(AuthoritativeFaucetMetadata {
                    name: faucet.token_name().as_str().to_string(),
                    symbol: faucet.symbol().to_string(),
                    decimals: faucet.decimals(),
                });
                Ok(())
            })
        })
        .await?;
    let out = slot.lock().unwrap().take().ok_or_else(|| {
        anyhow::anyhow!(
            "admin_registerNativeFaucet: could not read faucet account {faucet_id} \
             metadata (no authoritative token name/symbol/decimals); refusing to register \
             — no registry row written"
        )
    })?;
    Ok(out)
}

/// Guarded registry persist + read-back verification for a native faucet row —
/// shared by fresh registration (after the bridge `ConfigAggBridgeNote`) and
/// the AlreadyBound reconcile (review 0814), so BOTH paths report success only
/// when the persisted binding is exactly the one requested. `register_faucet`
/// upserts under a `WHERE faucet_registry.faucet_id = EXCLUDED.faucet_id`
/// guard, so a stale row for a DIFFERENT faucet holding this origin makes the
/// statement affect ZERO rows while returning Ok — the read-back turns that
/// silent split-brain into a hard error.
async fn persist_and_verify_native_row(
    state: &ServiceState,
    faucet_id: AccountId,
    origin_address: [u8; 20],
    origin_network: u32,
    scale: u8,
    resolved: &ResolvedNativeMetadata,
) -> anyhow::Result<()> {
    use alloy_core::sol_types::SolValue;
    let metadata_bytes = AdminTokenMetadata {
        name: resolved.name.clone(),
        symbol: resolved.symbol.clone(),
        decimals: resolved.decimals,
    }
    .abi_encode_params();
    // origin_network == the configured net id => is_native is derivable; all
    // fields come from the authoritative (resolved) metadata, never
    // caller-supplied.
    state
        .store
        .register_faucet(FaucetEntry {
            faucet_id,
            origin_address,
            origin_network,
            symbol: resolved.symbol.clone(),
            origin_decimals: resolved.decimals, // native: no L1<->Miden scaling
            miden_decimals: resolved.decimals,
            scale,
            metadata: metadata_bytes,
        })
        .await?;

    let persisted = state
        .store
        .get_faucet_by_origin(&origin_address, origin_network)
        .await?;
    match persisted {
        Some(row) if row.faucet_id == faucet_id => Ok(()),
        Some(row) => {
            anyhow::bail!(
                "registerNativeFaucet: the bridge is bound to faucet {} but the registry still \
                 records faucet {} for origin 0x{} (network {}) — the guarded upsert affected no \
                 rows because a stale row holds this origin. Refusing to report success on a \
                 split registry/bridge state; resolve the stale row and retry.",
                faucet_id.to_hex(),
                row.faucet_id.to_hex(),
                hex::encode(origin_address),
                origin_network,
            );
        }
        None => {
            anyhow::bail!(
                "registerNativeFaucet: the bridge is bound to faucet {} but no registry row for \
                 origin 0x{} (network {}) is readable afterwards — refusing to report success on \
                 an unpersisted registration.",
                faucet_id.to_hex(),
                hex::encode(origin_address),
                origin_network,
            );
        }
    }
}

fn parse_eth_address(s: &str) -> anyhow::Result<[u8; 20]> {
    let s = s.strip_prefix("0x").unwrap_or(s);
    let bytes = hex::decode(s)?;
    if bytes.len() != 20 {
        anyhow::bail!(
            "invalid ETH address: expected 20 bytes, got {}",
            bytes.len()
        );
    }
    let mut addr = [0u8; 20];
    addr.copy_from_slice(&bytes);
    Ok(addr)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::FaucetEntry;
    use crate::test_helpers::create_test_service;
    use miden_protocol::account::AccountId;

    // ── #149: pure native-faucet metadata validation/resolution ──────────────
    // The deployed faucet account is authoritative; caller-supplied metadata is
    // validated against it and the persisted preimage is resolved FROM it.

    fn authoritative(name: &str, symbol: &str, decimals: u8) -> AuthoritativeFaucetMetadata {
        AuthoritativeFaucetMetadata {
            name: name.into(),
            symbol: symbol.into(),
            decimals,
        }
    }

    /// Exact match (name == symbol) resolves to the authoritative triple.
    #[test]
    fn native_metadata_exact_match_resolves() {
        let auth = authoritative("MDN", "MDN", 8);
        let resolved =
            resolve_native_faucet_metadata(Some("MDN"), "MDN", 8, &auth).expect("match succeeds");
        assert_eq!(resolved.name, "MDN");
        assert_eq!(resolved.symbol, "MDN");
        assert_eq!(resolved.decimals, 8);
    }

    /// A wrong symbol is rejected independently with a specific error.
    #[test]
    fn native_metadata_symbol_mismatch_rejected() {
        let auth = authoritative("MDN", "MDN", 8);
        let err = resolve_native_faucet_metadata(Some("MDN"), "WRONG", 8, &auth).unwrap_err();
        assert!(
            err.to_string().contains("symbol mismatch"),
            "unexpected error: {err}"
        );
    }

    /// Wrong decimals are rejected independently.
    #[test]
    fn native_metadata_decimals_mismatch_rejected() {
        let auth = authoritative("MDN", "MDN", 8);
        let err = resolve_native_faucet_metadata(Some("MDN"), "MDN", 6, &auth).unwrap_err();
        assert!(
            err.to_string().contains("decimals mismatch"),
            "unexpected error: {err}"
        );
    }

    /// A supplied name that differs from the on-chain token name is rejected —
    /// a custom name must match exactly, never normalized.
    #[test]
    fn native_metadata_name_mismatch_rejected() {
        let auth = authoritative("Wrapped Midnight", "MDN", 8);
        let err =
            resolve_native_faucet_metadata(Some("Something Else"), "MDN", 8, &auth).unwrap_err();
        assert!(
            err.to_string().contains("name mismatch"),
            "unexpected error: {err}"
        );
    }

    /// An omitted name adopts the authoritative token name, and a custom
    /// `name != symbol` is preserved exactly (never collapsed to the symbol) — so
    /// the resolved preimage keccak-matches the deployed faucet's stored hash and
    /// survives database-loss restore.
    #[test]
    fn native_metadata_omitted_name_adopts_custom_name() {
        let auth = authoritative("Wrapped Midnight", "MDN", 8);
        let resolved = resolve_native_faucet_metadata(None, "MDN", 8, &auth)
            .expect("omitted name succeeds by adopting the authoritative name");
        assert_eq!(resolved.name, "Wrapped Midnight");
        assert_ne!(
            resolved.name, resolved.symbol,
            "a custom name must be preserved, not normalized to the symbol"
        );
        assert_eq!(resolved.symbol, "MDN");
        // The persisted preimage is abi.encode(resolved.name, symbol, decimals);
        // recovery's miden_faucet_candidate rebuilds the SAME triple from the
        // faucet account's token_name(), so the keccak hash matches on restore.
        use alloy_core::sol_types::SolValue;
        let bytes = AdminTokenMetadata {
            name: resolved.name.clone(),
            symbol: resolved.symbol.clone(),
            decimals: resolved.decimals,
        }
        .abi_encode_params();
        let from_authoritative = AdminTokenMetadata {
            name: auth.name.clone(),
            symbol: auth.symbol.clone(),
            decimals: auth.decimals,
        }
        .abi_encode_params();
        assert_eq!(
            bytes, from_authoritative,
            "resolved preimage must equal the authoritative preimage"
        );
    }

    /// A matching explicit custom name succeeds and is preserved.
    #[test]
    fn native_metadata_matching_custom_name_succeeds() {
        let auth = authoritative("Wrapped Midnight", "MDN", 8);
        let resolved = resolve_native_faucet_metadata(Some("Wrapped Midnight"), "MDN", 8, &auth)
            .expect("matching custom name succeeds");
        assert_eq!(resolved.name, "Wrapped Midnight");
    }

    /// Review 0814c — the guarded persist + read-back both reconcile arms
    /// (AlreadyBound, OriginBoundToOther) rely on: a clean origin persists and
    /// verifies; a STALE row for a DIFFERENT faucet holding the origin makes
    /// the guarded upsert a no-op and MUST surface as a hard error, never
    /// success over split state.
    #[tokio::test]
    async fn persist_and_verify_native_row_ok_then_stale_conflict() {
        let service = create_test_service();
        let faucet_a = AccountId::from_hex("0xac0000000000dd110000ee000000fc").unwrap();
        let faucet_b = AccountId::from_hex("0xac0000000000dd110000ee000000ad").unwrap();
        let origin = [0xDEu8; 20];
        let resolved = ResolvedNativeMetadata {
            name: "MDN".into(),
            symbol: "MDN".into(),
            decimals: 8,
        };

        // Clean insert: persists and read-back verifies.
        persist_and_verify_native_row(&service, faucet_a, origin, 1, 0, &resolved)
            .await
            .expect("clean persist verifies");
        let row = service
            .store
            .get_faucet_by_origin(&origin, 1)
            .await
            .unwrap()
            .expect("row persisted");
        assert_eq!(row.faucet_id, faucet_a);

        // Stale conflict: faucet B cannot claim the origin held by A — the
        // guarded upsert affects no rows and the read-back must hard-error.
        let err = persist_and_verify_native_row(&service, faucet_b, origin, 1, 0, &resolved)
            .await
            .expect_err("stale row must surface, not report success");
        assert!(
            err.to_string().contains("stale row holds this origin"),
            "unexpected error: {err}"
        );
        // The registry still records A — no silent overwrite happened.
        let row = service
            .store
            .get_faucet_by_origin(&origin, 1)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            row.faucet_id, faucet_a,
            "stale conflict must not clobber the row"
        );
    }

    /// Review 0814d — the no-local-row OriginBoundToOther branch must FAIL
    /// (never adopt): the message names the authoritative on-chain faucet, the
    /// requested faucet, and the sanctioned recovery paths.
    #[test]
    fn unknown_onchain_binding_is_rejected_not_adopted() {
        let onchain = AccountId::from_hex("0xac0000000000dd110000ee000000fc").unwrap();
        let requested = AccountId::from_hex("0xac0000000000dd110000ee000000ad").unwrap();
        let msg = unknown_onchain_binding_error(onchain, requested, [0xDE; 20], 1);
        assert!(
            msg.contains(&onchain.to_hex()),
            "names the on-chain binding"
        );
        assert!(
            msg.contains(&requested.to_hex()),
            "names the requested faucet"
        );
        assert!(
            msg.contains("security tripwire"),
            "cites the tripwire policy"
        );
        assert!(
            msg.contains("--restore"),
            "points at the sanctioned recovery"
        );
        assert!(msg.contains("No note emitted"), "asserts no mutation");
    }

    fn native_params(name: Option<&str>) -> RegisterNativeFaucetParams {
        RegisterNativeFaucetParams {
            // A valid protocol-0.15 faucet account id.
            faucet_id: "0xac0000000000dd110000ee000000fc".into(),
            origin_token_address: "0x000000000000000000000000000000000000dEaD".into(),
            symbol: "MDN".into(),
            decimals: 8,
            name: name.map(str::to_string),
        }
    }

    /// #149 fail-closed: when the authoritative faucet metadata cannot be read
    /// (the test client never yields an account), registration bails BEFORE the
    /// bridge ConfigAggBridgeNote and BEFORE any registry write — no partial
    /// state. `test_call_count() == 1` proves only the read was attempted and the
    /// bridge-register `with()` was never reached.
    #[tokio::test]
    async fn native_registration_fails_closed_when_metadata_unreadable() {
        let service = create_test_service();
        let origin = parse_eth_address("0x000000000000000000000000000000000000dEaD").unwrap();

        let err = admin_register_native_faucet(service.clone(), native_params(Some("MDN")))
            .await
            .expect_err("must fail closed when the faucet account metadata is unreadable");
        assert!(
            err.to_string().contains("could not read faucet account"),
            "unexpected error: {err}"
        );
        // No registry row written.
        assert!(
            service
                .store
                .get_faucet_by_origin(&origin, service.network_id)
                .await
                .unwrap()
                .is_none(),
            "a failed registration must leave no registry row"
        );
        // Only the authoritative-read `with()` was attempted; the bridge-register
        // `with()` (which would emit the ConfigAggBridgeNote) was never reached.
        assert_eq!(
            service.miden_client.test_call_count(),
            1,
            "bridge ConfigAggBridgeNote must not be emitted on a failed registration"
        );
    }

    /// #149 (PR #150) — EXECUTABLE proof that a SUCCESSFUL authoritative read
    /// FOLLOWED BY a metadata mismatch never reaches the bridge-register `with()`
    /// (never emits a `ConfigAggBridgeNote`). `register_native_validated` takes a
    /// materialized authoritative triple, so — unlike the endpoint test, whose mock
    /// never runs the read closure — this drives the mismatch path directly.
    /// `test_call_count() == 0` after a mismatch vs `== 1` after a match proves the
    /// register `with()` is conditional on validation passing (not vacuously zero).
    #[tokio::test]
    async fn native_registration_mismatch_skips_bridge_registration() {
        let service = create_test_service();
        // A SUCCESSFUL authoritative read: the deployed faucet has symbol=MDN, decimals=8.
        let auth = authoritative("MDN", "MDN", 8);
        let faucet_id = AccountId::from_hex(&native_params(None).faucet_id).unwrap();
        let origin = parse_eth_address(&native_params(None).origin_token_address).unwrap();
        let net = service.network_id;

        // symbol mismatch → Err, and the bridge-register with() is NEVER called.
        let mut p = native_params(None);
        p.symbol = "WRONG".into();
        let err = register_native_validated(&service, faucet_id, origin, net, 0, &p, &auth)
            .await
            .expect_err("symbol mismatch must be rejected");
        assert!(
            err.to_string().contains("symbol mismatch"),
            "unexpected: {err}"
        );
        assert_eq!(
            service.miden_client.test_call_count(),
            0,
            "no bridge ConfigAggBridgeNote may be emitted on a symbol mismatch"
        );
        assert!(
            service
                .store
                .get_faucet_by_origin(&origin, net)
                .await
                .unwrap()
                .is_none(),
            "a rejected registration must leave no registry row"
        );

        // decimals mismatch → Err, still no bridge call.
        let mut pd = native_params(None);
        pd.decimals = 6;
        let err = register_native_validated(&service, faucet_id, origin, net, 0, &pd, &auth)
            .await
            .expect_err("decimals mismatch must be rejected");
        assert!(
            err.to_string().contains("decimals mismatch"),
            "unexpected: {err}"
        );
        assert_eq!(service.miden_client.test_call_count(), 0);

        // name mismatch (explicit custom name != authoritative) → Err, still no bridge call.
        let err = register_native_validated(
            &service,
            faucet_id,
            origin,
            net,
            0,
            &native_params(Some("Not The Real Name")),
            &auth,
        )
        .await
        .expect_err("name mismatch must be rejected");
        assert!(
            err.to_string().contains("name mismatch"),
            "unexpected: {err}"
        );
        assert_eq!(
            service.miden_client.test_call_count(),
            0,
            "no bridge note emitted across the symbol/decimals/name mismatches"
        );

        // POSITIVE CONTROL: a MATCHING authoritative DOES reach the bridge-register
        // with() (count 0 -> 1), proving the count==0 assertions above are meaningful.
        register_native_validated(
            &service,
            faucet_id,
            origin,
            net,
            0,
            &native_params(None),
            &auth,
        )
        .await
        .expect("matching metadata registers");
        assert_eq!(
            service.miden_client.test_call_count(),
            1,
            "the register path DOES call the bridge with() when validation passes"
        );
    }

    const ORIGIN_HEX: &str = "0xdAC17F958D2ee523a2206206994597C13D831ec7";
    // A valid protocol-0.15 account id, reused as the poisoned faucet's id.
    const POISON_FAUCET_HEX: &str = "0xac0000000000dd110000ee000000fc";

    /// Seed a poisoned route: a 27-decimal token registered under the OLD fixed
    /// scheme (`miden = 8`, `scale = 19 > MAX_SCALING_FACTOR`) — unclaimable.
    async fn seed_poisoned_route(service: &ServiceState) {
        let origin = parse_eth_address(ORIGIN_HEX).unwrap();
        service
            .store
            .register_faucet(FaucetEntry {
                faucet_id: AccountId::from_hex(POISON_FAUCET_HEX).unwrap(),
                origin_address: origin,
                origin_network: 0,
                symbol: "TKN".into(),
                origin_decimals: 27,
                miden_decimals: 8,
                scale: 19,
                metadata: Vec::new(),
            })
            .await
            .unwrap();
    }

    fn repair_params() -> RegisterFaucetParams {
        RegisterFaucetParams {
            symbol: "TKN".into(),
            origin_token_address: ORIGIN_HEX.into(),
            origin_network: 0,
            origin_decimals: 27,
            // Ignored under the cap-at-8 scheme (faucet decimals = min(origin, 8));
            // left non-8 here to prove the param has no effect.
            miden_decimals: 9,
            name: None,
        }
    }

    /// An existing route short-circuits: `admin_registerFaucet` is strictly
    /// register-if-absent / return-existing-if-present. It returns the existing
    /// faucet_id and never touches Miden — there is deliberately no live "replace"
    /// path (removing one avoids re-creating the finding #6 split-brain by
    /// orphaning holders of the old faucet). DR repair is a throwaway image, not a
    /// standing endpoint.
    #[tokio::test]
    async fn existing_route_is_idempotent() {
        let service = create_test_service();
        seed_poisoned_route(&service).await;

        let id = admin_register_faucet(service.clone(), repair_params())
            .await
            .unwrap();
        assert_eq!(id, AccountId::from_hex(POISON_FAUCET_HEX).unwrap().to_hex());
        // No faucet deploy attempted — the existing route was returned as-is.
        assert_eq!(service.miden_client.test_call_count(), 0);
    }

    /// Idempotency must win over validation: an existing route is ALWAYS returned,
    /// even when the re-register call carries params that would FAIL new-route
    /// validation (origin_decimals = 27 → capped scale `27 - min(27,8) = 19 >
    /// MAX_SCALING_FACTOR`). Because the existence check runs before any
    /// validation, the poisoned-but-existing route is returned as-is instead of
    /// surfacing a spurious validation error — which would otherwise break the
    /// register-if-absent / return-existing contract.
    #[tokio::test]
    async fn existing_route_returned_even_when_params_would_fail_validation() {
        let service = create_test_service();
        seed_poisoned_route(&service).await;

        // These params would be rejected on a fresh origin (capped scale 27 -
        // min(27,8) = 19 > 18), but the origin already exists, so validation must
        // never run.
        let bad_params = RegisterFaucetParams {
            symbol: "TKN".into(),
            origin_token_address: ORIGIN_HEX.into(),
            origin_network: 0,
            origin_decimals: 27, // capped scale 27 - min(27,8) = 19 > MAX_SCALING_FACTOR
            miden_decimals: 7,   // ignored
            name: None,
        };
        let id = admin_register_faucet(service.clone(), bad_params)
            .await
            .expect("existing route must be returned without validation");
        assert_eq!(id, AccountId::from_hex(POISON_FAUCET_HEX).unwrap().to_hex());
        // Never touched Miden — no create/deploy on the existing-route path.
        assert_eq!(service.miden_client.test_call_count(), 0);
    }

    /// Finding #17 — an unsatisfiable route is rejected up-front and never
    /// persisted. Under the cap-at-8 scheme `origin_decimals = 27` yields the
    /// capped scale `27 - min(27,8) = 19 > MAX_SCALING_FACTOR` (18), i.e.
    /// origin_decimals > MAX_ORIGIN_DECIMALS (26). The `miden_decimals` param is
    /// ignored.
    #[tokio::test]
    async fn rejects_route_exceeding_scaling_factor() {
        let service = create_test_service();
        let params = RegisterFaucetParams {
            symbol: "TKN".into(),
            origin_token_address: ORIGIN_HEX.into(),
            origin_network: 0,
            origin_decimals: 27, // capped scale 27 - min(27,8) = 19 > 18
            miden_decimals: 7,   // ignored
            name: None,
        };
        let err = admin_register_faucet(service.clone(), params)
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("exceeds the shared limit"),
            "unexpected error: {err}"
        );
        // Nothing persisted.
        let origin = parse_eth_address(ORIGIN_HEX).unwrap();
        assert!(
            service
                .store
                .get_faucet_by_origin(&origin, 0)
                .await
                .unwrap()
                .is_none()
        );
    }
}

// ── #154: permissionless native-faucet registration ──────────────────────────

/// The canonical 20-byte origin identity for a Miden-ORIGINATED faucet.
///
/// # Why this is derived and not chosen
///
/// The admin API lets an operator pick `origin_token_address`. Exposing that
/// choice permissionlessly would let anyone SQUAT or OVERWRITE another token's
/// AggLayer identity — register first with someone else's intended address and
/// the real issuer is locked out, or worse, traffic routes to the squatter's
/// faucet. Issue #154 is explicit: do not expose a permissionless API that
/// accepts an unauthenticated arbitrary `origin_token_address`.
///
/// Deriving it from the faucet id removes the choice entirely: the identity is a
/// pure function of the account being registered, so there is nothing to squat.
///
/// # Why the protocol `EthEmbeddedAccountId` encoding, not a proxy-local hash
///
/// The identity is the protocol's canonical `EthEmbeddedAccountId` encoding of
/// the faucet id — the SAME 20-byte form every other address↔account path in
/// this proxy already uses (`address_mapper::account_id_from_address`,
/// `bridge_out::embedded_address`). That encoding is REVERSIBLE:
/// `[4 zero bytes][prefix(8)][suffix(8)]` embeds the AccountId losslessly, so an
/// operator (or the aggkit side) can recover the originating faucet from the
/// on-chain origin address with no lookup table. A proxy-local
/// `last20(keccak(domain || id))` would be one-way and would disagree with the
/// encoding used everywhere else, orphaning that reversibility. It is collision-
/// free by construction (distinct AccountIds embed to distinct addresses) and
/// deterministic, which is what makes registration idempotent.
pub fn derive_native_origin_address(faucet_id: AccountId) -> [u8; 20] {
    miden_standards::interop::eth::EthEmbeddedAccountId::from(faucet_id).into()
}

/// Params for the PERMISSIONLESS registration: the faucet id and nothing else.
///
/// Deliberately carries no metadata and no origin address. Everything else is
/// derived — from the deployed account for name/symbol/decimals, and from the
/// faucet id for the origin identity — so there is no caller-supplied value that
/// could be trusted by mistake.
#[derive(Debug, serde::Deserialize)]
pub struct RegisterNativeFaucetPublicParams {
    pub faucet_id: String,
}

/// Process-wide single-flight for native-faucet registration, SHARED by the
/// public (`miden_registerNativeFaucet`) and admin (`admin_registerNativeFaucet`)
/// paths so the "read authoritative state → decide → emit config note → persist"
/// critical section is serialized ACROSS both. Without a shared lock a public
/// call and a concurrent admin call could each observe "not registered" and both
/// submit a config note (the exact rebind the reviewer flagged).
///
/// Admission is BOUNDED, not queued: the untrusted public path acquires it with
/// `try_lock()` and sheds on contention (an unauthenticated flood cannot pile up
/// unbounded slow tasks behind one lock), while the trusted admin path may wait
/// on `lock().await`. Cross-replica coordination is intentionally out of scope
/// (#142); this is within-one-process only.
static REGISTER_NATIVE_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// The authoritative on-chain binding for a native faucet, read from the bridge
/// account's `faucet_metadata_map` UNDER the registration lock. The local
/// `faucet_registry` alone is not authoritative: after DB loss/lag it can be
/// empty while the bridge still binds the faucet (or its origin), so trusting it
/// would let the public path emit a duplicate/rebinding config note. This is the
/// preflight the reviewer required before any note is emitted.
enum BridgeBinding {
    /// The faucet is bound on the bridge at EXACTLY the requested `(origin,
    /// network)` — registration is a no-op; only the local registry may need
    /// reconciling, and reconciling with the requested origin is SAFE because it
    /// equals the on-chain one.
    AlreadyBound,
    /// The faucet is bound on the bridge but at a DIFFERENT origin than requested
    /// (e.g. it was admin-registered with an operator-chosen origin, and the
    /// public path derives a different one). Refuse to rebind, and never reconcile
    /// a local row with the requested origin — that would disagree with the
    /// bridge. Carries the origin it is ACTUALLY bound to.
    FaucetBoundToDifferentOrigin([u8; 20]),
    /// The requested origin `(address, network)` is already bound to a DIFFERENT
    /// faucet on the bridge — refuse rather than rebind.
    OriginBoundToOther(AccountId),
    /// Neither the faucet nor its origin is on the bridge — safe to register.
    Unbound,
}

/// Read the authoritative bridge binding for `(faucet_id, origin_address,
/// origin_network)` from the on-chain `faucet_metadata_map`. Caller MUST hold
/// [`REGISTER_NATIVE_LOCK`] so the read-decide-emit sequence is atomic w.r.t. the
/// other registration path.
async fn preflight_bridge_binding(
    state: &ServiceState,
    faucet_id: AccountId,
    origin_address: [u8; 20],
    origin_network: u32,
) -> anyhow::Result<BridgeBinding> {
    let bridge_id = state.accounts.0.bridge.0;
    let out = Arc::new(std::sync::Mutex::new(None::<BridgeBinding>));
    let out_write = out.clone();
    state
        .miden_client
        .with(move |client| {
            Box::new(async move {
                // Explicit sync: `get_account` serves the LOCAL view, which can
                // lag the chain. An idempotent "already registered" answer is
                // only trustworthy if the binding it rests on is current
                // (PR#164 re-review).
                client.sync_state().await?;
                let bridge = client
                    .get_account(bridge_id)
                    .await
                    .map_err(|e| {
                        anyhow::anyhow!("preflight: get_account(bridge {bridge_id}): {e}")
                    })?
                    .ok_or_else(|| {
                        anyhow::anyhow!("preflight: bridge account {bridge_id} not found locally")
                    })?;
                let storage = bridge.storage();
                let binding = match crate::metadata_recovery::read_faucet_conversion_metadata(
                    storage, faucet_id,
                ) {
                    // Faucet already bound — is it at the SAME origin we're asked to
                    // register? Only then is it an idempotent no-op.
                    Some(conv)
                        if conv.origin_address == origin_address
                            && conv.origin_network == origin_network =>
                    {
                        BridgeBinding::AlreadyBound
                    }
                    // Bound, but at a different origin than requested — a rebind.
                    Some(conv) => BridgeBinding::FaucetBoundToDifferentOrigin(conv.origin_address),
                    // Faucet not bound; is the requested origin taken by another faucet?
                    None => match crate::metadata_recovery::find_registered_faucet_for_origin(
                        storage,
                        &origin_address,
                        origin_network,
                    ) {
                        Some((other, _conv)) => BridgeBinding::OriginBoundToOther(other),
                        None => BridgeBinding::Unbound,
                    },
                };
                *out_write.lock().unwrap() = Some(binding);
                Ok(())
            })
        })
        .await?;
    out.lock().unwrap().take().ok_or_else(|| {
        anyhow::anyhow!("preflight: bridge-binding read produced no result for faucet {faucet_id}")
    })
}

/// `miden_registerNativeFaucet` — permissionless registration of an
/// already-deployed Miden-native faucet.
///
/// "Permissionless" is scoped precisely: the PUBLIC RPC validates a deterministic
/// request and then the proxy's SERVICE account submits the existing admin
/// `ConfigAggBridgeNote`. The bridge account stays admin-controlled; this does
/// not make arbitrary bridge configuration public.
pub async fn miden_register_native_faucet(
    state: ServiceState,
    params: RegisterNativeFaucetPublicParams,
) -> anyhow::Result<serde_json::Value> {
    let faucet_id = AccountId::from_hex(&params.faucet_id)
        .map_err(|e| anyhow::anyhow!("bad faucet_id {}: {e:?}", params.faucet_id))?;
    let origin_address = derive_native_origin_address(faucet_id);
    let origin_network = state.network_id;

    // Bounded single-flight (shared with the admin path). `try_lock` sheds under
    // contention instead of queueing, so an unauthenticated flood cannot pile up
    // unbounded slow registration tasks behind one lock; and because the admin
    // path takes the SAME lock, a concurrent admin registration cannot slip a
    // rebinding config note past this path's checks. Held to end of function.
    let _guard = REGISTER_NATIVE_LOCK.try_lock().map_err(|_| {
        anyhow::anyhow!(
            "miden_registerNativeFaucet: another native-faucet registration is in progress; \
             retry shortly (bounded admission — requests are not queued)"
        )
    })?;

    // ── conflict checks BEFORE any state change ──────────────────────────────
    // Idempotent for the same faucet; explicit failure for anything that would
    // change an existing binding.
    if let Some(existing) = state.store.get_faucet_by_id(faucet_id).await? {
        if existing.origin_address != origin_address || existing.origin_network != origin_network {
            anyhow::bail!(
                "miden_registerNativeFaucet: faucet {} is already registered with a different \
                 origin identity (network {}), which this method cannot change. It was likely \
                 registered through the admin API with an operator-chosen address; use that API \
                 to modify it. No state was changed.",
                faucet_id.to_hex(),
                existing.origin_network
            );
        }
        // PR#164 re-review: the local row is a CACHE, not proof of an active
        // route. It can be stale — written by an earlier false-success (config
        // note created but never consumed), or left by a partial restore — so
        // answering `already_registered` on its word alone reports a binding the
        // bridge may not have, and the caller never retries. Confirm against the
        // AUTHORITATIVE on-chain binding before claiming success.
        match preflight_bridge_binding(&state, faucet_id, origin_address, origin_network).await? {
            BridgeBinding::AlreadyBound => {
                return Ok(serde_json::json!({
                    "faucet_id": faucet_id.to_hex(),
                    "origin_token_address": format!("0x{}", hex::encode(origin_address)),
                    "origin_network": origin_network,
                    "symbol": existing.symbol,
                    "decimals": existing.miden_decimals,
                    "already_registered": true,
                }));
            }
            BridgeBinding::OriginBoundToOther(other) => {
                anyhow::bail!(
                    "miden_registerNativeFaucet: the local registry claims faucet {} owns this \
                     origin, but the bridge binds it to a DIFFERENT faucet {}. Refusing to \
                     report success on a stale row. No state was changed.",
                    faucet_id.to_hex(),
                    other.to_hex()
                );
            }
            BridgeBinding::FaucetBoundToDifferentOrigin(bound) => {
                anyhow::bail!(
                    "miden_registerNativeFaucet: the local registry records origin 0x{} for \
                     faucet {}, but the bridge binds it to 0x{}. Refusing to report success on a \
                     divergent row. No state was changed.",
                    hex::encode(origin_address),
                    faucet_id.to_hex(),
                    hex::encode(bound),
                );
            }
            BridgeBinding::Unbound => {
                // Stale row: we recorded a registration the bridge never got.
                // Do NOT report success — fall through and re-drive the
                // registration, which heals the row (and now only returns Ok
                // once the bridge binding is actually observed).
                ::metrics::counter!("faucet_registry_stale_row_redriven_total").increment(1);
                tracing::warn!(
                    faucet_id = %faucet_id.to_hex(),
                    "miden_registerNativeFaucet: local registry row exists but the bridge has NO \
                     binding for it (stale/false-success row) — re-driving registration"
                );
            }
        }
    }
    if let Some(other) = state
        .store
        .get_faucet_by_origin(&origin_address, origin_network)
        .await?
        // PR#164 re-review — EXCLUDE our own row. The derived origin is a pure
        // function of `faucet_id`, so a stale row for THIS faucet necessarily
        // matches this lookup too. Bailing on it made the `Unbound` re-drive above
        // unreachable (the heal could never run) and produced a nonsensical error
        // claiming the faucet was "already bound" to ITSELF. That case is fully
        // decided by the get_faucet_by_id branch above — reached here only when it
        // chose to re-drive, and `register_faucet` upserts our own row in place.
        .filter(|other| other.faucet_id != faucet_id)
    {
        // The encoding is collision-free, so this is only reachable when an
        // operator chose this exact address by hand via the admin API. Either
        // way, refuse rather than rebind.
        anyhow::bail!(
            "miden_registerNativeFaucet: the derived origin identity for faucet {} is already \
             bound to faucet {}. Refusing to rebind. No state was changed.",
            faucet_id.to_hex(),
            other.faucet_id.to_hex()
        );
    }

    // ── authoritative metadata from the DEPLOYED account ─────────────────────
    // Same read the admin path performs (#149/PR #150): import if absent, require
    // the operator-owned NATIVE kind, and take name/symbol/decimals from chain.
    let authoritative = Arc::new(std::sync::Mutex::new(None::<AuthoritativeFaucetMetadata>));
    let authoritative_write = authoritative.clone();
    state
        .miden_client
        .with(move |client| {
            Box::new(async move {
                if client.get_account(faucet_id).await.ok().flatten().is_none()
                    && let Err(e) = client.import_account_by_id(faucet_id).await
                {
                    anyhow::bail!(
                        "miden_registerNativeFaucet: cannot import faucet {faucet_id} from the \
                         node (is it deployed and public?): {e}"
                    );
                }
                let faucet_account = client
                    .get_account(faucet_id)
                    .await
                    .map_err(|e| anyhow::anyhow!("get_account({faucet_id}): {e}"))?
                    .ok_or_else(|| anyhow::anyhow!("faucet {faucet_id} not found after import"))?;
                let (kind, faucet) = faucet_ops::classify_faucet_account(&faucet_account)?;
                if kind != faucet_ops::FaucetKind::NativeFungible {
                    anyhow::bail!(
                        "miden_registerNativeFaucet: faucet {faucet_id} is not an operator-owned \
                         native faucet (it is {kind:?}); only Miden-originated tokens can be \
                         registered this way"
                    );
                }
                *authoritative_write.lock().unwrap() = Some(AuthoritativeFaucetMetadata {
                    name: faucet.token_name().as_str().to_string(),
                    symbol: faucet.symbol().to_string(),
                    decimals: faucet.decimals(),
                });
                Ok(())
            })
        })
        .await?;
    let authoritative = authoritative.lock().unwrap().take().ok_or_else(|| {
        anyhow::anyhow!(
            "miden_registerNativeFaucet: could not read faucet {} metadata; refusing to \
             register — no registry row written",
            faucet_id.to_hex()
        )
    })?;

    // Preflight the AUTHORITATIVE on-chain binding under the lock, before any
    // note is emitted. The local registry checks above can be stale (DB loss/lag)
    // or racing the admin path; the bridge's faucet_metadata_map is the source of
    // truth for what is actually bound.
    match preflight_bridge_binding(&state, faucet_id, origin_address, origin_network).await? {
        BridgeBinding::OriginBoundToOther(other) => {
            anyhow::bail!(
                "miden_registerNativeFaucet: the derived origin identity for faucet {} is \
                 already bound on the bridge to a DIFFERENT faucet {}. Refusing to rebind. \
                 No note emitted, no state changed.",
                faucet_id.to_hex(),
                other.to_hex()
            );
        }
        BridgeBinding::FaucetBoundToDifferentOrigin(bound) => {
            // The faucet is on the bridge but at an origin that differs from the
            // one this path derives — it was almost certainly admin-registered
            // with an operator-chosen origin. Reconciling a local row with the
            // derived origin would disagree with the bridge, so refuse.
            anyhow::bail!(
                "miden_registerNativeFaucet: faucet {} is already registered on the bridge with a \
                 different origin identity (0x{}) than this method derives (0x{}); it was likely \
                 registered through the admin API. This method cannot change it. No state changed.",
                faucet_id.to_hex(),
                hex::encode(bound),
                hex::encode(origin_address),
            );
        }
        BridgeBinding::AlreadyBound => {
            // The bridge already binds this faucet; emitting another config note
            // would be a duplicate. Reconcile the local registry (it may be empty
            // after DB loss) from the authoritative metadata, then report the
            // idempotent success WITHOUT touching the bridge.
            use alloy_core::sol_types::SolValue;
            let metadata_bytes = AdminTokenMetadata {
                name: authoritative.name.clone(),
                symbol: authoritative.symbol.clone(),
                decimals: authoritative.decimals,
            }
            .abi_encode_params();
            state
                .store
                .register_faucet(FaucetEntry {
                    faucet_id,
                    origin_address,
                    origin_network,
                    symbol: authoritative.symbol.clone(),
                    origin_decimals: authoritative.decimals,
                    miden_decimals: authoritative.decimals,
                    scale: 0u8,
                    metadata: metadata_bytes,
                })
                .await?;
            return Ok(serde_json::json!({
                "faucet_id": faucet_id.to_hex(),
                "origin_token_address": format!("0x{}", hex::encode(origin_address)),
                "origin_network": origin_network,
                "symbol": authoritative.symbol,
                "decimals": authoritative.decimals,
                "already_registered": true,
            }));
        }
        BridgeBinding::Unbound => { /* safe to register below */ }
    }

    // The shared validator cross-checks REQUESTED against AUTHORITATIVE. This
    // path accepts no caller metadata, so the "request" IS the authoritative
    // triple — the cross-check is satisfied by construction rather than skipped,
    // which keeps one registration pipeline instead of two (#154).
    let derived_params = RegisterNativeFaucetParams {
        faucet_id: faucet_id.to_hex(),
        origin_token_address: format!("0x{}", hex::encode(origin_address)),
        symbol: authoritative.symbol.clone(),
        decimals: authoritative.decimals,
        name: Some(authoritative.name.clone()),
    };
    register_native_validated(
        &state,
        faucet_id,
        origin_address,
        origin_network,
        0u8,
        &derived_params,
        &authoritative,
    )
    .await?;

    metrics::counter!("rpc_permissionless_faucet_registrations_total").increment(1);
    Ok(serde_json::json!({
        "faucet_id": faucet_id.to_hex(),
        "origin_token_address": format!("0x{}", hex::encode(origin_address)),
        "origin_network": origin_network,
        "symbol": authoritative.symbol,
        "decimals": authoritative.decimals,
        "already_registered": false,
    }))
}

#[cfg(test)]
mod permissionless_registration_tests {
    use super::*;

    fn fid(hex: &str) -> AccountId {
        AccountId::from_hex(hex).unwrap()
    }

    /// The property the whole design rests on: the origin identity is a function
    /// of the faucet, so there is nothing for a public caller to choose — and
    /// therefore nothing to squat.
    #[test]
    fn origin_identity_is_deterministic_and_faucet_bound() {
        let a = fid("0xaa0000000000bc310000bc000000de");
        let b = fid("0xac0000000000dd110000ee000000fc");

        assert_eq!(
            derive_native_origin_address(a),
            derive_native_origin_address(a),
            "same faucet must always derive the same identity — this is what makes \
             registration idempotent"
        );
        assert_ne!(
            derive_native_origin_address(a),
            derive_native_origin_address(b),
            "different faucets must not share an origin identity"
        );
    }

    /// A public caller supplies ONLY a faucet id. If the params struct ever grew
    /// an origin address or metadata field, an unauthenticated caller could
    /// influence the canonical identity or the recorded token metadata — the
    /// exact hazard #154 forbids. Deserializing a request that tries to set them
    /// must ignore them rather than honour them.
    #[test]
    fn caller_cannot_supply_an_origin_address_or_metadata() {
        let hostile = serde_json::json!({
            "faucet_id": "0xaa0000000000bc310000bc000000de",
            "origin_token_address": "0x000000000000000000000000000000000000dead",
            "symbol": "SQUAT",
            "decimals": 18,
            "name": "not the real token",
        });
        let parsed: RegisterNativeFaucetPublicParams =
            serde_json::from_value(hostile).expect("extra fields are ignored, not honoured");
        assert_eq!(parsed.faucet_id, "0xaa0000000000bc310000bc000000de");
        // The derived identity is unaffected by anything the caller wrote.
        assert_eq!(
            derive_native_origin_address(fid(&parsed.faucet_id)),
            derive_native_origin_address(fid("0xaa0000000000bc310000bc000000de")),
            "the caller's origin_token_address must have no influence whatsoever"
        );
    }

    /// Interoperability + reversibility: the derived origin address is the
    /// protocol's canonical `EthEmbeddedAccountId` encoding, which every other
    /// address↔account path in the proxy shares. Two properties matter and are
    /// pinned here:
    ///   1. It agrees byte-for-byte with `account_id_from_address`'s inverse —
    ///      i.e. the address we persist round-trips BACK to the exact faucet id.
    ///      A proxy-local keccak identity could never satisfy this.
    ///   2. It is the same 20-byte form the rest of the codebase produces for a
    ///      Miden account (`is_miden_compatible_address` accepts it), so an
    ///      operator can recover the faucet from the on-chain origin with no
    ///      lookup table.
    /// Bounded admission (PR#164 #3): while one registration holds the shared
    /// single-flight lock, a second public-path attempt (`try_lock`) is SHED
    /// immediately rather than queued. This is what stops an unauthenticated
    /// flood from piling up unbounded slow tasks behind one global lock, and it
    /// makes the "bounded, not queued" claim executable rather than inferred.
    #[tokio::test]
    async fn native_registration_admission_is_bounded_not_queued() {
        let held = REGISTER_NATIVE_LOCK.lock().await;
        assert!(
            REGISTER_NATIVE_LOCK.try_lock().is_err(),
            "a concurrent public registration must be shed (not queued) while one is in flight"
        );
        drop(held);
        assert!(
            REGISTER_NATIVE_LOCK.try_lock().is_ok(),
            "admission must be available again once the in-flight registration completes"
        );
    }

    #[test]
    fn origin_identity_is_the_reversible_protocol_encoding() {
        let faucet = fid("0xaa0000000000bc310000bc000000de");
        let origin = derive_native_origin_address(faucet);

        // The persisted origin is a canonical zero-padded Miden address...
        let as_addr = alloy::primitives::Address::from(origin);
        assert!(
            crate::address_mapper::is_miden_compatible_address(as_addr),
            "the origin identity must be a canonical Miden-compatible address"
        );
        // ...and it reverses back to EXACTLY the faucet we registered.
        assert_eq!(
            crate::address_mapper::account_id_from_address(as_addr),
            Some(faucet),
            "the derived origin address must round-trip back to the faucet id — \
             this is the reversibility a keccak identity would lose"
        );
    }
}
