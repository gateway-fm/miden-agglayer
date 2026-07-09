//! Cantina MA#18 — recovery for quarantined ("unbridgeable") B2AGG bridge-outs.
//!
//! # The finding
//!
//! A `B2AGG` bridge-out submitted as an **erased note** (the note is created AND
//! consumed in the same transaction) has its nullifier stripped at block
//! construction (`remove_erased_nullifiers`), so it never appears in
//! `BlockBody.created_nullifiers`. The live indexer surfaces consumed notes via
//! nullifiers (`NoteFilter::Consumed`), so it never observes the erased
//! bridge-out — no synthetic `BridgeEvent` is written, the off-chain side is
//! never enacted, and the user's funds are stranded (burned on the source,
//! never minted on the destination). Meanwhile the on-chain bridge LET frontier
//! DID advance, so the Cantina #9 LET-divergence monitor sees
//! `let_num_leaves > deposit_count`.
//!
//! # What is recoverable proxy-side (honest scope)
//!
//! The on-chain bridge account stores only the LET **frontier** (O(log n)
//! append-boundary subtree roots), the LET **root** (a hash), and the leaf
//! **count** — it never stores leaf *preimages*. So the `(destinationNetwork,
//! destinationAddress, amount, originToken, metadata)` needed to rebuild a
//! `BridgeEvent` **cannot** be reconstructed from on-chain state via FPI; only
//! the count gap is observable. A *truly* erased note whose preimage the proxy
//! never captured is therefore **not** recoverable here — closing that gap
//! needs either the note preimage (from the depositor/sequencer, off-chain) or
//! a protocol-level fix that stops erasing bridge-out nullifiers.
//!
//! What this module CAN recover is the class of MA#18 skips whose preimage WAS
//! captured into the `unbridgeable_bridge_outs` quarantine table (migration
//! 006) — e.g. an `unknown_faucet` note whose faucet is now registered, or an
//! `atomic_commit_failed` transient. For each such row we re-derive the
//! BridgeEvent fields from the captured `note_dump` and emit it via the SAME
//! two store primitives a normal bridge-out takes (`mark_note_processed` +
//! `add_bridge_event`, with rollback on failure — see
//! `restore::project_b2agg_note`), then delete the quarantine row. This
//! implements the recovery that migration 006 previously declared
//! "NOT IMPLEMENTED YET", and closes the `deposit_count` gap so the LET
//! divergence clears and the funds become claimable.

use std::sync::Arc;

use miden_protocol::account::AccountId;

use crate::block_state::BlockState;
use crate::store::{Store, UnbridgeableBridgeOut};

/// Outcome of attempting to recover a single quarantined bridge-out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecoveryOutcome {
    /// A synthetic `BridgeEvent` was emitted and the quarantine row deleted.
    Recovered {
        note_id: String,
        deposit_count: u32,
        destination_network: u32,
    },
    /// A prior run already emitted the BridgeEvent for this note (it is marked
    /// processed); we simply cleared the now-stale quarantine row.
    StaleCleared { note_id: String },
    /// The row could not be recovered yet. `reason` is a stable, machine-usable
    /// tag for the blocker (faucet still unknown, dump not reconstructable,
    /// self-targeted poison leaf, …). The row is LEFT in place for a later
    /// re-attempt / operator action.
    StillBlocked {
        note_id: String,
        reason: &'static str,
    },
}

/// Aggregate result of a recovery sweep over the whole quarantine table.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RecoverySummary {
    pub attempted: usize,
    pub recovered: usize,
    pub stale_cleared: usize,
    pub still_blocked: usize,
}

/// Parsed forensic fields extracted from a quarantine `note_dump`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedQuarantineDump {
    /// Raw B2AGG storage felts (canonical u64 limb values), in note order.
    pub storage_felts: Vec<u64>,
    /// Fungible assets as `(faucet_id, miden_amount)` pairs.
    pub fungible_assets: Vec<(AccountId, u64)>,
}

