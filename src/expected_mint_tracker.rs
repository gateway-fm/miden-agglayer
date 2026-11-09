//! Durable watchdog for submitted CLAIM-note consumption.
//!
//! The legacy module, table, and metric names say "expected MINT", but the
//! observed signal is consumption of the CLAIM by our bridge, not delivery of
//! its output MINT. The tracker does not prove recipient credit or retry claims.
//!
//! AwaitingClaim -> AlertPending is persisted before returning an alert. The
//! caller emits its log/metric and then acknowledges that exact generation.
//! Unacknowledged alerts survive restart and are delivered at least once; a
//! crash after emission and before acknowledgement can produce a duplicate.
//! Explicit resubmission supersedes the previous watchdog (including a due
//! alert) with a new generation; stale acknowledgement cannot delete the new
//! watchdog. Otherwise due alerts remain until acknowledged, even if the claim
//! subsequently lands.

use std::collections::HashSet;
use std::sync::Arc;

use crate::store::Store;

/// 32-byte CLAIM note identity observed in the consumed-note inventory.
pub type ClaimNoteId = [u8; 32];
/// Compatibility name; these values have always identified CLAIM notes.
pub type MintNoteId = ClaimNoteId;
/// 32-byte global index (claim identifier on the L1 side).
pub type GlobalIndex = [u8; 32];

/// Retained for callers configuring the old cache. Durable rows are read on
/// every tick; an in-memory mirror is unnecessary for this bounded watchdog.
pub const DEFAULT_CACHE_CAPACITY: usize = 10_000;

pub struct ExpectedClaimTracker {
    store: Arc<dyn Store>,
}
/// Compatibility name for existing submission paths.
pub type ExpectedMintTracker = ExpectedClaimTracker;

/// Identifies an alert delivery. Generations are never reused, including after
/// the previous watchdog was deleted and the same claim was submitted again.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClaimAlertToken {
    pub generation: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClaimStatus {
    /// The CLAIM was consumed by the bridge. This is not a MINT-delivery receipt.
    Landed,
    Pending {
        ticks_pending: u32,
    },
    /// Persisted delivery pending; acknowledge only after emitting the alert.
    StaleAlert {
        ticks_pending: u32,
        token: ClaimAlertToken,
    },
}
/// Compatibility name for existing monitor integrations.
pub type MintStatus = ClaimStatus;

impl ExpectedClaimTracker {
    pub fn new(store: Arc<dyn Store>) -> Self {
        Self { store }
    }

    pub fn with_capacity(store: Arc<dyn Store>, _capacity: usize) -> Self {
        Self::new(store)
    }

    /// Record a submitted CLAIM note. Explicit resubmission supersedes any old
    /// watchdog/undelivered alert for this claim and starts a fresh generation.
    pub async fn record_expected(
        &self,
        global_index: GlobalIndex,
        claim_note: ClaimNoteId,
    ) -> anyhow::Result<()> {
        self.store
            .expected_mint_record(&global_index, &claim_note)
            .await
    }

    pub async fn tick(
        &self,
        consumed_claim_ids: &HashSet<ClaimNoteId>,
        stale_threshold_ticks: u32,
    ) -> anyhow::Result<Vec<(GlobalIndex, ClaimStatus)>> {
        let rows = self.store.expected_mint_load_all().await?;
        let mut results = Vec::with_capacity(rows.len());
        for row in rows {
            // Once due, delivery remains pending even if the claim lands before
            // the next scan. Only acknowledgement may remove the pending alert.
            if row.alert_pending {
                results.push((
                    row.global_index,
                    ClaimStatus::StaleAlert {
                        ticks_pending: row.ticks_pending,
                        token: ClaimAlertToken {
                            generation: row.generation,
                        },
                    },
                ));
                continue;
            }
            if consumed_claim_ids.contains(&row.claim_note) {
                if self
                    .store
                    .expected_mint_remove_generation(&row.global_index, row.generation)
                    .await?
                {
                    results.push((row.global_index, ClaimStatus::Landed));
                }
                continue;
            }
            let next_ticks = row.ticks_pending.saturating_add(1);
            let alert_pending = next_ticks >= stale_threshold_ticks;
            // Compare-and-update: a concurrent resubmission must retain its new
            // window, and another ticker cannot move an alert back to pending.
            if !self
                .store
                .expected_mint_update_tick(
                    &row.global_index,
                    row.generation,
                    next_ticks,
                    alert_pending,
                )
                .await?
            {
                continue;
            }
            let status = if alert_pending {
                ClaimStatus::StaleAlert {
                    ticks_pending: next_ticks,
                    token: ClaimAlertToken {
                        generation: row.generation,
                    },
                }
            } else {
                ClaimStatus::Pending {
                    ticks_pending: next_ticks,
                }
            };
            results.push((row.global_index, status));
        }
        results.sort_by_key(|(gi, _)| *gi);
        Ok(results)
    }

