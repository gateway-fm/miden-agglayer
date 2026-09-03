//! Faucet-identity BOOTSTRAP — the chain-derived state the canonical projector
//! needs before the first dependent B2AGG / CLAIM is projected (issue #167,
//! item 5).
//!
//! A B2AGG bridge-out is projectable only when its faucet's origin identity
//! (L1 token address / network / scale) is known locally; a CLAIM's synthetic
//! calldata resolves through the same registry. The bridge account holds the
//! authoritative `faucet_metadata_map`, so after a PostgreSQL loss every
//! registered faucet's identity can be rebuilt from public on-chain state —
//! faucets are bridge-owned (mint/burn), no signing key is involved, and the
//! account is never re-deployed (its random seed is unrecoverable; a re-deploy
//! would strand balances in a second generation — Cantina #6).
//!
//! This used to be a restore-private phase (`restore::restore_faucet_identities`,
//! best-effort: per-faucet failures logged and skipped, historical exits then
//! QUARANTINED as `UnknownFaucet` and the restore "succeeded" over a
//! `depositCount` gap the emitted-frontier gate later refused). It is now a
//! normal reconciliation primitive the projector runs itself in restore
//! posture, FAIL-CLOSED: an unknown faucet type, a failed rebuild, or an
//! unavailable bridge account halts projection before any dependent event
//! seals, instead of quarantining history and continuing.
//!
//! Live posture deliberately does NOT run this: on a live proxy a bridge
//! faucet with no local row is a security signal (the admin key was used
//! outside the proxy — see `faucet_registry_reconciler`), and adopting it would
//! launder a compromise. `--restore` is the one sanctioned import path.

use std::sync::Arc;

use miden_protocol::account::AccountId;

use crate::metadata_recovery::{
    NetworkRpcMap, enumerate_registered_faucet_ids, read_faucet_conversion_metadata,
};
use crate::miden_client::MidenClientLib;
use crate::store::Store;

/// What one bootstrap pass did.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct FaucetBootstrapReport {
    /// `faucet_registry` rows rebuilt from the bridge's `faucet_metadata_map`.
    pub rebuilt: usize,
    /// Faucets the bridge registers that already had a local row.
    pub already_known: usize,
}

/// Rebuild every MISSING local faucet-identity row from the bridge's
/// authoritative `faucet_metadata_map`, fail-closed.
///
/// Idempotent and cheap when nothing is missing (one local bridge-account read
/// plus one registry lookup per registered faucet), so the projector runs it at
/// the start of every restore pass. Requires the bridge account to be tracked
/// locally (restore's Phase 0 reimports it).
///
/// Errors (all halt the caller before it projects anything):
/// * the bridge account is not available locally;
/// * a registered faucet matches no supported faucet kind (`UNKNOWN faucet
///   type` — malformed or hostile registration; counted in
///   `restore_unknown_faucet_type_total`);
/// * a faucet's identity could not be read back from chain, or the rebuilt row
///   could not be persisted (counted in
///   `restore_faucet_identity_rebuild_failed_total`).
pub async fn rebuild_missing_faucet_identities(
    client: &mut MidenClientLib,
    store: &Arc<dyn Store>,
    bridge_id: AccountId,
    network_rpcs: &NetworkRpcMap,
) -> anyhow::Result<FaucetBootstrapReport> {
    let bridge_account = client
        .get_account(bridge_id)
        .await
        .map_err(|e| anyhow::anyhow!("faucet bootstrap: get_account({bridge_id}): {e}"))?
        .ok_or_else(|| {
            anyhow::anyhow!(
                "faucet bootstrap: bridge account {bridge_id} is not available locally — \
                 it must be re-imported before history can be projected"
            )
        })?;

    let faucet_ids = enumerate_registered_faucet_ids(bridge_account.storage());
    tracing::debug!(
        count = faucet_ids.len(),
        "faucet bootstrap: bridge registers {} faucet(s); checking local rows",
        faucet_ids.len()
    );

    let mut report = FaucetBootstrapReport::default();
    for faucet_id in faucet_ids {
        if store.get_faucet_by_id(faucet_id).await?.is_some() {
            report.already_known += 1;
            continue;
        }
        let Some(conversion) = read_faucet_conversion_metadata(bridge_account.storage(), faucet_id)
        else {
            // All-zero conversion = the native-ETH sentinel (seeded from config at
            // startup, never rebuilt from chain) or an unregistered id. A registered
            // NATIVE faucet has a non-zero origin (origin_network == network_id,
            // scale 0), so it does NOT land here.
            continue;
        };
        let entry = match crate::faucet_ops::rebuild_faucet_entry_from_chain(
            client,
            &bridge_account,
            faucet_id,
            &conversion,
            network_rpcs
                .get(&conversion.origin_network)
                .map(String::as_str),
        )
        .await
        {
            Ok(entry) => entry,
            Err(e) => {
                if format!("{e:?}").contains("UNKNOWN faucet type") {
                    ::metrics::counter!("restore_unknown_faucet_type_total").increment(1);
                    anyhow::bail!(
                        "faucet bootstrap: faucet {faucet_id} registered in the bridge matches \
                         no supported faucet kind (malformed or hostile registration) — \
                         refusing to project history that depends on it: {e:#}"
                    );
                }
                ::metrics::counter!("restore_faucet_identity_rebuild_failed_total").increment(1);
                anyhow::bail!(
                    "faucet bootstrap: could not rebuild the identity of faucet {faucet_id} \
                     from chain (origin network {}) — every historical bridge-out minted by \
                     it would be quarantined, so projection halts here instead: {e:#}",
                    conversion.origin_network
                );
            }
        };
        let (origin_network, scale) = (entry.origin_network, entry.scale);
        store.register_faucet(entry).await.map_err(|e| {
            ::metrics::counter!("restore_faucet_identity_rebuild_failed_total").increment(1);
            anyhow::anyhow!("faucet bootstrap: register_faucet({faucet_id}) failed: {e:#}")
        })?;
        report.rebuilt += 1;
        ::metrics::counter!("restore_faucet_identity_rebuilt_total").increment(1);
        tracing::info!(
            faucet_id = %faucet_id,
            origin_network,
            scale,
            "faucet bootstrap: rebuilt missing faucet_registry row from the bridge's \
             faucet_metadata_map (Cantina #6)"
        );
    }
    Ok(report)
}
