//! Faucet-registry SECURITY reconciler (a tripwire, not an adopter).
//!
//! The bridge admin (== the proxy's `service` account) is the ONLY party that may
//! register a faucet on the bridge, and `admin_register_faucet` /
//! `admin_register_native_faucet` write the local `faucet_registry` row alongside the
//! on-chain `ConfigAggBridgeNote`. So in steady state every faucet the bridge knows
//! about MUST also have a local store row.
//!
//! A faucet registered on the bridge that the proxy has NO local row for therefore
//! means the bridge admin key was used OUTSIDE the proxy — a compromise or a leaked
//! key. This reconciler periodically enumerates the bridge's on-chain faucet
//! registrations and, if it finds one with no local row that persists past a short
//! grace window, HALTS THE PROXY (fail-closed, non-zero exit).
//!
//! It deliberately does NOT adopt unknown faucets — unlike `L1InfoTreeIndexer`, which
//! adopts on-chain GER state regardless of who wrote it. GERs are permissionlessly
//! writable by design; faucet registration is admin-only, so an unexpected one is an
//! attack signal, and silently adopting it would launder a compromise. The ONLY
//! sanctioned path to import faucets the proxy did not itself register is `--restore`
//! (`restore::restore_faucet_identities`).
//!
//! ## Grace window (why not halt on the first sighting)
//! The proxy's own registration lands the on-chain note slightly before it commits the
//! store row, so a naive check would false-halt on a registration that is merely
//! in-flight. An unknown faucet must be observed for `grace_ticks` CONSECUTIVE polls
//! before it trips the wire: a legitimate registration's row commits within one tick
//! (the streak resets), while a real external registration never gets a row and
//! persists until the grace window elapses.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use miden_protocol::account::AccountId;
use tokio::sync::oneshot;

use crate::metadata_recovery::enumerate_registered_faucet_ids;
use crate::miden_client::MidenClient;
use crate::store::Store;

/// Default interval between bridge-registry scans.
const DEFAULT_POLL_INTERVAL: Duration = Duration::from_secs(30);
/// Consecutive scans an unknown faucet must survive before it halts the proxy. With
/// the 30s default interval this is a ~90s grace window — far longer than the gap
/// between a proxy registration's on-chain note and its store-row commit, but short
/// enough that a real compromise is caught promptly.
const DEFAULT_GRACE_TICKS: u32 = 3;

pub struct FaucetRegistryReconciler {
    miden_client: Arc<MidenClient>,
    store: Arc<dyn Store>,
    bridge_id: AccountId,
    poll_interval: Duration,
    grace_ticks: u32,
}

impl FaucetRegistryReconciler {
    pub fn new(
        miden_client: Arc<MidenClient>,
        store: Arc<dyn Store>,
        bridge_id: AccountId,
    ) -> Self {
        Self {
            miden_client,
            store,
            bridge_id,
            poll_interval: DEFAULT_POLL_INTERVAL,
            grace_ticks: DEFAULT_GRACE_TICKS,
        }
    }

    pub fn with_poll_interval(mut self, interval: Duration) -> Self {
        self.poll_interval = interval;
        self
    }

    pub fn with_grace_ticks(mut self, grace_ticks: u32) -> Self {
        self.grace_ticks = grace_ticks.max(1);
        self
    }

    /// Spawn the reconciler as a tokio task. Returns a oneshot sender for graceful
    /// shutdown — drop it or send `()` to stop the loop. Transient poll errors are
    /// logged and retried; only a persistent unknown faucet halts the process.
    pub fn spawn(self) -> oneshot::Sender<()> {
        let (shutdown_tx, mut shutdown_rx) = oneshot::channel();

        tokio::spawn(async move {
            tracing::info!(
                bridge = %self.bridge_id,
                poll_interval_ms = self.poll_interval.as_millis() as u64,
                grace_ticks = self.grace_ticks,
                "FaucetRegistryReconciler starting (security tripwire — halts on unknown bridge faucet)"
            );

            let mut ticker = tokio::time::interval(self.poll_interval);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            // Consume the immediate first tick so the first scan waits one interval —
            // gives the initial sync time to populate the bridge account locally.
            ticker.tick().await;

            // faucet_id -> consecutive polls seen unknown-in-store.
            let mut streaks: HashMap<AccountId, u32> = HashMap::new();

            loop {
                tokio::select! {
                    biased;
                    _ = &mut shutdown_rx => {
                        tracing::info!("FaucetRegistryReconciler shutdown requested");
                        break;
                    }
                    _ = ticker.tick() => {}
                }

                if let Err(e) = self.poll_once(&mut streaks).await {
                    tracing::warn!(error = %e, "FaucetRegistryReconciler poll failed, retrying");
                    metrics::counter!("faucet_registry_reconciler_poll_errors_total").increment(1);
                }
            }

            tracing::info!("FaucetRegistryReconciler stopped");
        });

        shutdown_tx
    }