/// Extract the single bracketed list following `"<key>":[` in the dump, or
/// `None` if the key is absent / malformed. Returns the raw inner text.
fn extract_bracket_list<'a>(dump: &'a str, key: &str) -> Option<&'a str> {
    let needle = format!("\"{key}\":[");
    let start = dump.find(&needle)? + needle.len();
    let rest = &dump[start..];
    let end = rest.find(']')?;
    Some(&rest[..end])
}

/// Parse a `dump_note_for_quarantine` string back into structured recovery
/// fields. Pure and total (never panics); returns `Err` if the dump is not in
/// the expected shape. Mirrors the writer in
/// [`crate::bridge_out::dump_note_for_quarantine`].
pub fn parse_quarantine_dump(dump: &str) -> anyhow::Result<ParsedQuarantineDump> {
    let storage_inner = extract_bracket_list(dump, "storage_items")
        .ok_or_else(|| anyhow::anyhow!("note_dump missing storage_items list"))?;
    let mut storage_felts = Vec::new();
    for tok in storage_inner.split(',') {
        let tok = tok.trim();
        if tok.is_empty() {
            continue;
        }
        let v: u64 = tok
            .parse()
            .map_err(|e| anyhow::anyhow!("storage_items entry {tok:?} not a u64: {e}"))?;
        storage_felts.push(v);
    }

    // fungible_assets is a list of `{faucet=<hex>, amount=<u64>}` records. Walk
    // each `faucet=` / `amount=` pair. The faucet id is canonical hex thanks to
    // the `to_hex()` writer, so it round-trips via `AccountId::from_hex`.
    let assets_inner = extract_bracket_list(dump, "fungible_assets")
        .ok_or_else(|| anyhow::anyhow!("note_dump missing fungible_assets list"))?;
    let mut fungible_assets = Vec::new();
    let mut cursor = assets_inner;
    while let Some(fpos) = cursor.find("faucet=") {
        let after_faucet = &cursor[fpos + "faucet=".len()..];
        let faucet_end = after_faucet
            .find([',', '}'])
            .ok_or_else(|| anyhow::anyhow!("fungible_assets faucet field unterminated"))?;
        let faucet_hex = after_faucet[..faucet_end].trim();
        let faucet_id = AccountId::from_hex(faucet_hex)
            .map_err(|e| anyhow::anyhow!("fungible_assets faucet {faucet_hex:?} invalid: {e}"))?;

        let apos = after_faucet
            .find("amount=")
            .ok_or_else(|| anyhow::anyhow!("fungible_assets record missing amount"))?;
        let after_amount = &after_faucet[apos + "amount=".len()..];
        let amount_end = after_amount.find([',', '}']).unwrap_or(after_amount.len());
        let amount_str = after_amount[..amount_end].trim();
        let amount: u64 = amount_str
            .parse()
            .map_err(|e| anyhow::anyhow!("fungible_assets amount {amount_str:?} not a u64: {e}"))?;

        fungible_assets.push((faucet_id, amount));
        cursor = &after_amount[amount_end..];
    }

    Ok(ParsedQuarantineDump {
        storage_felts,
        fungible_assets,
    })
}

/// Re-derive `(destination_network, destination_address)` from raw B2AGG
/// storage felts — the same decoding as
/// [`crate::bridge_out::parse_b2agg_storage`], but operating on the plain u64
/// limb values captured in the quarantine dump (which does not preserve the
/// original `NoteStorage`). Pure; kept in lockstep with the canonical parser.
pub fn derive_destination_from_felts(items: &[u64]) -> anyhow::Result<(u32, [u8; 20])> {
    if items.len() < 6 {
        anyhow::bail!(
            "B2AGG storage too short to recover: expected ≥6 felts, got {}",
            items.len()
        );
    }
    let raw_network = u32::try_from(items[0])
        .map_err(|_| anyhow::anyhow!("destination_network limb exceeds u32::MAX"))?;
    let destination_network = u32::from_le_bytes(raw_network.to_be_bytes());

    let mut address = [0u8; 20];
    for i in 0..5 {
        let limb = u32::try_from(items[1 + i])
            .map_err(|_| anyhow::anyhow!("address limb {i} exceeds u32::MAX"))?;
        address[i * 4..(i + 1) * 4].copy_from_slice(&limb.to_le_bytes());
    }
    Ok((destination_network, address))
}

