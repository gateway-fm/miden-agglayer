//! Transaction receipt state and submission evidence are independent.
//!
//! In particular, a failed receipt can retain a prepared handoff from an older
//! release. Expiration reconciliation may repair that pair back to a pending
//! intent. These classifications describe a snapshot; store-level fences and
//! compare-and-set operations still authorize every durable transition.

use crate::store::{NoteHandoff, NoteHandoffState, TxnData};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReceiptState {
    Missing,
    Pending,
    Succeeded,
    Failed,
}

impl ReceiptState {
    #[cfg(any(test, feature = "postgres"))]
    pub(crate) fn from_persisted(status: &str) -> anyhow::Result<Self> {
        match status {
            "pending" => Ok(Self::Pending),
            "success" => Ok(Self::Succeeded),
            "failed" => Ok(Self::Failed),
            other => anyhow::bail!("unknown persisted transaction receipt state: {other}"),
        }
    }

    pub(crate) fn is_terminal(self) -> bool {
        matches!(self, Self::Succeeded | Self::Failed)
    }

    /// Ordinary receipt finalization: real landing may heal failure, success is
    /// absorbing, and an ambiguous handoff forbids publishing a failure. Store
    /// implementations evaluate this against their locked authoritative row;
    /// confirmed duplicates and expired preparation use dedicated transitions.
    pub(crate) fn accepts_result(self, success: bool, has_handoff: bool) -> bool {
        match self {
            Self::Missing | Self::Succeeded => false,
            Self::Pending => success || !has_handoff,
            Self::Failed => success,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SubmissionBoundary {
    /// Durable intent, with no evidence of external submission.
    Unlinked,
    /// An exact note was prepared; inclusion is still ambiguous.
    Prepared,
    /// Legacy records can carry both signals. The Miden transaction id forbids
    /// a re-drive, while the exact prepared link remains eligible for inspection.
    PreparedWithTransactionId,
    /// The exact note was confirmed by inclusion or later observation.
    Submitted,
    /// Legacy submission evidence without an exact note link.
    TransactionRecorded,
}

impl SubmissionBoundary {
    pub(crate) fn from_evidence(handoff: Option<NoteHandoffState>, has_miden_id: bool) -> Self {
        match (handoff, has_miden_id) {
            (Some(NoteHandoffState::Submitted), _) => Self::Submitted,
            (Some(NoteHandoffState::Prepared), true) => Self::PreparedWithTransactionId,
            (Some(NoteHandoffState::Prepared), false) => Self::Prepared,
            (None, true) => Self::TransactionRecorded,
            (None, false) => Self::Unlinked,
        }
    }

    pub(crate) fn has_prepared_note(self) -> bool {
        matches!(self, Self::Prepared | Self::PreparedWithTransactionId)
    }

    pub(crate) fn has_note_handoff(self) -> bool {
        matches!(
            self,
            Self::Prepared | Self::PreparedWithTransactionId | Self::Submitted
        )
    }

    pub(crate) fn is_confirmed(self) -> bool {
        matches!(
            self,
            Self::Submitted | Self::TransactionRecorded | Self::PreparedWithTransactionId
        )
    }
}

/// Evidence about whose note caused the bridge effect, not merely whether the
/// claim nullifier/GER exists. Only the exact note can justify its own success.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ExactNoteOutcome {
    NotApplied,
    AppliedByExactNote,
    AppliedElsewhere,
    Uncertain,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RecoveryDecision {
    NoPendingReceipt,
    AwaitProjection,
    ConfirmDuplicate,
    AwaitConfirmation,
    CheckPreparedExpiry,
    Redrive,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TransactionLifecycle {
    pub(crate) receipt: ReceiptState,
    pub(crate) submission: SubmissionBoundary,
}

impl TransactionLifecycle {
    pub(crate) fn from_snapshot(tx: Option<&TxnData>, handoff: Option<&NoteHandoff>) -> Self {
        let receipt = match tx.map(|tx| &tx.result) {
            None => ReceiptState::Missing,
            Some(None) => ReceiptState::Pending,
            Some(Some(Ok(()))) => ReceiptState::Succeeded,
            Some(Some(Err(_))) => ReceiptState::Failed,
        };
        Self {
            receipt,
            submission: SubmissionBoundary::from_evidence(
                handoff.map(|handoff| handoff.state),
                tx.is_some_and(|tx| tx.id.is_some()),
            ),
        }
    }

    pub(crate) fn is_durable_intent(self) -> bool {
        self.receipt == ReceiptState::Pending && self.submission == SubmissionBoundary::Unlinked
    }

    /// Under the signer lock, any terminal receipt or submission evidence
    /// fences off a second note, even if another task has removed its cache row.
    pub(crate) fn admission_is_known(self) -> bool {
        self.receipt.is_terminal() || self.submission != SubmissionBoundary::Unlinked
    }

    /// Call after validating a claim proof when the fresh effect is NotApplied.
    /// That deterministic rejection intentionally precedes ordinary submission
    /// waiting. Exact-note success remains projector-owned for event atomicity.
    pub(crate) fn recovery_decision(self, effect: ExactNoteOutcome) -> RecoveryDecision {
        if self.receipt != ReceiptState::Pending {
            return RecoveryDecision::NoPendingReceipt;
        }
        match effect {
            ExactNoteOutcome::AppliedByExactNote => RecoveryDecision::AwaitProjection,
            ExactNoteOutcome::AppliedElsewhere => RecoveryDecision::ConfirmDuplicate,
            ExactNoteOutcome::NotApplied | ExactNoteOutcome::Uncertain => {
                if self.submission.is_confirmed() {
                    RecoveryDecision::AwaitConfirmation
                } else if self.submission.has_prepared_note() {
                    RecoveryDecision::CheckPreparedExpiry
                } else {
                    RecoveryDecision::Redrive
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn persisted_receipt_states_are_closed() {
        for (value, state) in [
            ("pending", ReceiptState::Pending),
            ("success", ReceiptState::Succeeded),
            ("failed", ReceiptState::Failed),
        ] {
            assert_eq!(ReceiptState::from_persisted(value).unwrap(), state);
        }
        for unknown in ["", "missing", "submitted", "corrupt", "SUCCESS"] {
            assert!(ReceiptState::from_persisted(unknown).is_err());
        }
    }

    #[test]
    fn receipt_transition_table_preserves_success_and_handoff_fences() {
        use ReceiptState::*;
        // Expected: failure without/with handoff; success without/with handoff.
        for (state, expected) in [
            (Missing, [false, false, false, false]),
            (Pending, [true, false, true, true]),
            (Succeeded, [false, false, false, false]),
            (Failed, [false, false, true, true]),
        ] {
            for ((success, has_handoff), accepted) in
                [(false, false), (false, true), (true, false), (true, true)]
                    .into_iter()
                    .zip(expected)
            {
                assert_eq!(
                    state.accepts_result(success, has_handoff),
                    accepted,
                    "{state:?}: success={success}, handoff={has_handoff}"
                );
            }
        }
    }

    #[test]
    fn receipt_and_submission_product_preserves_every_recovery_case() {
        use ExactNoteOutcome::*;
        use ReceiptState::*;
        use RecoveryDecision::*;
        use SubmissionBoundary::*;
        // Each row encodes the policy in effect order: absent effect, exact
        // note consumed, another transaction applied, incomplete observation.
        let boundaries = [
            (
                Unlinked,
                [Redrive, AwaitProjection, ConfirmDuplicate, Redrive],
            ),
            (
                Prepared,
                [
                    CheckPreparedExpiry,
                    AwaitProjection,
                    ConfirmDuplicate,
                    CheckPreparedExpiry,
                ],
            ),
            (
                PreparedWithTransactionId,
                [
                    AwaitConfirmation,
                    AwaitProjection,
                    ConfirmDuplicate,
                    AwaitConfirmation,
                ],
            ),
            (
                Submitted,
                [
                    AwaitConfirmation,
                    AwaitProjection,
                    ConfirmDuplicate,
                    AwaitConfirmation,
                ],
            ),
            (
                TransactionRecorded,
                [
                    AwaitConfirmation,
                    AwaitProjection,
                    ConfirmDuplicate,
                    AwaitConfirmation,
                ],
            ),
        ];
        for receipt in [Missing, Pending, Succeeded, Failed] {
            for (submission, pending_decisions) in boundaries {
                let lifecycle = TransactionLifecycle {
                    receipt,
                    submission,
                };
                assert_eq!(
                    lifecycle.is_durable_intent(),
                    receipt == Pending && submission == Unlinked
                );
                assert_eq!(
                    lifecycle.admission_is_known(),
                    matches!(receipt, Succeeded | Failed) || submission != Unlinked
                );
                for (effect, pending_decision) in
                    [NotApplied, AppliedByExactNote, AppliedElsewhere, Uncertain]
                        .into_iter()
                        .zip(pending_decisions)
                {
                    assert_eq!(
                        lifecycle.recovery_decision(effect),
                        if receipt == Pending {
                            pending_decision
                        } else {
                            NoPendingReceipt
                        },
                        "{lifecycle:?} / {effect:?}"
                    );
                }
            }
        }
        // Failed does not erase the prepared identity: admission can still ask
        // the store to repair expired preparation before terminal deduplication.
        let repaired_candidate = TransactionLifecycle {
            receipt: Failed,
            submission: Prepared,
        };
        assert!(repaired_candidate.submission.has_prepared_note());
        assert!(!repaired_candidate.is_durable_intent());
    }

    #[test]
    fn every_submission_evidence_combination_is_preserved() {
        use SubmissionBoundary::*;
        for (handoff, has_id, expected) in [
            (None, false, Unlinked),
            (None, true, TransactionRecorded),
            (Some(NoteHandoffState::Prepared), false, Prepared),
            (
                Some(NoteHandoffState::Prepared),
                true,
                PreparedWithTransactionId,
            ),
            (Some(NoteHandoffState::Submitted), false, Submitted),
            (Some(NoteHandoffState::Submitted), true, Submitted),
        ] {
            assert_eq!(SubmissionBoundary::from_evidence(handoff, has_id), expected);
            assert_eq!(expected.has_note_handoff(), handoff.is_some());
        }
    }
}
