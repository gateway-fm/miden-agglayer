//! Faucet identities are the chain-derived state a bridge-out needs before it
//! can be projected. The bridge's `faucet_metadata_map` is authoritative, so
//! after a PostgreSQL loss they are rebuilt from public state; the faucet
//! account itself is never re-deployed, because its seed is unrecoverable and a
//! second generation would strand balances (Cantina #6).
//!
//! Fail-closed on purpose: the previous best-effort version quarantined the
//! affected exits and let a restore "succeed" over a `depositCount` gap.
//!
//! Restore only. On a live proxy an unregistered bridge faucet means the admin
//! key was used outside the proxy (`faucet_registry_reconciler`); adopting it
//! would launder a compromise.

use std::sync::Arc;

use miden_protocol::account::AccountId;

use crate::metadata_recovery::{
    NetworkRpcMap, enumerate_registered_faucet_ids, read_faucet_conversion_metadata,
};
use crate::miden_client::MidenClientLib;
use crate::store::Store;

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct FaucetBootstrapReport {
    pub rebuilt: usize,
    pub already_known: usize,
}

/// Idempotent and cheap when nothing is missing, so the projector can run it
/// before every restore pass. Needs the bridge account tracked locally.
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
            // All-zero conversion is the native-ETH sentinel, seeded from config
            // at startup rather than from chain.
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