    /// One scan: enumerate the bridge's on-chain faucet registrations, find any with no
    /// local store row, advance/reset per-faucet streaks, and halt if a streak reaches
    /// the grace threshold. A store read error is treated as "known" this tick so an
    /// infra blip cannot trip the wire.
    async fn poll_once(&self, streaks: &mut HashMap<AccountId, u32>) -> anyhow::Result<()> {
        let bridge_id = self.bridge_id;
        let store = self.store.clone();
        // Anomalous bridge faucets this tick, each with a human reason. Two anomaly classes:
        //   - no local store row (registered outside the proxy — admin key used elsewhere)
        //   - unknown faucet TYPE (account matches no supported kind — malformed/hostile)
        let anomalies: Arc<std::sync::Mutex<Vec<(AccountId, String)>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let anomalies_inner = anomalies.clone();

        self.miden_client
            .with(move |client| {
                Box::new(async move {
                    let Some(bridge_account) =
                        client.get_account(bridge_id).await.ok().flatten()
                    else {
                        // Bridge account not synced locally yet — skip this tick.
                        return Ok(());
                    };
                    let ids = enumerate_registered_faucet_ids(bridge_account.storage());
                    let mut found = Vec::new();
                    for id in ids {
                        // Type check FIRST — only when the faucet account is locally synced,
                        // so sync lag (account not yet imported) is skipped this tick rather
                        // than mistaken for an unsupported type.
                        if let Ok(Some(acct)) = client.get_account(id).await
                            && let Err(e) = crate::faucet_ops::classify_faucet_account(&acct)
                        {
                            found.push((id, format!("unknown faucet TYPE: {e}")));
                            continue;
                        }
                        // Store membership. A read error is treated as "known" this tick so an
                        // infra blip cannot trip the wire.
                        match store.get_faucet_by_id(id).await {
                            Ok(Some(_)) => {}
                            Ok(None) => found.push((
                                id,
                                "no local faucet_registry row (registered outside the proxy)"
                                    .to_string(),
                            )),
                            Err(e) => tracing::warn!(
                                faucet_id = %id,
                                error = ?e,
                                "reconciler: get_faucet_by_id failed; not counting as unknown this tick"
                            ),
                        }
                    }
                    *anomalies_inner.lock().unwrap() = found;
                    Ok(())
                })
            })
            .await?;

        let found = anomalies.lock().unwrap().clone();
        let observed: Vec<AccountId> = found.iter().map(|(id, _)| *id).collect();
        let reasons: HashMap<AccountId, String> = found.into_iter().collect();

        if let Some(tripped) = Self::evaluate(&observed, streaks, self.grace_ticks) {
            let reason = reasons
                .get(&tripped)
                .map(String::as_str)
                .unwrap_or("anomalous bridge faucet");
            tracing::error!(
                faucet_id = %tripped,
                reason,
                grace_ticks = self.grace_ticks,
                "SECURITY TRIPWIRE: the bridge registers a faucet that is anomalous \
                 (see `reason`), persisting past the grace window. Either the bridge admin key \
                 was used OUTSIDE the proxy (compromise/leak) or an unsupported faucet type was \
                 registered. Halting fail-closed. Import a legitimate faucet via --restore only \
                 after confirming it."
            );
            metrics::counter!("faucet_registry_reconciler_unknown_faucet_total").increment(1);
            // Let tracing flush the fatal line before the process dies.
            tokio::time::sleep(Duration::from_millis(200)).await;
            std::process::exit(1);
        }

        Ok(())
    }