    /// Call after the alert log/metric has been emitted. A failed acknowledgement
    /// leaves durable delivery pending; the next tick may emit it again.
    pub async fn acknowledge_alert(
        &self,
        global_index: GlobalIndex,
        token: ClaimAlertToken,
    ) -> anyhow::Result<()> {
        self.store
            .expected_mint_acknowledge_alert(&global_index, token.generation)
            .await
    }

    pub async fn pending_count(&self) -> anyhow::Result<usize> {
        Ok(self.store.expected_mint_load_all().await?.len())
    }

    /// Clear an awaiting CLAIM only after authoritative bridge-consumption
    /// evidence, never merely the creation transaction committing. Already-due
    /// alerts remain pending until delivered and acknowledged.
    pub async fn mark_landed(&self, global_index: GlobalIndex) -> anyhow::Result<()> {
        self.store.expected_mint_remove(&global_index).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::memory::InMemoryStore;

    fn setup() -> (Arc<InMemoryStore>, ExpectedClaimTracker) {
        let store = Arc::new(InMemoryStore::new());
        let tracker = ExpectedClaimTracker::new(store.clone());
        (store, tracker)
    }

    fn alert_token(results: &[(GlobalIndex, ClaimStatus)]) -> ClaimAlertToken {
        match results[0].1 {
            ClaimStatus::StaleAlert { token, .. } => token,
            other => panic!("expected due alert, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn consumed_claim_is_removed_while_other_claim_keeps_its_window() {
        let (_, tracker) = setup();
        tracker.record_expected([1; 32], [11; 32]).await.unwrap();
        tracker.record_expected([2; 32], [22; 32]).await.unwrap();
        let results = tracker.tick(&HashSet::from([[11; 32]]), 3).await.unwrap();
        assert_eq!(
            results,
            vec![
                ([1; 32], ClaimStatus::Landed),
                ([2; 32], ClaimStatus::Pending { ticks_pending: 1 }),
            ]
        );
        assert_eq!(tracker.pending_count().await.unwrap(), 1);
    }

    #[tokio::test]
    async fn restart_preserves_pending_window_and_due_unacknowledged_alert() {
        let (store, tracker) = setup();
        tracker.record_expected([1; 32], [11; 32]).await.unwrap();
        assert_eq!(
            tracker.tick(&HashSet::new(), 2).await.unwrap()[0].1,
            ClaimStatus::Pending { ticks_pending: 1 }
        );
        drop(tracker);
        let tracker = ExpectedClaimTracker::new(store.clone());
        let due = tracker.tick(&HashSet::new(), 2).await.unwrap();
        assert!(matches!(
            due[0].1,
            ClaimStatus::StaleAlert {
                ticks_pending: 2,
                ..
            }
        ));
        drop(tracker); // Crash before the caller emits the alert.
        let tracker = ExpectedClaimTracker::new(store.clone());
        assert_eq!(tracker.tick(&HashSet::new(), 2).await.unwrap(), due);
        tracker
            .acknowledge_alert([1; 32], alert_token(&due))
            .await
            .unwrap();
        drop(tracker);
        let tracker = ExpectedClaimTracker::new(store);
        assert!(tracker.tick(&HashSet::new(), 2).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn failed_acknowledgement_redelivers_until_acknowledged() {
        let (store, tracker) = setup();
        tracker.record_expected([1; 32], [11; 32]).await.unwrap();
        let due = tracker.tick(&HashSet::new(), 1).await.unwrap();
        store.fail_monitor_operation("expected_mint_acknowledge_alert", true);
        assert!(
            tracker
                .acknowledge_alert([1; 32], alert_token(&due))
                .await
                .is_err()
        );
        let restarted = ExpectedClaimTracker::new(store.clone());
        assert_eq!(restarted.tick(&HashSet::new(), 1).await.unwrap(), due);
        store.fail_monitor_operation("expected_mint_acknowledge_alert", false);
        restarted
            .acknowledge_alert([1; 32], alert_token(&due))
            .await
            .unwrap();
        assert!(restarted.tick(&HashSet::new(), 1).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn due_alert_must_be_durable_before_it_can_be_emitted() {
        let (store, tracker) = setup();
        tracker.record_expected([1; 32], [11; 32]).await.unwrap();
        store.fail_monitor_operation("expected_mint_update_tick", true);
        assert!(tracker.tick(&HashSet::new(), 1).await.is_err());
        let row = store.expected_mint_load_all().await.unwrap()[0];
        assert_eq!(row.ticks_pending, 0);
        assert!(!row.alert_pending);
        store.fail_monitor_operation("expected_mint_update_tick", false);
        assert!(matches!(
            tracker.tick(&HashSet::new(), 1).await.unwrap()[0].1,
            ClaimStatus::StaleAlert { .. }
        ));
    }

    #[tokio::test]
    async fn resubmission_supersedes_old_alert_without_accepting_its_stale_ack_or_tick() {
        let (store, tracker) = setup();
        tracker.record_expected([1; 32], [11; 32]).await.unwrap();
        let due = tracker.tick(&HashSet::new(), 1).await.unwrap();
        let old = alert_token(&due);
        tracker.record_expected([1; 32], [11; 32]).await.unwrap();
        let resumed = store.expected_mint_load_all().await.unwrap();
        assert_eq!(
            resumed.len(),
            1,
            "resubmission intentionally supersedes the old alert"
        );
        assert!(!resumed[0].alert_pending);
        assert_ne!(resumed[0].generation, old.generation);
        tracker.acknowledge_alert([1; 32], old).await.unwrap();
        assert!(
            !store
                .expected_mint_update_tick(&[1; 32], old.generation, 99, true)
                .await
                .unwrap()
        );
        assert!(
            !store
                .expected_mint_remove_generation(&[1; 32], old.generation)
                .await
                .unwrap()
        );
        assert_eq!(
            tracker.tick(&HashSet::new(), 3).await.unwrap()[0].1,
            ClaimStatus::Pending { ticks_pending: 1 }
        );
        let new_due = tracker.tick(&HashSet::new(), 1).await.unwrap();
        assert_ne!(alert_token(&new_due), old);
        tracker.acknowledge_alert([1; 32], old).await.unwrap();
        assert_eq!(tracker.tick(&HashSet::new(), 1).await.unwrap(), new_due);
    }

    #[tokio::test]
    async fn generation_is_not_reused_after_the_previous_row_was_deleted() {
        let (_, tracker) = setup();
        tracker.record_expected([1; 32], [11; 32]).await.unwrap();
        let old = alert_token(&tracker.tick(&HashSet::new(), 1).await.unwrap());
        tracker.acknowledge_alert([1; 32], old).await.unwrap();
        tracker.record_expected([1; 32], [11; 32]).await.unwrap();
        let due = tracker.tick(&HashSet::new(), 1).await.unwrap();
        assert_ne!(alert_token(&due), old);
        tracker.acknowledge_alert([1; 32], old).await.unwrap();
        assert_eq!(tracker.tick(&HashSet::new(), 1).await.unwrap(), due);
    }

    #[tokio::test]
    async fn late_claim_consumption_does_not_silently_delete_a_due_alert() {
        let (_, tracker) = setup();
        tracker.record_expected([1; 32], [11; 32]).await.unwrap();
        let due = tracker.tick(&HashSet::new(), 1).await.unwrap();
        tracker.mark_landed([1; 32]).await.unwrap();
        assert_eq!(
            tracker.tick(&HashSet::from([[11; 32]]), 1).await.unwrap(),
            due
        );
        tracker
            .acknowledge_alert([1; 32], alert_token(&due))
            .await
            .unwrap();
        assert_eq!(tracker.pending_count().await.unwrap(), 0);
    }

    #[tokio::test]
    async fn acknowledged_alert_is_one_shot_and_large_threshold_stays_pending() {
        let (_, tracker) = setup();
        tracker.record_expected([1; 32], [11; 32]).await.unwrap();
        for ticks_pending in 1..10 {
            assert_eq!(
                tracker.tick(&HashSet::new(), u32::MAX).await.unwrap()[0].1,
                ClaimStatus::Pending { ticks_pending }
            );
        }
        let due = tracker.tick(&HashSet::new(), 10).await.unwrap();
        tracker
            .acknowledge_alert([1; 32], alert_token(&due))
            .await
            .unwrap();
        for _ in 0..10 {
            assert!(tracker.tick(&HashSet::new(), 10).await.unwrap().is_empty());
        }
    }
}
