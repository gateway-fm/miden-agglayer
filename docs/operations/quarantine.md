# B2AGG quarantine

A quarantine row means the bridge consumed a B2AGG note on Miden, but the
service could not derive a fully validated synthetic `BridgeEvent`. Emitting a
guessed event would poison certificates and immutable `eth_getLogs` history, so
the projector fails closed: it records the note in
`unbridgeable_bridge_outs`, increments a labelled counter, and emits no event.

This is a funds-impacting incident. The on-chain bridge consumption and LET
advance have already happened, while AggKit cannot see an exit to certify.

## Detection

Alert on any increase in:

```text
bridge_out_quarantined_erased_b2agg_total{reason=...}
```

Also watch `bridge_out_unknown_faucet_total`,
`bridge_out_metadata_unrecoverable_total`, and
`bridge_out_b2agg_metadata_too_large_total`.

The persisted handle is:

```sql
SELECT note_id, bridge_account, reason, detail,
       observed_block, created_at
FROM unbridgeable_bridge_outs
ORDER BY created_at DESC;
```

Retrieve `note_dump` only into a restricted forensic workspace; it can be
large and contains the note's captured script/storage/asset material:

```sql
SELECT note_id, note_dump
FROM unbridgeable_bridge_outs
WHERE note_id = :'note_id';
```

Correlate the row with logs from target `bridge_out::quarantine`, the Miden
consumption block/transaction, bridge LET state, and AggKit certificate range.

## Current reason values

The enum and Postgres decoder currently support:

| `reason` | Meaning |
|---|---|
| `storage_parse_failed` | B2AGG storage was absent, truncated, or contained a value that could not be decoded |
| `no_fungible_asset` | The consumed note had no fungible asset |
| `unknown_faucet` | The asset faucet had no local registry identity, so origin token fields could not be derived |
| `amount_overflow` | Reverse decimal scaling could not fit the L1 amount |
| `atomic_commit_failed` | The atomic synthetic-event/processed-state store commit failed and rolled back |
| `metadata_too_large` | Validated metadata exceeded the 64 KiB event cap |

Rows are first-write-wins by `note_id`; repeated observation does not replace
the original evidence.

Two related fail-close cases are not guaranteed to create this table row:

- unrecoverable empty ERC-20 metadata increments
  `bridge_out_metadata_unrecoverable_total` and remains retryable by a later
  restore after authoritative metadata becomes available;
- a B2AGG targeting the local network increments
  `bridge_out_self_targeted_total` and is refused as a poison leaf.

Investigate those from their logs and note ID even if the table is empty.

## Immediate response

1. Preserve current/previous logs, the quarantine row including `note_dump`,
   the matching Miden note/transaction evidence, image digest, account-config
   checksum, and a Postgres snapshot.
2. Pause new bridge-outs if subsequent certificates would include the bad LET
   leaf or if the cause can affect more notes.
3. Confirm that no synthetic `BridgeEvent` exists for the note/deposit. Do not
   manufacture one with SQL.
4. Classify whether the missing data can be established authoritatively or is
   intrinsically unavailable.
5. Escalate to the bridge/AggLayer owner before any restore or governance
   action. The proxy has no live per-note replay/admin endpoint.

The leaf's exact deposit index was already reserved before quarantine; it remains
`emitted = false`. Do not delete or alter that reservation, delete the quarantine
row, mark the note processed, change the deposit counter, or insert a synthetic log
manually. Those actions bypass execution ordering, hash-chain, receipt, and
immutability rules.

## Unknown faucet

First determine whether the faucet is legitimate and whether it is already
registered in the bridge account. An absent local row can mean store loss, an
out-of-band admin action, or an unsupported/hostile faucet. The
`FaucetRegistryReconciler` treats an on-chain registration without a valid local
row as a security tripwire and can halt the process.

For a legitimate externally deployed Miden-native faucet that has **not yet
produced a quarantined exit**, the supported allow-list call is
`admin_registerNativeFaucet`. It requires `ADMIN_API_KEY` and derives
`origin_network` from this service's configured `NETWORK_ID`:

```bash
curl -fsS -X POST "$PROXY_RPC" \
  -H 'content-type: application/json' \
  -H "Authorization: Bearer $ADMIN_API_KEY" \
  -d '{
    "jsonrpc":"2.0",
    "id":1,
    "method":"admin_registerNativeFaucet",
    "params":[{
      "faucet_id":"0xACCOUNT_ID",
      "origin_token_address":"0x20_BYTE_ADDRESS",
      "symbol":"TOKEN",
      "decimals":8,
      "name":"Token name"
    }]
  }'
```

The endpoint sends a Miden bridge-configuration transaction and persists the
registry row; it is not a read-only repair. Verify identity, ownership, symbol,
decimals, canonical origin address, and approval before calling it.

Registering a faucet does **not** retroactively emit an event for an existing
quarantine row. It only prevents/fixes the identity prerequisite. Historical
reconstruction requires an offline-rehearsed full-store restore or a future
purpose-built recovery implementation.

## Recovery boundary

The current service does not implement the recovery RPC sketched in migration
`006_unbridgeable_bridge_outs.sql`. Therefore there is no supported command to
replay one quarantined note into a live, already-exposed synthetic chain.

Some causes can be made derivable in a clean reconstruction:

- restore the exact existing faucet identity from authoritative bridge/Miden
  state;
- make authoritative ERC-20 metadata available and validate its hash;
- deploy code that fixes a deterministic parse/atomic-store bug.

Others (`no_fungible_asset`, irretrievably absent storage, self-targeted poison
leaf) cannot be repaired by supplying local metadata.

If a full `--restore` into a clean coordinated store is proposed:

1. Treat it as disaster recovery, not a row-level fix.
2. Rehearse against cloned Postgres and Miden stores with `--read-only` where
   chain mutation is not intended.
3. Prove every previously exposed log remains byte-identical at its historical
   block and the recovered event is derived from authoritative data.
4. Coordinate downtime, backups, AggKit/certificate state, and rollback with
   the chain owners.
5. Never wipe the keystore or `bridge_accounts.toml`.

If those proofs cannot be made, resolution belongs to bridge governance/token
issuers rather than an operator database edit.

## Prevention

- Register and independently verify Miden-native faucets before enabling user
  bridge-outs.
- Keep Postgres, Miden store, keystore, and account config under coordinated
  backup/restore procedures.
- Keep `L1_RPC_URL` available for ERC-20 metadata recovery where applicable.
- Alert on every quarantine/integrity counter increase.
- Validate destination network/address and token route before creating a B2AGG.
- Run the repository completeness and immutability monitors during load and
  release acceptance.
