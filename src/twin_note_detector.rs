//! Twin-NoteId detector — Cantina #6 monitor.
//!
//! Cantina #6 reports that B2AGG reclaim authorises against
//! `NoteMetadata.sender`, but `NoteId` and `Nullifier` ignore metadata.
//! An attacker can clone a victim's public B2AGG note: keep the recipient
//! digest and assets identical (same NoteId, same nullifier), substitute
//! their own `metadata.sender`, and submit it. When the attacker reclaims
//! the twin, the shared nullifier is consumed and the victim's original
//! B2AGG can no longer be reclaimed OR consumed by the bridge — funds are
//! permanently stranded.
//!
//! The aggkit-side defense is detection: cross-index every observed note
//! by its `NoteId`, and on a second observation with a DIFFERENT
//! `note.commitment()` (i.e. different metadata), page critical and
//! freeze claim/bridge-out processing for the affected asset.
//!
//! This module exposes the in-memory cross-index. The wiring point that
//! feeds it from observed notes lives in `bridge_out::on_post_sync` (a
//! separate commit). The unit tests pin the predicate's contract — same
//! NoteId + same commitment = legitimate dedup; same NoteId + different
//! commitment = Cantina #6 signature.

use std::collections::HashMap;
use std::sync::RwLock;

/// 32-byte NoteId (stable across the same recipient+asset_commitment pair).
pub type NoteIdBytes = [u8; 32];
/// 32-byte full note commitment (changes when metadata changes).
pub type NoteCommitment = [u8; 32];

/// Cross-index of observed notes by `NoteId` → set of distinct commitments
/// observed. The first commitment is the legitimate one; any subsequent
/// distinct commitment for the same NoteId is the Cantina #6 twin signature.
pub struct TwinNoteDetector {
    seen: RwLock<HashMap<NoteIdBytes, Vec<NoteCommitment>>>,
}

/// Outcome of a `record` call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// First time this NoteId has been observed.
    New,
    /// Same NoteId, same commitment as a previous record — legitimate
    /// duplicate (e.g. re-sync, replay during restore). Not an alert.
    LegitimateDuplicate,
    /// Same NoteId but a NEW commitment — Cantina #6 twin signature.
    /// `prior_commitments` returns every previously-seen commitment for
    /// this NoteId so the operator can attribute the original vs the
    /// twin.
    TwinDetected {
        prior_commitments: Vec<NoteCommitment>,
    },
}

impl Default for TwinNoteDetector {
    fn default() -> Self {
        Self::new()
    }
}

impl TwinNoteDetector {
    pub fn new() -> Self {
        Self {
            seen: RwLock::new(HashMap::new()),
        }
    }

    /// Record an observed `(NoteId, commitment)` pair. Returns the outcome.
    pub fn record(&self, note_id: NoteIdBytes, commitment: NoteCommitment) -> Outcome {
        let mut map = self.seen.write().expect("TwinNoteDetector lock poisoned");
        match map.get_mut(&note_id) {
            None => {
                map.insert(note_id, vec![commitment]);
                Outcome::New
            }
            Some(existing) => {
                if existing.contains(&commitment) {
                    Outcome::LegitimateDuplicate
                } else {
                    let prior = existing.clone();
                    existing.push(commitment);
                    Outcome::TwinDetected {
                        prior_commitments: prior,
                    }
                }
            }
        }
    }

    /// Total distinct NoteIds observed since startup.
    pub fn distinct_note_ids(&self) -> usize {
        self.seen.read().map(|m| m.len()).unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Cantina #6 — repro+regression. The detector must distinguish three
    /// states cleanly:
    /// - First observation → `New`
    /// - Same NoteId + same commitment → `LegitimateDuplicate` (no alert)
    /// - Same NoteId + different commitment → `TwinDetected` with prior
    ///   commitments listed for attribution
    ///
    /// Pre-fix the detector did not exist; aggkit had no signal for the
    /// Cantina #6 attacker pattern.
    #[test]
    fn cantina_6_twin_detector_three_states() {
        let d = TwinNoteDetector::new();
        let id_a = [0xAAu8; 32];
        let id_b = [0xBBu8; 32];
        let c1 = [0x11u8; 32];
        let c2 = [0x22u8; 32];

        // First observation of (id_a, c1) — New.
        assert_eq!(d.record(id_a, c1), Outcome::New);
        assert_eq!(d.distinct_note_ids(), 1);

        // Same (id_a, c1) again — LegitimateDuplicate (not a twin).
        assert_eq!(d.record(id_a, c1), Outcome::LegitimateDuplicate);
        assert_eq!(d.distinct_note_ids(), 1);

        // Different NoteId — independent New, no false positive.
        assert_eq!(d.record(id_b, c1), Outcome::New);

        // Same NoteId (id_a) + DIFFERENT commitment (c2) — TwinDetected.
        match d.record(id_a, c2) {
            Outcome::TwinDetected { prior_commitments } => {
                assert_eq!(prior_commitments, vec![c1]);
            }
            other => panic!("expected TwinDetected, got {other:?}"),
        }
        assert_eq!(d.distinct_note_ids(), 2);

        // Re-observing the twin commitment c2 is now a LegitimateDuplicate
        // (we've seen it once already). The alert fires only on first sight.
        assert_eq!(d.record(id_a, c2), Outcome::LegitimateDuplicate);

        // A THIRD distinct commitment for id_a fires another TwinDetected
        // and reports BOTH prior commitments. Useful for attribution when
        // the attacker repeatedly clones.
        let c3 = [0x33u8; 32];
        match d.record(id_a, c3) {
            Outcome::TwinDetected { prior_commitments } => {
                assert!(prior_commitments.contains(&c1));
                assert!(prior_commitments.contains(&c2));
                assert_eq!(prior_commitments.len(), 2);
            }
            other => panic!("expected TwinDetected with 2 priors, got {other:?}"),
        }
    }

    /// The detector must not panic on edge inputs.
    #[test]
    fn cantina_6_zero_inputs_not_special() {
        let d = TwinNoteDetector::new();
        assert_eq!(d.record([0u8; 32], [0u8; 32]), Outcome::New);
        // Same zero NoteId with different commitment IS a twin.
        match d.record([0u8; 32], [1u8; 32]) {
            Outcome::TwinDetected { .. } => {}
            other => panic!("expected TwinDetected, got {other:?}"),
        }
    }

    /// Concurrent observations of the same `(NoteId, commitment)` pair must
    /// produce exactly one `New` regardless of how many threads race.
    #[test]
    fn cantina_6_detector_serialises_concurrent_inserts() {
        use std::sync::Arc;
        use std::thread;

        let d = Arc::new(TwinNoteDetector::new());
        let id = [0x99u8; 32];
        let commitment = [0x77u8; 32];

        let handles: Vec<_> = (0..16)
            .map(|_| {
                let d = d.clone();
                thread::spawn(move || d.record(id, commitment))
            })
            .collect();
        let outcomes: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();

        let new_count = outcomes
            .iter()
            .filter(|o| matches!(o, Outcome::New))
            .count();
        assert_eq!(new_count, 1, "exactly one thread must record New");
        assert_eq!(d.distinct_note_ids(), 1);
    }
}
