//! Authoritative bridge-state reads used by the EVM compatibility surface.
//!
//! Submission locks (`claimed_indices`) deliberately do not participate here:
//! they include in-flight work. An operation is "applied" only when either the
//! synthetic landed projection exists or the synchronized Miden bridge account
//! contains the corresponding GER/nullifier entry.

use crate::miden_client::MidenClientLib;
use crate::service_state::ServiceState;
use crate::store::Store;
use alloy::primitives::U256;
use anyhow::Context;
use miden_base_agglayer::{AggLayerBridge, ExitRoot};
use miden_protocol::account::AccountId;
use miden_protocol::crypto::hash::poseidon2::Poseidon2;
use miden_protocol::note::NoteId;
use miden_protocol::{Felt, Word};
#[cfg(not(test))]
use std::sync::{Arc, Mutex};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ExactNoteOutcome {
    NotApplied,
    AppliedByExactNote,
    AppliedElsewhere,
    Uncertain,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NoteObservation {
    NotRequested,
    Missing,
    Unconsumed,
    Consumed,
}

#[derive(Debug, Clone, Copy)]
struct BridgeSnapshot {
    ger_applied: Option<bool>,
    claim_applied: Option<bool>,
    note: NoteObservation,
}

/// Decode the Solidity global-index layout into the nullifier coordinates
/// stored by the Miden bridge: `(leaf_index, source_bridge_network)`.
pub(crate) fn claim_coordinates(global_index: U256) -> anyhow::Result<(u32, u32)> {
    let bytes = global_index.to_be_bytes::<32>();
    if bytes[..20].iter().any(|byte| *byte != 0) {
        anyhow::bail!("globalIndex has non-zero leading 160 bits");
    }
    let mainnet_flag = u32::from_be_bytes(bytes[20..24].try_into().expect("four bytes"));
    let rollup_index = u32::from_be_bytes(bytes[24..28].try_into().expect("four bytes"));
    let leaf_index = u32::from_be_bytes(bytes[28..32].try_into().expect("four bytes"));
    let source = match (mainnet_flag, rollup_index) {
        (1, 0) => 0,
        (0, rollup) => rollup
            .checked_add(1)
            .context("rollup globalIndex cannot map to a uint32 source network")?,
        (1, _) => anyhow::bail!("mainnet globalIndex has a non-zero rollup index"),
        _ => anyhow::bail!("globalIndex mainnet flag must be 0 or 1"),
    };
    Ok((leaf_index, source))
}

/// Build the Solidity global index used by `claimAsset` from the arguments to
/// `isClaimed(uint32,uint32)`.
pub(crate) fn global_index_for_claim(leaf_index: u32, source: u32) -> U256 {
    if source == 0 {
        (U256::from(1u8) << 64) | U256::from(leaf_index)
    } else {
        (U256::from(source - 1) << 32) | U256::from(leaf_index)
    }
}

fn claim_is_set(
    storage: &miden_protocol::account::AccountStorage,
    global_index: U256,
) -> anyhow::Result<bool> {
    let (leaf_index, source) = claim_coordinates(global_index)?;
    let nullifier_elements = [
        Felt::new(u64::from(leaf_index)).context("encoding claim leaf index as Felt")?,
        Felt::new(u64::from(source)).context("encoding claim source network as Felt")?,
    ];
    let nullifier = Poseidon2::hash_elements(&nullifier_elements);
    let value = storage
        .get_map_item(AggLayerBridge::claim_nullifiers_slot_name(), nullifier)
        .context("reading bridge claim-nullifier map")?;
    Ok(value == Word::from([1u32, 0, 0, 0]))
}

async fn bridge_snapshot_with_client(
    client: &mut MidenClientLib,
    bridge_id: AccountId,
    ger: Option<[u8; 32]>,
    claim: Option<U256>,
    note_id: Option<String>,
) -> anyhow::Result<BridgeSnapshot> {
    let bridge = client
        .get_account(bridge_id)
        .await
        .context("reading Miden bridge account")?
        .context("Miden bridge account is not available locally after sync")?;

    let ger_applied = ger
        .map(|root| {
            AggLayerBridge::is_ger_registered(ExitRoot::new(root), &bridge)
                .context("reading bridge GER map")
        })
        .transpose()?;
    let claim_applied = claim
        .map(|gi| claim_is_set(bridge.storage(), gi))
        .transpose()?;
    let note = match note_id {
        None => NoteObservation::NotRequested,
        Some(note_id) => {
            let note_id = NoteId::try_from_hex(&note_id).context("parsing exact handoff NoteId")?;
            match client
                .get_output_note(note_id)
                .await
                .context("reading exact handoff output note")?
            {
                None => NoteObservation::Missing,
                Some(note) if note.is_consumed() => NoteObservation::Consumed,
                Some(_) => NoteObservation::Unconsumed,
            }
        }
    };
    Ok(BridgeSnapshot {
        ger_applied,
        claim_applied,
        note,
    })
}

#[cfg(test)]
async fn bridge_snapshot(
    _service: &ServiceState,
    ger: Option<[u8; 32]>,
    claim: Option<U256>,
    note_id: Option<String>,
) -> anyhow::Result<BridgeSnapshot> {
    Ok(BridgeSnapshot {
        ger_applied: ger.map(|_| false),
        claim_applied: claim.map(|_| false),
        note: if note_id.is_some() {
            NoteObservation::Missing
        } else {
            NoteObservation::NotRequested
        },
    })
}

#[cfg(not(test))]
async fn bridge_snapshot(
    service: &ServiceState,
    ger: Option<[u8; 32]>,
    claim: Option<U256>,
    note_id: Option<String>,
) -> anyhow::Result<BridgeSnapshot> {
    let result: Arc<Mutex<Option<BridgeSnapshot>>> = Arc::new(Mutex::new(None));
    let result_in = result.clone();
    let bridge_id = service.accounts.0.bridge.0;

    service
        .miden_client
        .with(move |client| {
            Box::new(async move {
                let snapshot =
                    bridge_snapshot_with_client(client, bridge_id, ger, claim, note_id).await?;
                *result_in.lock().expect("bridge snapshot mutex poisoned") = Some(snapshot);
                Ok(())
            })
        })
        .await?;

    result
        .lock()
        .expect("bridge snapshot mutex poisoned")
        .take()
        .context("Miden bridge-state request completed without a snapshot")
}

pub(crate) async fn ger_applied(service: &ServiceState, ger: &[u8; 32]) -> anyhow::Result<bool> {
    if service.store.is_ger_injected(ger).await? {
        return Ok(true);
    }
    bridge_snapshot(service, Some(*ger), None, None)
        .await?
        .ger_applied
        .context("GER state was not requested")
}

pub(crate) async fn claim_applied(
    service: &ServiceState,
    global_index: U256,
) -> anyhow::Result<bool> {
    if service
        .store
        .has_claim_event_for_global_index(&global_index.to_be_bytes::<32>())
        .await?
    {
        return Ok(true);
    }
    bridge_snapshot(service, None, Some(global_index), None)
        .await?
        .claim_applied
        .context("claim state was not requested")
}

/// Read claim and GER state with at most one serialized Miden-client request.
pub(crate) async fn claim_and_ger_applied(
    service: &ServiceState,
    global_index: U256,
    ger: &[u8; 32],
) -> anyhow::Result<(bool, bool)> {
    let projected_claim = service
        .store
        .has_claim_event_for_global_index(&global_index.to_be_bytes::<32>())
        .await?;
    let projected_ger = service.store.is_ger_injected(ger).await?;
    if projected_claim {
        return Ok((true, projected_ger));
    }
    let snapshot = bridge_snapshot(
        service,
        (!projected_ger).then_some(*ger),
        (!projected_claim).then_some(global_index),
        None,
    )
    .await?;
    Ok((
        projected_claim || snapshot.claim_applied.unwrap_or(false),
        projected_ger || snapshot.ger_applied.unwrap_or(false),
    ))
}

fn classify_exact_note(applied: bool, note: NoteObservation) -> ExactNoteOutcome {
    if !applied {
        return ExactNoteOutcome::NotApplied;
    }
    match note {
        NoteObservation::Consumed => ExactNoteOutcome::AppliedByExactNote,
        NoteObservation::Unconsumed => ExactNoteOutcome::AppliedElsewhere,
        NoteObservation::Missing | NoteObservation::NotRequested => ExactNoteOutcome::Uncertain,
    }
}

pub(crate) async fn reconcile_ger_handoff_with_client(
    store: &dyn Store,
    client: &mut MidenClientLib,
    bridge_id: AccountId,
    ger: [u8; 32],
    note_id: String,
) -> anyhow::Result<ExactNoteOutcome> {
    if store.is_ger_injected(&ger).await? {
        // GER event + a still-pending receipt can only be another transaction:
        // normal projection persists the event and its linked receipt atomically.
        return Ok(ExactNoteOutcome::AppliedElsewhere);
    }
    let snapshot =
        bridge_snapshot_with_client(client, bridge_id, Some(ger), None, Some(note_id)).await?;
    Ok(classify_exact_note(
        snapshot.ger_applied.unwrap_or(false),
        snapshot.note,
    ))
}

pub(crate) async fn reconcile_claim_handoff_with_client(
    store: &dyn Store,
    client: &mut MidenClientLib,
    bridge_id: AccountId,
    global_index: U256,
    note_id: String,
) -> anyhow::Result<ExactNoteOutcome> {
    if store
        .has_claim_event_for_global_index(&global_index.to_be_bytes::<32>())
        .await?
    {
        // ClaimEvent + a still-pending receipt can only be another transaction:
        // normal projection persists the event and its linked receipt atomically.
        return Ok(ExactNoteOutcome::AppliedElsewhere);
    }
    let snapshot =
        bridge_snapshot_with_client(client, bridge_id, None, Some(global_index), Some(note_id))
            .await?;
    Ok(classify_exact_note(
        snapshot.claim_applied.unwrap_or(false),
        snapshot.note,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_claimed_arguments_round_trip_to_global_index() {
        let mainnet = global_index_for_claim(42, 0);
        assert_eq!(claim_coordinates(mainnet).unwrap(), (42, 0));

        let rollup = global_index_for_claim(99, 7);
        assert_eq!(claim_coordinates(rollup).unwrap(), (99, 7));
        assert_eq!(rollup, (U256::from(6u8) << 32) | U256::from(99u8));
    }

    #[test]
    fn claim_nullifier_reads_leaf_and_source_qualified_key() {
        use miden_protocol::account::{AccountStorage, StorageMap, StorageMapKey, StorageSlot};

        let leaf = 42u32;
        let source = 7u32;
        let key = Poseidon2::hash_elements(&[
            Felt::new(u64::from(leaf)).unwrap(),
            Felt::new(u64::from(source)).unwrap(),
        ]);
        let mut map = StorageMap::new();
        map.insert(StorageMapKey::new(key), Word::from([1u32, 0, 0, 0]))
            .unwrap();
        let storage = AccountStorage::new(vec![StorageSlot::with_map(
            AggLayerBridge::claim_nullifiers_slot_name().clone(),
            map,
        )])
        .unwrap();

        assert!(claim_is_set(&storage, global_index_for_claim(leaf, source)).unwrap());
        assert!(!claim_is_set(&storage, global_index_for_claim(leaf, source + 1)).unwrap());
    }

    #[test]
    fn exact_note_confirmation_never_guesses() {
        assert_eq!(
            classify_exact_note(true, NoteObservation::Unconsumed),
            ExactNoteOutcome::AppliedElsewhere
        );
        assert_eq!(
            classify_exact_note(true, NoteObservation::Consumed),
            ExactNoteOutcome::AppliedByExactNote
        );
        assert_eq!(
            classify_exact_note(true, NoteObservation::Missing),
            ExactNoteOutcome::Uncertain
        );
        assert_eq!(
            classify_exact_note(false, NoteObservation::Unconsumed),
            ExactNoteOutcome::NotApplied
        );
    }
}