/// Attempt to recover a single quarantined bridge-out from its captured
/// `note_dump`. On success emits the synthetic `BridgeEvent` (via the same
/// `mark_note_processed` + `add_bridge_event` commit a normal bridge-out uses)
/// and deletes the quarantine row. On a still-present blocker, LEAVES the row
/// in place and returns [`RecoveryOutcome::StillBlocked`].
pub async fn recover_unbridgeable_bridge_out(
    store: &Arc<dyn Store>,
    block_state: &BlockState,
    entry: &UnbridgeableBridgeOut,
    bridge_address: &str,
    local_network_id: u32,
) -> anyhow::Result<RecoveryOutcome> {
    let note_id = entry.note_id.clone();

    // Idempotency: a prior recovery / live re-projection may already have
    // emitted this note's BridgeEvent. Don't double-emit — just clear the
    // now-stale quarantine row.
    if store.is_note_processed(&note_id).await? {
        store.delete_unbridgeable_bridge_out(&note_id).await?;
        return Ok(RecoveryOutcome::StaleCleared { note_id });
    }

    let parsed = match parse_quarantine_dump(&entry.note_dump) {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(
                target: "bridge_out::recovery",
                note_id = %note_id,
                error = %e,
                "MA#18 recovery: note_dump is not reconstructable — leaving quarantined"
            );
            return Ok(RecoveryOutcome::StillBlocked {
                note_id,
                reason: "dump_unparsable",
            });
        }
    };

    let (destination_network, destination_address) =
        match derive_destination_from_felts(&parsed.storage_felts) {
            Ok(v) => v,
            Err(_) => {
                // Truly erased storage (e.g. a single-felt placeholder) — the
                // destination is unrecoverable proxy-side. This is the genuine
                // erased-note case: needs the off-chain preimage or a
                // protocol-level fix. Leave quarantined as the hard signal.
                return Ok(RecoveryOutcome::StillBlocked {
                    note_id,
                    reason: "storage_unrecoverable",
                });
            }
        };

    // Cantina #13 poison-leaf gate: a bridge-out targeting the local network is
    // an un-settleable exit; recovering (emitting) it would wedge the next
    // certificate. Never emit — leave quarantined for operator escalation.
    if destination_network == local_network_id {
        return Ok(RecoveryOutcome::StillBlocked {
            note_id,
            reason: "self_targeted_poison",
        });
    }
    if crate::bridge_out::is_invalid_destination_address(&destination_address) {
        return Ok(RecoveryOutcome::StillBlocked {
            note_id,
            reason: "invalid_destination_address",
        });
    }

    let Some(&(faucet_id, miden_amount)) = parsed.fungible_assets.first() else {
        return Ok(RecoveryOutcome::StillBlocked {
            note_id,
            reason: "no_fungible_asset",
        });
    };

    let origin = match crate::bridge_out::resolve_faucet_origin(faucet_id, &**store).await {
        Ok(o) => o,
        Err(_) => {
            // The blocker persists (faucet still not registered). This is the
            // common transient MA#18 case; re-attempt on a later sweep once the
            // operator registers the faucet.
            return Ok(RecoveryOutcome::StillBlocked {
                note_id,
                reason: "faucet_still_unknown",
            });
        }
    };

    let origin_amount = match crate::bridge_out::reverse_scale_amount(miden_amount, origin.scale) {
        Ok(v) => v,
        Err(_) => {
            return Ok(RecoveryOutcome::StillBlocked {
                note_id,
                reason: "amount_overflow",
            });
        }
    };

    // Metadata: unlike the live projector, the recovery path has no Miden client
    // to run the Cantina #13 Layer-2 ERC-20 metadata re-derivation. If a
    // non-native token still has empty stored metadata, refuse to emit an
    // unvalidated empty-metadata leaf — the operator must backfill the faucet
    // registry (admin_registerFaucet) first, then re-run recovery. Native ETH
    // (zero origin address) legitimately carries empty metadata.
    let is_erc20 = origin.origin_address != [0u8; 20];
    if origin.metadata.is_empty() && is_erc20 {
        return Ok(RecoveryOutcome::StillBlocked {
            note_id,
            reason: "metadata_empty_erc20",
        });
    }
    if origin.metadata.len() > crate::bridge_out::MAX_BRIDGE_EVENT_METADATA_BYTES {
        return Ok(RecoveryOutcome::StillBlocked {
            note_id,
            reason: "metadata_too_large",
        });
    }

    // ── Commit — the SAME path a normal bridge-out takes ──────────────────
    // NOTE (merge-order): on current main the b2agg commit is the two-step
    // `mark_note_processed` + `add_bridge_event`. Audit H1 (PR #117) replaces
    // that with the atomic `commit_b2agg_event_atomic`. If #117 lands first,
    // rebase this emit onto the atomic call (identical fields, returns
    // deposit_count, self-rolls-back — drop the manual unmark below).
    let tx_hash = crate::bridge_out::derive_bridge_out_tx_hash(&note_id);
    let block_hash = block_state.get_block_hash(entry.observed_block);
    let deposit_count = store.mark_note_processed(note_id.clone()).await?;

    if let Err(err) = store
        .add_bridge_event(
            bridge_address,
            entry.observed_block,
            block_hash,
            &tx_hash,
            0, // LEAF_TYPE_ASSET
            origin.origin_network,
            &origin.origin_address,
            destination_network,
            &destination_address,
            origin_amount,
            &origin.metadata,
            deposit_count,
        )
        .await
    {
        // Roll back the counter bump so the note is retried cleanly.
        let _ = store.unmark_note_processed(&note_id).await;
        return Err(err);
    }

    // Clear the quarantine handle now that the leaf is enacted.
    store.delete_unbridgeable_bridge_out(&note_id).await?;

    metrics::counter!(
        "bridge_out_recovered_unbridgeable_total",
        "reason" => entry.reason.as_str()
    )
    .increment(1);
    tracing::info!(
        target: "bridge_out::recovery",
        note_id = %note_id,
        deposit_count,
        destination_network,
        original_reason = entry.reason.as_str(),
        "MA#18 recovery: re-derived + emitted BridgeEvent from quarantine dump; \
         deposit_count advanced, quarantine row cleared"
    );

    Ok(RecoveryOutcome::Recovered {
        note_id,
        deposit_count,
        destination_network,
    })
}