    /// Pure decision core (no I/O, no exit) so the streak/grace logic is unit-testable.
    /// Advances per-faucet streaks for every currently-unknown faucet, drops streaks for
    /// faucets that are no longer unknown (their row committed), and returns the first
    /// faucet whose streak has reached `grace_ticks` — the one that should halt the proxy.
    /// Generic over the id type (faucet ids are opaque keys here) so it can be tested with
    /// plain integers instead of constructing chain AccountIds.
    fn evaluate<K: Eq + std::hash::Hash + Copy + std::fmt::Display>(
        observed: &[K],
        streaks: &mut HashMap<K, u32>,
        grace_ticks: u32,
    ) -> Option<K> {
        // Reset streaks for faucets that are no longer unknown (their row committed).
        streaks.retain(|id, _| observed.contains(id));

        let mut tripped = None;
        for id in observed {
            let count = streaks.entry(*id).or_insert(0);
            *count += 1;
            if *count >= grace_ticks {
                tripped.get_or_insert(*id);
            } else {
                tracing::warn!(
                    faucet_id = %id,
                    streak = *count,
                    grace_ticks,
                    "reconciler: unknown bridge faucet observed; will HALT if it persists"
                );
            }
        }
        tripped
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // The reconciler treats faucet ids as opaque map keys, so `evaluate` is generic; use
    // plain integers as stand-in ids to test the streak/grace logic without constructing
    // chain AccountIds.
    fn faucet_id(seed: u8) -> u64 {
        seed as u64
    }

    #[test]
    fn unknown_faucet_trips_only_after_grace_window() {
        let mut streaks = HashMap::new();
        let f = faucet_id(1);
        // Seen unknown but under the grace threshold on the first two scans.
        assert_eq!(
            FaucetRegistryReconciler::evaluate(&[f], &mut streaks, 3),
            None
        );
        assert_eq!(
            FaucetRegistryReconciler::evaluate(&[f], &mut streaks, 3),
            None
        );
        // Third consecutive scan reaches grace_ticks -> trips.
        assert_eq!(
            FaucetRegistryReconciler::evaluate(&[f], &mut streaks, 3),
            Some(f)
        );
    }

    #[test]
    fn registration_landing_within_grace_resets_streak() {
        let mut streaks = HashMap::new();
        let f = faucet_id(2);
        // Unknown for two scans (proxy registration in flight)...
        assert_eq!(
            FaucetRegistryReconciler::evaluate(&[f], &mut streaks, 3),
            None
        );
        assert_eq!(
            FaucetRegistryReconciler::evaluate(&[f], &mut streaks, 3),
            None
        );
        // ...then the store row commits: no longer unknown -> streak drops.
        assert_eq!(
            FaucetRegistryReconciler::evaluate(&[], &mut streaks, 3),
            None
        );
        assert!(streaks.is_empty());
        // A later transient sighting starts from zero again, so it does NOT immediately trip.
        assert_eq!(
            FaucetRegistryReconciler::evaluate(&[f], &mut streaks, 3),
            None
        );
    }

    #[test]
    fn grace_ticks_one_trips_immediately() {
        let mut streaks = HashMap::new();
        let f = faucet_id(3);
        assert_eq!(
            FaucetRegistryReconciler::evaluate(&[f], &mut streaks, 1),
            Some(f)
        );
    }

    #[test]
    fn distinct_unknown_faucets_tracked_independently() {
        let mut streaks = HashMap::new();
        let (a, b) = (faucet_id(4), faucet_id(5));
        // `a` unknown for two scans; `b` only appears on the second.
        assert_eq!(
            FaucetRegistryReconciler::evaluate(&[a], &mut streaks, 3),
            None
        );
        assert_eq!(
            FaucetRegistryReconciler::evaluate(&[a, b], &mut streaks, 3),
            None
        );
        // Third scan: `a` reaches 3 and trips; `b` is only at 2.
        assert_eq!(
            FaucetRegistryReconciler::evaluate(&[a, b], &mut streaks, 3),
            Some(a)
        );
    }
}
