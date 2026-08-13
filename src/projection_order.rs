//! The `ORDER` stage of `docs/design/UNIFIED-PROJECTOR.md`, as CODE.
//!
//! Every synthetic event — live tick and `--restore` replay alike — is emitted
//! in the order defined by [`ProjectionOrder::key`]. There is deliberately **no
//! second implementation** of this comparator anywhere in the codebase.
//!
//! # Why one function, enforced
//!
//! The live projector and the restore replay each carried their own copy of
//! "the same" comparator for months, and the copies drifted THREE times, each
//! time producing consumer-visible history divergence measured by the
//! full-DB-loss drill's `eth_getLogs` diff:
//!
//!   * kind-major vs block-major replay (finding #100): renumbered every
//!     `log_index` after a restore;
//!   * a consumption-tx-order tiebreak (bbece50, reverted): flipped a same-block
//!     GER pair and re-chained 72 lines of history;
//!   * a creation-order tiebreak (d724baa, deleted by this module): flipped a
//!     same-block GER pair at block 2750 and re-chained 28 lines.
//!
//! An `UpdateHashChain` log carries the rolling `hash_chain_value` in its
//! topics, so ONE misordered same-block pair re-chains every subsequent GER
//! event — divergence here is never cosmetic. The only stable posture is a
//! single shared key, constructed from what the live client store actually
//! records for each note, and property-tested for live/replay equivalence.
//!
//! # The key
//!
//! `(block, consumed_tx_order, within_tx_pos, details_commitment, note_id)`
//!
//! * `consumed_tx_order` — the per-block transaction order **as the live client
//!   store records it**: `Some(order)` for B2AGG notes (the reconciler resolves
//!   and durably records their consuming transaction), `None` for every other
//!   kind (the store keeps NULL for notes consumed externally — measured
//!   135/135 on a live stack). A replay source MUST mirror the live record, not
//!   "improve" on it: replaying claims at their true `Some(order)` while live
//!   emitted them from `None`-order records is exactly how history diverges.
//! * `within_tx_pos` — the input position inside the consuming transaction,
//!   known live only for B2AGG (from `resolve_b2agg_consumptions`); `0` for
//!   everything else.
//! * `details_commitment`, then `note_id` — the stable identity tiebreaks, in
//!   this order, matching what the live projector has always applied.

use miden_client::store::InputNoteRecord;
use miden_protocol::note::NoteId;

/// The canonical ordering key. Tuple form so it works directly with
/// `sort_by_key` / tuple comparison everywhere.
pub type ProjectionOrderKey = (u64, Option<u32>, u32, [u8; 32], Option<[u8; 32]>);

/// One projected note's position in the canonical emission order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProjectionOrder {
    pub block: u64,
    /// `Some` iff the live client store records a consuming-tx order for this
    /// note kind (B2AGG only). See the module docs before "fixing" a `None`.
    pub consumed_tx_order: Option<u32>,
    /// B2AGG input position within its consuming transaction; `0` otherwise.
    pub within_tx_pos: u32,
    pub details_commitment: [u8; 32],
    pub note_id: Option<[u8; 32]>,
}

impl ProjectionOrder {
    /// The canonical key. This is the ONLY comparator for synthetic-event
    /// emission order; do not sort projection inputs by anything else.
    pub fn key(&self) -> ProjectionOrderKey {
        (
            self.block,
            self.consumed_tx_order,
            self.within_tx_pos,
            self.details_commitment,
            self.note_id,
        )
    }

    /// Order for a client-store record — the live projector's per-block input
    /// shape, and the replay's `Consumed` source. `within_tx_pos` is looked up
    /// by NoteId (populated only for B2AGG), mirroring
    /// `SyntheticProjector::project_block_notes`.
    pub fn for_record(
        block: u64,
        note_id: Option<NoteId>,
        note: &InputNoteRecord,
        within_tx_pos: &std::collections::HashMap<NoteId, u32>,
    ) -> Self {
        Self {
            block,
            consumed_tx_order: note.state().consumed_tx_order(),
            within_tx_pos: note_id
                .and_then(|id| within_tx_pos.get(&id))
                .copied()
                .unwrap_or(0),
            details_commitment: note.details_commitment().as_bytes(),
            note_id: note_id.map(|id| id.as_bytes()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn order(
        block: u64,
        tx: Option<u32>,
        pos: u32,
        commitment: u8,
        id: Option<u8>,
    ) -> ProjectionOrder {
        ProjectionOrder {
            block,
            consumed_tx_order: tx,
            within_tx_pos: pos,
            details_commitment: [commitment; 32],
            note_id: id.map(|b| [b; 32]),
        }
    }

    /// The full precedence chain, pinned: block ≫ tx_order (None first) ≫
    /// within_tx_pos ≫ commitment ≫ id.
    #[test]
    fn key_precedence_is_block_tx_pos_commitment_id() {
        let ordered = [
            order(1, Some(9), 9, 0xFF, Some(0xFF)), // earlier block beats everything
            order(2, None, 0, 0xFF, Some(0xFF)),    // None tx_order before Some
            order(2, Some(0), 9, 0xFF, Some(0xFF)), // lower tx_order
            order(2, Some(1), 0, 0xFF, Some(0xFF)), // lower position
            order(2, Some(1), 1, 0x00, Some(0xFF)), // lower commitment
            order(2, Some(1), 1, 0x01, Some(0x00)), // id breaks the final tie
            order(2, Some(1), 1, 0x01, Some(0x01)),
        ];
        for pair in ordered.windows(2) {
            assert!(
                pair[0].key() < pair[1].key(),
                "precedence violated: {:?} !< {:?}",
                pair[0],
                pair[1]
            );
        }
    }

    /// The #102-class regression, pinned at the comparator level: a same-block
    /// pair with identical (tx_order, pos) MUST tie-break on commitment — no
    /// creation order, no consumption order, nothing else. Both prior drifts
    /// (bbece50, d724baa) would fail this test.
    #[test]
    fn same_block_pair_ties_on_commitment_only() {
        let a = order(50, None, 0, 0x01, Some(0xEE));
        let b = order(50, None, 0, 0x02, Some(0x11));
        // commitment decides — the LOWER id on `b` must not matter.
        assert!(a.key() < b.key());
    }
}
