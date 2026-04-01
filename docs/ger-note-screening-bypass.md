# GER Injection: NoteScreener Bypass

## Problem

After the first L1→L2 CLAIM is processed by the NTX builder, subsequent
UpdateGerNote submissions fail with:

```
NoteScreenerError → NoteCheckerError → TransactionPreparation
  → FetchAssetWitnessFailed → MerkleStoreError → RootNotInStore
```

This blocks all GER injection after the first claim, which prevents the
bridge-service from resolving exit roots for new deposits, which prevents
ClaimTxManager from creating claims for dynamic ERC-20 tokens.

## Root Cause

The miden-client's `submit_new_transaction` internally calls
`apply_transaction` → `get_note_updates` → `NoteScreener::can_consume_batch`.

The NoteScreener runs a **test execution** of the UpdateGerNote against the
bridge account to check if it can be consumed. This test needs the bridge
account's asset Merkle tree from the local store. After the NTX builder
modifies the bridge account (minting faucet tokens during CLAIM processing),
the asset tree root changes. The local client's Merkle store doesn't have
this new root → `RootNotInStore` → screening fails → entire submission fails.

### Why it only fails after the first CLAIM

1. Before CLAIM: bridge account asset tree is unchanged since init, local
   store has the correct root
2. CLAIM succeeds: NTX builder mints tokens on the bridge account, changing
   the asset tree root on the miden-node
3. `sync_state()` updates the account header but does NOT pull the full
   asset SMT data
4. Next UpdateGerNote: screener tries to load asset tree → root not in local
   store → `FetchAssetWitnessFailed`

### How Igor's aggkit-proxy avoids this

Igor's version uses the same `submit_new_transaction` call, but imports the
bridge account as a `NoAuth` tracked account with full state. The miden-client
test suite (agglayer_bridge_in_out.rs) does the same — all target accounts
are pre-registered in every client instance.

Our architecture differs: we use a service account to submit transactions and
only reference the bridge account by ID. The client doesn't maintain a full
local copy of the bridge account's Merkle trees.

## Solution: Split Execute → Prove → Submit

Instead of `submit_new_transaction` (which bundles execute + prove + submit +
apply), we use the individual methods:

```rust
let tx_result = client.execute_transaction(service_id, tx_request).await?;
let proven = client.prove_transaction(&tx_result).await?;
let height = client.submit_proven_transaction(proven, &tx_result).await?;

// apply_transaction runs the NoteScreener. If it fails, the TX is
// already on the node — log and continue.
if let Err(e) = client.apply_transaction(&tx_result, height).await {
    tracing::warn!("apply_transaction failed (TX already submitted): {e:#}");
}
```

### Why this is safe

1. **The miden-node validates the proof independently.** The
   `submit_proven_transaction` step sends the proven TX to the node, which
   runs its own verification. If the proof is invalid, the node rejects it.
   We are not bypassing node-side validation.

2. **UpdateGerNote carries zero assets.** The `FetchAssetWitnessFailed` error
   is about validating an asset tree that is irrelevant for UpdateGerNote —
   these notes only carry GER data in their storage slots, not fungible
   assets.

3. **The NoteScreener is a client-side optimization.** It decides whether to
   save the output note locally as a future input note. The NTX builder on
   the miden-node consumes the note regardless of the client's local
   tracking.

4. **Scope is limited to GER injection only.** CLAIM notes still use
   `submit_new_transaction` with full screening. We only tolerate
   `apply_transaction` failure in the GER injection code path.

5. **We still retry on execution/proving failures.** The retry loop catches
   errors from `execute_transaction` (stale state commitment) and retries
   after `sync_state()`. Only `apply_transaction` errors are tolerated.

### What we lose

- The local client store won't track the UpdateGerNote as an input note.
  This is acceptable because we don't consume UpdateGerNotes locally — the
  NTX builder on the miden-node handles consumption.

- If `apply_transaction` fails, the local transaction history may be
  incomplete. We mitigate this by polling `get_transactions` for commitment
  confirmation.

## Long-term fix

The proper fix is one of:

1. **Import the bridge account with full state** into the miden-client, so
   the NoteScreener has the complete asset SMT. This requires architectural
   changes to how we manage account state.

2. **Register UpdateGerNote tags** in the miden-client during init, so the
   NoteScreener can use tag-based screening instead of execution-based
   screening. Requires understanding the exact NoteTag computation for
   UpdateGerNote.

3. **Upstream miden-client fix**: Allow `apply_transaction` to succeed even
   when the NoteScreener can't validate zero-asset notes. The asset witness
   check is unnecessary for notes that don't carry assets.

## Related files

- `src/ger.rs` — GER injection with split submission flow
- `src/init.rs` — Account initialization (where bridge account is created)
- `src/claim.rs` — CLAIM processing (uses `submit_new_transaction`)
- miden-client `crates/rust-client/src/transaction/mod.rs:527` — NoteScreener call
- miden-client `crates/rust-client/src/note/note_screener.rs:100` — `can_consume_batch`
- miden-tx `src/executor/notes_checker.rs:150` — `can_consume` → `prepare_tx_inputs`

## Timeline

- **2026-03-27**: aggkit upgraded to 0.8.3-rc1, GER injection failures observed
- **2026-04-01**: Root cause identified (FetchAssetWitnessFailed after CLAIM)
- **2026-04-01**: Split submission flow implemented as targeted bypass
