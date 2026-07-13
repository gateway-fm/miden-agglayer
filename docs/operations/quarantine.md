# B2AGG quarantine — diagnosis & recovery runbook

What to do when a bridge-out (B2AGG) note was consumed by the bridge **on-chain** but the
proxy **refused to emit its BridgeEvent** — most importantly the case where the exit came
from a **Miden-originated (native) faucet that was never registered/allowlisted**.

## 1. What quarantine is (and why it's fail-closed)

The proxy never emits a BridgeEvent it cannot fully derive and validate: a wrong or
half-filled event would enter certificates and the L1 exit tree (a consensus-level fault),
and getLogs immutability forbids fixing an emitted event later. So an underivable exit is
**quarantined**: recorded in the `unbridgeable_bridge_outs` store table, logged, counted in
metrics — and **no event is emitted**. The projector advances (a quarantine never wedges
the chain).

**The sharp edge:** by the time the proxy quarantines, the depositor's asset is already
**locked in the bridge on-chain**. The exit stays invisible to aggkit/agglayer until an
operator completes the recovery below. Funds are safe (locked, not lost) but stranded
until then.

### Quarantine reasons (`unbridgeable_bridge_outs.reason`)

| Reason | Meaning | Typical cause |
|---|---|---|
| `UnknownFaucet` | The asset's faucet has no `faucet_registry` row | **Native faucet bridged out without `admin_registerNativeFaucet`**; or a foreign faucet whose registry row was lost |
| `StorageParseFailed` | Note storage missing/truncated/overflowing ("erased note", MA#18) | Note erased by the node (created+consumed in one batch) and unrecoverable |
| `NoFungibleAsset` | The consumed B2AGG carried no fungible asset | Malformed/dust note |
| (metadata refusal) | ERC-20 metadata empty and not validatable against the bridge's hash | Poisoned/lost registry metadata (see cantina13) — logged as *"refusing to emit empty/unvalidated metadata"* |

## 2. The unregistered-native-faucet scenario, step by step

1. An external party deploys a native faucet (e.g. `bridge-out-tool --create-native-faucet`)
   and mints — **but the admin allowlist step is skipped**.
2. A user bridges out: the B2AGG note is created and the bridge **consumes it on-chain**
   (the asset is now locked in the bridge vault).
3. The proxy projects the consumption, calls `resolve_faucet_origin` → **no registry row** →
   error *"unknown faucet ID …: not found in faucet registry. Register the faucet via
   admin_registerFaucet or bridge a claim first."*
4. The exit is quarantined as `UnknownFaucet`. No BridgeEvent. The user's wrapped tokens
   never appear on the destination chain.

## 3. Diagnosis

**Logs** (the proxy, ANSI-strip before grepping):

```sh
docker logs <proxy> 2>&1 | sed -e 's/\x1b\[[0-9;]*m//g' \
  | grep -aiE "unknown faucet|refusing to emit|quarantin|unbridgeable"
# restore path warn:  "restore: B2AGG unknown faucet: …"
```

**Metrics** (must-watch; alert on any increase):

| Metric | Meaning |
|---|---|
| `bridge_out_unknown_faucet_total` | exits hitting the unknown-faucet path |
| `bridge_out_quarantined_erased_b2agg_total` | quarantine rows written (MA#18) |
| `bridge_out_metadata_unrecoverable_total` | metadata-refusal deferrals |
| `synthetic_projector_completeness_missing_total` | in-proxy completeness auditor — a quarantine is *excluded* here (deliberate non-emit), so this staying 0 while the above rise is the expected quarantine signature |

**Store** (the operator's concrete handle — one row per quarantined exit):

```sql
SELECT note_id, reason, detail, observed_block, created_at
FROM unbridgeable_bridge_outs ORDER BY created_at DESC;
```

`detail` carries the human-readable cause (including the faucet id for `UnknownFaucet`);
`note_dump` carries the full note for offline analysis. (No admin RPC lists this table
yet — query the store directly; postgres in production deployments.)

**External cross-check:** `scripts/monitoring/watch-completeness.sh` classifies deliberate
refusals as `EXPECTED-QUARANTINE` (matching the proxy's refusal warns by faucet) and only
prints `COMPLETENESS VIOLATION` for unexplained absences. A quarantine therefore shows as
EXPECTED-QUARANTINE there — if you see VIOLATION instead, you are NOT looking at a
quarantine; treat it as a completeness incident.

## 4. Recovery (unregistered native faucet)

> Order matters: **register first, then restore**. The live path deliberately does NOT
> retro-emit after a mere registry fix (getLogs immutability — the block is sealed);
> recovery of already-quarantined exits goes through `--restore`, which rebuilds the
> synthetic chain with the fixed registry so the event exists at its exact historical
> block.

1. **Verify the faucet is legitimate** before registering — the registry is a security
   boundary (allowlist). Confirm with the token issuer: faucet account id, intended L1
   token address, symbol, decimals. A quarantine can also be *someone probing with a
   garbage faucet* — in that case, register nothing; the row is the audit trail.

2. **Register (allowlist) the native faucet** via the admin API (auth: `ADMIN_API_KEY`):

```sh
curl -s -X POST http://<proxy>:8546 \
  -H "content-type: application/json" -H "Authorization: Bearer $ADMIN_API_KEY" \
  -d '{"jsonrpc":"2.0","id":1,"method":"admin_registerNativeFaucet","params":[{
        "faucet_id":       "0x<miden faucet account id>",
        "origin_token_address": "0x<20-byte canonical token address>",
        "symbol":          "XYZ",
        "decimals":        18,
        "name":            "XYZ Token"
      }]}'
# verify:
#   method admin_listFaucets → the new row with is_native=true, origin_network=1
```

3. **Run `--restore`** (rebuilds the store from the Miden node + L1, then exits — see
   `docs/operations/runbook.md` §R2 for the full procedure and flags):

```sh
docker compose stop miden-agglayer
docker compose run --rm --no-deps miden-agglayer \
    <normal command line...> --reset-miden-store --restore
docker compose up -d miden-agglayer
```

4. **Verify recovery:**
   - the exit's BridgeEvent now exists at its consumption block:
     `eth_getLogs` at that block for topic `0x501781…62f9b`;
   - the quarantine row for that note is gone / superseded after restore;
   - the user's claim proceeds normally on the destination chain (autoclaim or manual);
   - completeness watcher back to plain `OK` lines.

## 5. Recovery (other reasons)

- **Metadata refusal** — backfill `faucet_registry.metadata` (or wire an L1 RPC for the
  token's origin network so the proxy can fetch + keccak-validate it), then `--restore`.
  The exact operator hint is printed in the warn itself.
- **`StorageParseFailed` / `NoFungibleAsset`** — genuinely unrecoverable from the proxy's
  side (the note's content is gone or empty). The row is the audit trail; resolution is a
  token-issuer/bridge-governance question, not a proxy operation.

## 6. Prevention

- **Allowlist before announcing**: a native token must be registered
  (`admin_registerNativeFaucet`) *before* users can bridge it — the on-chain bridge will
  lock assets regardless of proxy registration.
- Alert on `bridge_out_unknown_faucet_total` > 0 — it is always either a mis-ordered
  rollout (register late) or a probe.
- Release follow-up (task #47): e2e covering this exact scenario end-to-end
  (lock → quarantine → register → restore → claimable), plus a live-projection-path unit
  test with a native-shaped unregistered faucet.