/// Sweep the entire `unbridgeable_bridge_outs` quarantine table and attempt to
/// recover every row. Recovered rows emit a BridgeEvent + are deleted; blocked
/// rows are left in place (and counted). This is the driver wired into the
/// Cantina #9 LET-divergence monitor (run when the on-chain LET is ahead) and
/// exposed to operators via `admin_recoverUnbridgeableBridgeOuts`.
pub async fn recover_all_unbridgeable_bridge_outs(
    store: &Arc<dyn Store>,
    block_state: &BlockState,
    bridge_address: &str,
    local_network_id: u32,
) -> anyhow::Result<RecoverySummary> {
    let rows = store.list_unbridgeable_bridge_outs().await?;
    let mut summary = RecoverySummary {
        attempted: rows.len(),
        ..Default::default()
    };
    for entry in &rows {
        match recover_unbridgeable_bridge_out(
            store,
            block_state,
            entry,
            bridge_address,
            local_network_id,
        )
        .await
        {
            Ok(RecoveryOutcome::Recovered { .. }) => summary.recovered += 1,
            Ok(RecoveryOutcome::StaleCleared { .. }) => summary.stale_cleared += 1,
            Ok(RecoveryOutcome::StillBlocked { note_id, reason }) => {
                summary.still_blocked += 1;
                metrics::counter!(
                    "bridge_out_recovery_still_blocked_total",
                    "reason" => reason
                )
                .increment(1);
                tracing::debug!(
                    target: "bridge_out::recovery",
                    note_id = %note_id,
                    reason,
                    "MA#18 recovery: row still blocked"
                );
            }
            Err(e) => {
                // A transient store error on one row must not abort the sweep.
                summary.still_blocked += 1;
                tracing::warn!(
                    target: "bridge_out::recovery",
                    note_id = %entry.note_id,
                    error = %e,
                    "MA#18 recovery: transient error recovering row — will retry next sweep"
                );
            }
        }
    }
    Ok(summary)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::memory::InMemoryStore;
    use crate::store::{FaucetEntry, UnbridgeableBridgeOut, UnbridgeableBridgeOutReason};

    // A B2AGG destined for network 7, address 0x01..14 (20 bytes), from a
    // known faucet. Storage felts are the byte-swapped network + 5 LE limbs,
    // exactly as `parse_b2agg_storage` expects.
    fn sample_storage_felts(dest_network: u32) -> Vec<u64> {
        // Encode as the writer does: u32::from_le_bytes(dest.to_be_bytes()).
        let raw = u32::from_le_bytes(dest_network.to_be_bytes());
        // Address 0x0102030405060708090a0b0c0d0e0f1011121314 → 5 LE u32 limbs.
        let addr: [u8; 20] = [
            1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20,
        ];
        let mut felts = vec![raw as u64];
        for i in 0..5 {
            let limb = u32::from_le_bytes(addr[i * 4..(i + 1) * 4].try_into().unwrap());
            felts.push(limb as u64);
        }
        felts
    }

    fn make_dump(storage: &[u64], faucet: AccountId, amount: u64) -> String {
        let items: Vec<String> = storage.iter().map(|v| v.to_string()).collect();
        format!(
            "{{\"script_root\":\"0xdead\",\"storage_items\":[{}],\"fungible_assets\":[{{faucet={}, amount={}}}]}}",
            items.join(","),
            faucet.to_hex(),
            amount
        )
    }

    fn faucet_id() -> AccountId {
        // A valid fungible-faucet AccountId (same shape the restore.rs tests use;
        // round-trips via to_hex/from_hex).
        AccountId::from_hex("0xac0000000000dd110000ee000000fc").unwrap()
    }

    fn faucet_entry(id: AccountId) -> FaucetEntry {
        // Native ETH (zero origin address) so empty metadata is legitimately
        // emittable; scale 0 so origin_amount == miden_amount.
        FaucetEntry {
            faucet_id: id,
            origin_network: 0,
            origin_address: [0u8; 20],
            symbol: "ETH".to_string(),
            origin_decimals: 18,
            miden_decimals: 18,
            scale: 0,
            metadata: vec![],
        }
    }

    async fn register_eth_faucet(store: &InMemoryStore, id: AccountId) {
        store.register_faucet(faucet_entry(id)).await.unwrap();
    }

    #[test]
    fn dump_round_trips_through_parser() {
        let f = faucet_id();
        let storage = sample_storage_felts(7);
        let dump = make_dump(&storage, f, 4242);
        let parsed = parse_quarantine_dump(&dump).unwrap();
        assert_eq!(parsed.storage_felts, storage);
        assert_eq!(parsed.fungible_assets, vec![(f, 4242)]);
    }

    #[test]
    fn dump_parser_rejects_malformed() {
        assert!(parse_quarantine_dump("not a dump at all").is_err());
        // Missing fungible_assets list.
        assert!(
            parse_quarantine_dump("{\"storage_items\":[1,2,3]}").is_err(),
            "missing fungible_assets must error"
        );
    }

    #[test]
    fn derive_destination_matches_canonical_parser() {
        // Cross-check against the canonical NoteStorage parser so the two
        // decoders can never drift.
        use miden_protocol::Felt;
        use miden_protocol::note::NoteStorage;
        let storage = sample_storage_felts(7);
        let (net, addr) = derive_destination_from_felts(&storage).unwrap();
        assert_eq!(net, 7);

        let felts: Vec<Felt> = storage.iter().map(|v| Felt::new(*v).unwrap()).collect();
        let ns = NoteStorage::new(felts).unwrap();
        let (net2, addr2) = crate::bridge_out::parse_b2agg_storage(&ns).unwrap();
        assert_eq!((net, addr), (net2, addr2));
    }

    #[test]
    fn derive_destination_rejects_erased_storage() {
        // A single-felt "erased" placeholder is unrecoverable.
        assert!(derive_destination_from_felts(&[0]).is_err());
    }

    #[tokio::test]
    async fn recovers_unknown_faucet_once_registered() {
        let store = InMemoryStore::new();
        let f = faucet_id();
        let note_id = "a1b2c3".to_string();
        let dump = make_dump(&sample_storage_felts(7), f, 1000);
        assert!(
            store
                .record_unbridgeable_bridge_out(UnbridgeableBridgeOut {
                    note_id: note_id.clone(),
                    bridge_account: f,
                    reason: UnbridgeableBridgeOutReason::UnknownFaucet,
                    detail: "unknown faucet".to_string(),
                    note_dump: dump,
                    observed_block: 55,
                })
                .await
                .unwrap()
        );
        let store: Arc<dyn Store> = Arc::new(store);
        let bs = BlockState::new();

        // BEFORE registering the faucet: still blocked, row stays, no deposit.
        let out = recover_unbridgeable_bridge_out(
            &store,
            &bs,
            &row(&store, &note_id).await,
            "0xbridge",
            1,
        )
        .await
        .unwrap();
        assert_eq!(
            out,
            RecoveryOutcome::StillBlocked {
                note_id: note_id.clone(),
                reason: "faucet_still_unknown"
            }
        );
        assert_eq!(store.get_deposit_count().await.unwrap(), 0);
        assert!(
            store
                .get_unbridgeable_bridge_out(&note_id)
                .await
                .unwrap()
                .is_some()
        );

        // Register the faucet — the blocker is now resolved.
        store.register_faucet(faucet_entry(f)).await.unwrap();

        let out = recover_unbridgeable_bridge_out(
            &store,
            &bs,
            &row(&store, &note_id).await,
            "0xbridge",
            1,
        )
        .await
        .unwrap();
        match out {
            RecoveryOutcome::Recovered {
                deposit_count,
                destination_network,
                ..
            } => {
                assert_eq!(deposit_count, 0);
                assert_eq!(destination_network, 7);
            }
            other => panic!("expected Recovered, got {other:?}"),
        }
        // deposit_count advanced, quarantine row cleared, note marked processed.
        assert_eq!(store.get_deposit_count().await.unwrap(), 1);
        assert!(store.is_note_processed(&note_id).await.unwrap());
        assert!(
            store
                .get_unbridgeable_bridge_out(&note_id)
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn self_targeted_leaf_is_never_recovered() {
        let store = InMemoryStore::new();
        let f = faucet_id();
        register_eth_faucet(&store, f).await;
        let note_id = "poison".to_string();
        // destination_network == local_network_id (9) → poison leaf.
        let dump = make_dump(&sample_storage_felts(9), f, 1);
        store
            .record_unbridgeable_bridge_out(UnbridgeableBridgeOut {
                note_id: note_id.clone(),
                bridge_account: f,
                reason: UnbridgeableBridgeOutReason::UnknownFaucet,
                detail: "x".to_string(),
                note_dump: dump,
                observed_block: 1,
            })
            .await
            .unwrap();
        let store: Arc<dyn Store> = Arc::new(store);
        let bs = BlockState::new();
        let out = recover_unbridgeable_bridge_out(
            &store,
            &bs,
            &row(&store, &note_id).await,
            "0xbridge",
            9, // local network id == destination → poison
        )
        .await
        .unwrap();
        assert_eq!(
            out,
            RecoveryOutcome::StillBlocked {
                note_id: note_id.clone(),
                reason: "self_targeted_poison"
            }
        );
        assert!(
            store
                .get_unbridgeable_bridge_out(&note_id)
                .await
                .unwrap()
                .is_some()
        );
        assert_eq!(store.get_deposit_count().await.unwrap(), 0);
    }

    #[tokio::test]
    async fn erased_storage_row_stays_blocked() {
        let store = InMemoryStore::new();
        let f = faucet_id();
        register_eth_faucet(&store, f).await;
        let note_id = "erased".to_string();
        // Single-felt storage → destination unrecoverable (the true erased case).
        let dump = make_dump(&[0], f, 1);
        store
            .record_unbridgeable_bridge_out(UnbridgeableBridgeOut {
                note_id: note_id.clone(),
                bridge_account: f,
                reason: UnbridgeableBridgeOutReason::StorageParseFailed,
                detail: "storage too short".to_string(),
                note_dump: dump,
                observed_block: 1,
            })
            .await
            .unwrap();
        let store: Arc<dyn Store> = Arc::new(store);
        let bs = BlockState::new();
        let out = recover_unbridgeable_bridge_out(
            &store,
            &bs,
            &row(&store, &note_id).await,
            "0xbridge",
            1,
        )
        .await
        .unwrap();
        assert_eq!(
            out,
            RecoveryOutcome::StillBlocked {
                note_id,
                reason: "storage_unrecoverable"
            }
        );
        assert_eq!(store.get_deposit_count().await.unwrap(), 0);
    }

    #[tokio::test]
    async fn sweep_recovers_and_reports_summary() {
        let store = InMemoryStore::new();
        let f = faucet_id();
        register_eth_faucet(&store, f).await;
        // One recoverable (good storage + registered faucet) + one erased.
        store
            .record_unbridgeable_bridge_out(UnbridgeableBridgeOut {
                note_id: "good".to_string(),
                bridge_account: f,
                reason: UnbridgeableBridgeOutReason::UnknownFaucet,
                detail: "x".to_string(),
                note_dump: make_dump(&sample_storage_felts(7), f, 500),
                observed_block: 3,
            })
            .await
            .unwrap();
        store
            .record_unbridgeable_bridge_out(UnbridgeableBridgeOut {
                note_id: "erased".to_string(),
                bridge_account: f,
                reason: UnbridgeableBridgeOutReason::StorageParseFailed,
                detail: "x".to_string(),
                note_dump: make_dump(&[0], f, 1),
                observed_block: 3,
            })
            .await
            .unwrap();
        let store: Arc<dyn Store> = Arc::new(store);
        let bs = BlockState::new();
        let summary = recover_all_unbridgeable_bridge_outs(&store, &bs, "0xbridge", 1)
            .await
            .unwrap();
        assert_eq!(summary.attempted, 2);
        assert_eq!(summary.recovered, 1);
        assert_eq!(summary.still_blocked, 1);
        assert_eq!(store.get_deposit_count().await.unwrap(), 1);
        // The recoverable row was cleared; the erased one remains as the signal.
        assert!(
            store
                .get_unbridgeable_bridge_out("good")
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            store
                .get_unbridgeable_bridge_out("erased")
                .await
                .unwrap()
                .is_some()
        );
    }

    // ── test helpers ─────────────────────────────────────────────
    async fn row(store: &Arc<dyn Store>, note_id: &str) -> UnbridgeableBridgeOut {
        store
            .get_unbridgeable_bridge_out(note_id)
            .await
            .unwrap()
            .expect("row present")
    }
}
