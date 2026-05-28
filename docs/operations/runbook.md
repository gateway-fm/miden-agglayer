# miden-agglayer operations runbook

Step-by-step recovery procedures for the failure modes we know about.
For pre-fix snapshot collection (logs / SQL / pod state), use
[`diagnostics.md`](./diagnostics.md) first. The runbook assumes you
already have a verdict.

## How to use this doc

1. Identify the failure mode (most recently fired alert; or the verdict
   block from the diagnostic skill).
2. Jump to the matching section.
3. Read the **blast radius** + **rollback** lines before executing.
4. Note that anything labelled `<TODO: ...>` requires confirmation from
   Max before running on bali — the underlying behaviour hasn't been
   confirmed in this revision of the doc.

## Common preamble — port-forwards + secrets

Most procedures need at least one of:

```bash
# Proxy DB
kubectl -n outpost-testnet-miden-testnet port-forward svc/miden-agglayer-db 15434:5432 &

# Bridge-service DB
kubectl -n outpost-testnet-miden-testnet port-forward svc/bridge-db 15435:5432 &

# Service JSON-RPC
kubectl -n outpost-testnet-miden-testnet port-forward svc/miden-agglayer 8546:8546 &

# Verify all listeners came up
ss -tlnp | grep -E ':(8546|1543[45])'
```

Read the DB passwords from secrets — never paste them into chat:

```bash
export PROXY_PG_PASSWORD="$(
  kubectl -n outpost-testnet-miden-testnet get secret miden-agglayer-secret \
    -o jsonpath='{.data.database_url}' \
  | base64 -d | grep -oP 'password=\K[^ ]+')"

# Bridge-service password lives in 1Password.
# <TODO: confirm 1Password item name with Max>
```

`kubectl exec`-free SQL via docker:

```bash
docker run --rm --network host -e PGPASSWORD="$PROXY_PG_PASSWORD" postgres:17-alpine \
  psql -h 127.0.0.1 -p 15434 -U agglayer -d agglayer_store -c "<query>"
```

---

## Failure mode A — AccountDataNotFound / IAIC

Symptoms in logs:

- `account data wasn't found for account id 0x<id>`, OR
- `incorrect account initial commitment`, OR
- gRPC tail `transaction conflicts with current mempool state`.

In Prometheus: `MidenAgglayerAccountDivergence` alert firing; recently:
no fresh rows in `transactions` with `status='success'`.

Background: see [`../POSTMORTEM_2026-05-11_IAIC_TO_ADNF.md`](../POSTMORTEM_2026-05-11_IAIC_TO_ADNF.md).
Both symptoms map to the same class of fault — local miden-client store
diverged from on-chain state.

### A.1 Is it just a stale lock?

`miden_locked_accounts_detected_total > 0` at last startup, or startup
logs include:

```
src/main.rs: startup diagnostic: 1 managed account(s) are LOCKED in miden-client
```

Surgical unlock — preserves all local state (milliseconds), no
re-sync needed:

```bash
kubectl -n outpost-testnet-miden-testnet patch statefulset miden-agglayer \
  --type='json' \
  -p='[{"op":"add","path":"/spec/template/spec/containers/0/args/-","value":"--unlock-miden-accounts"}]'

kubectl -n outpost-testnet-miden-testnet rollout status statefulset miden-agglayer --watch

# Expect "unlocked N row(s)" then the pod exits cleanly.

# Remove the flag and let the pod restart normally
kubectl -n outpost-testnet-miden-testnet patch statefulset miden-agglayer \
  --type='json' \
  -p='[{"op":"remove","path":"/spec/template/spec/containers/0/args/<index-of-flag>"}]'
```

**Blast radius:** writes to `latest_account_headers` +
`historical_account_headers` in miden-client sqlite. Reversible — locks
are recovered from the node on the next sync if they were correct.

**Rollback:** none needed. The flag is idempotent; re-running it is safe.

### A.2 Full miden-store reset

Required when `--unlock-miden-accounts` doesn't clear the symptom — the
local sqlite has structurally diverged.

> **READ FIRST:** the runbook for this on bali specifically is
> [`../REDEPLOY_RUNBOOK_BALI.md`](../REDEPLOY_RUNBOOK_BALI.md). It captures the v0.4.1 version
> requirement, GER state assertions, and the cure-event log signatures
> we expect after redeploy. Follow that doc on bali; this section is the
> generic procedure for other clusters.

Procedure:

```bash
# 1. Confirm the pod image is at least v0.3.0 (Phase 0 reimport in
#    restore.rs is required for AccountDataNotFound recovery to work).
kubectl -n <namespace> describe pod <pod> | grep Image:

# 2. Add the recovery flags. Order matters: --reset wipes sqlite, then
#    --restore reimports accounts + replays state from the node + L1.
kubectl -n <namespace> patch statefulset miden-agglayer \
  --type='json' \
  -p='[
    {"op":"add","path":"/spec/template/spec/containers/0/args/-","value":"--reset-miden-store"},
    {"op":"add","path":"/spec/template/spec/containers/0/args/-","value":"--restore"}
  ]'

kubectl -n <namespace> rollout status statefulset miden-agglayer --watch

# 3. Expected boot log sequence:
#    INFO recovery.rs: reset_miden_store: deleted .../store.sqlite3
#    INFO restore.rs: === RESTORE: starting state reconstruction ===
#    INFO Phase 0: re-importing bridge accounts from Miden node...
#    INFO Phase 0 complete: bridge account reimport pass done
#    INFO Phase 1: sync_miden_block ...
#    INFO === RESTORE: complete ===
#    Then pod exits (return Ok). Kubernetes restarts it without the
#    recovery flags by virtue of step 4.

# 4. CRITICAL — remove the flags before the pod restarts again, otherwise
#    every restart loops through reset+restore and never serves traffic.
kubectl -n <namespace> patch statefulset miden-agglayer \
  --type='json' \
  -p='[
    {"op":"remove","path":"/spec/template/spec/containers/0/args/<index-of-restore>"},
    {"op":"remove","path":"/spec/template/spec/containers/0/args/<index-of-reset>"}
  ]'

# 5. Watch for the first successful GER inject post-recovery:
kubectl -n <namespace> logs -f <pod> | grep -E 'UpdateGerNote transaction committed|account data wasn'
```

**Blast radius:** deletes `store.sqlite3` (+ WAL/SHM) in the pod's
PersistentVolume. Keystore + `bridge_accounts.toml` are preserved
explicitly by `recovery::reset_miden_store`. The proxy's PgStore and
all on-chain state are untouched.

**Caveat:** Phase 0 reimport requires accounts to be `Public` storage
mode. The bali infrastructure accounts deployed before commit `34d4316`
(pre-v0.3.0) are `Private` — for those, `import_account_by_id` returns
`AccountIsPrivate` and the reimport silently fails for that account.
This is the case bali is in today — `<TODO: confirm with Max whether
bali's accounts have been redeployed as Public yet; if not, full reset
is not a safe option and the only recourse is the v0.4.1 redeploy
documented in REDEPLOY_RUNBOOK_BALI.md>`.

**Rollback:** none. If `restore` left the system worse, you must
escalate to Max + Igor for a manual rebuild of bridge_accounts.toml
against fresh accounts.

### A.3 The IAIC variant (mempool conflict, not store divergence)

If the gRPC tail is literally `transaction conflicts with current mempool
state` and not `incorrect account initial commitment` alone, the cause is
mempool serialisation, not local cache lag. This class of error was
**closed structurally in v0.3.0** by routing all submissions through the
`MidenClient` channel-of-1 — so on any v0.3.0+ deployment, observing a
fresh IAIC means something has reintroduced a parallel-submit path.

If a fresh IAIC fires on a v0.3.0+ deployment:

1. Capture a 30-minute Loki window around the first occurrence.
2. Look for two concurrent `submit_proven_transaction` calls in the same
   timestamp bucket — that's the regression signature.
3. Open an incident ticket and **page the maintainer** before redeploying.
   A self-heal will mask the regression but not fix it.

---

## Failure mode B — Stuck L1→L2 deposit (no claim arriving on L2)

User report: "I sent ETH on Sepolia, the bridge-service shows my deposit
but no balance has appeared on Miden."

Use the trace in [`diagnostics.md` section 4](./diagnostics.md#4-trace-a-single-l1l2-deposit)
to localise the wedge to one of:

- **Bridge-service hasn't marked the deposit `ready_for_claim`.** Cause:
  bridge-service hasn't ingested a GER that covers the deposit yet. Look
  at proxy `ger_entries` — is there a recent injected row? If not, see
  failure mode E.
- **Bridge-service is `ready_for_claim=true` but no CLAIM has landed on
  the proxy.** Cause: claim sponsor / ClaimTxManager isn't picking the
  deposit up. Check claim sponsor logs in
  `<TODO: confirm aggkit claimsponsor pod name on bali — likely aggkit-0>`.
- **CLAIM submission keeps failing on the proxy.** Cause: failure mode A
  is firing — go fix that first.
- **CLAIM committed on Miden but the user's balance didn't change.**
  Cause: address mapping fell back to zero-padding for an address that
  doesn't exist on Miden (counter
  `address_mapper_zero_padding_fallback_total` incremented). The mint
  happened to a synthesised account that nobody can spend. **Page Max**
  before proceeding — this needs a deliberate decision on whether to
  re-issue.

For the recovery cure that unsticks marti's deposits specifically, the
v0.4.1 redeploy in [`../REDEPLOY_RUNBOOK_BALI.md`](../REDEPLOY_RUNBOOK_BALI.md) is the procedure of
record.

---

## Failure mode C — Stuck L2→L1 withdrawal (no claim landing on L1)

User report: "I burned my Miden balance via a B2AGG note, the L2 logs
show `BridgeEvent`, but I never received ETH on Sepolia."

Trace via [`diagnostics.md` section 5](./diagnostics.md#5-trace-a-single-l2l1-withdrawal).
Possible wedges:

1. **`BridgeEvent` not emitted on L2.** The `BridgeOutScanner`
   (`src/bridge_out.rs`) didn't see the B2AGG note. Check
   `bridge_outs_total` rate. Cross-check the `bridge_out_processed`
   table for the user's deposit_count — if absent, the scanner never
   processed it. Likely cause: the note's faucet isn't in our
   registry (`bridge_out_unknown_faucet_total` incremented) — quarantine
   is by design but the deposit is permanently stuck until the registry
   is updated.
2. **AggLayer certificate not built.** Look in aggsender logs for the
   `BridgeEvent` block — does aggsender pick up the event?
   `<TODO: confirm aggsender pod name and logs path on bali>`.
3. **Certificate built but ClaimSettler hasn't auto-claimed on L1.**
   Inspect ClaimSettler config: `CLAIM_SETTLER_ENABLED=true`,
   `CLAIM_SETTLER_WATCH_ADDRESSES` covers the destination address, the
   signer L1 balance is sufficient.

### C.1 ClaimSettler signer is dry

Signer address is logged at startup as
`ClaimSettler: signing as <address>`. Check balance:

```bash
cast balance <signer_address> --rpc-url "$SEPOLIA_RPC_URL"
```

Top-up procedure: `<TODO: confirm bali claimsponsor funding source with
Max — likely the gateway-treasury 1Password "Sepolia faucet" entry>`.

**Blast radius:** L1 ETH spend, irreversible.

---

## Failure mode D — Bridge invariant violation (Cantina hard-page metrics)

If any of these counters increment, the safe action is to **stop processing
new claims and bridge-outs** until the cause is understood:

- `bridge_burn_serial_collision_total`
- `bridge_twin_note_detected_total`
- `bridge_mint_target_mismatch_total`
- `bridge_faucet_ownership_drift_total{kind=renounced}`
- `bridge_forged_mint_total`

### D.1 Stop the world

There is **no clean "pause" flag**. The least-bad approximation:

```bash
# Scale the StatefulSet to 0 — refuses all JSON-RPC, breaks the
# aggoracle / aggsender / claim sponsor loop until restored.
kubectl -n <namespace> scale statefulset miden-agglayer --replicas=0
```

**Blast radius:** all bridging halts in both directions. Aggsender will
queue events; aggoracle will retry. This is the right action for a
suspected exploit. Do not unwind without explicit go from Max + Igor.

**Rollback:**

```bash
kubectl -n <namespace> scale statefulset miden-agglayer --replicas=1
```

### D.2 Capture evidence

Before any further action, snapshot the proxy DB:

```bash
# <TODO: confirm pg_dump RBAC permissions and S3 destination bucket>
PGPASSWORD="$PROXY_PG_PASSWORD" pg_dump -h 127.0.0.1 -p 15434 \
  -U agglayer -d agglayer_store \
  -F c -f bali-agglayer-store-$(date -u +%Y%m%dT%H%M%SZ).pgdump
```

Snapshot the relevant Loki window:

```logql
{namespace="outpost-testnet-miden-testnet", container="miden-agglayer"}
  |~ "bridge_invariant_violation"
```

Open an incident ticket with the metric name + violation kind + the
NoteId / AccountId mentioned in the WARN/ERROR log line.

---

## Failure mode E — Aggoracle pushed a GER but it didn't land

Symptoms:

- bridge-service log shows aggoracle calling `insertGlobalExitRoot`
  (or `updateExitRoot`) against the proxy,
- proxy `ger_entries` does **not** show a corresponding row, OR shows
  the row with `is_injected=FALSE`.

### E.1 Step 1: which RPC method did aggoracle use?

Check the aggkit config:

```bash
kubectl -n <namespace> get configmap aggkit-config -o yaml | grep UseUpdateExitRoot
```

If `UseUpdateExitRoot = false`, aggoracle is using the legacy
`insertGlobalExitRoot(combinedGER)` path, which **must** be paired with
the L1InfoTreeIndexer to backfill the two roots. Confirm
`L1_RPC_URL` + `L1_GER_ADDRESS` (or `--l1-rpc-url` / `--l1-ger-address`)
are set on the proxy pod. If they aren't, the proxy stores `(NULL, NULL)`
roots permanently (the failure documented in [`../ger-decomposition.md`](../ger-decomposition.md)).

Fix: set the env vars + restart, OR flip `UseUpdateExitRoot = true` and
restart aggkit (preferred — closes the race entirely, see RD-862).

### E.2 Step 2: indexer falling behind?

```bash
kubectl logs <pod> -n <namespace> --tail=2000 \
  | grep 'L1InfoTreeIndexer polled' | tail -5
```

If `head` is many blocks ahead of `to`, the indexer is behind L1. Causes
to consider:

- L1 RPC slow / rate-limited (Infura quota burnt).
- The pod OOMKilled recently — each restart resets the cursor to current
  L1 head and forgets the in-progress window (`<TODO: confirm whether
  the v0.4.0 cursor persistence in migration 005 has fully closed this
  failure mode>`).
- L1 reorg backed up the indexer's reverification logic.

### E.3 Step 3: Miden submission failing?

If the GER row in `ger_entries` has `is_injected=FALSE` and the proxy
keeps trying:

```logql
{namespace="<ns>", container="miden-agglayer"}
  |~ "UpdateGerNote.*failed|NoteScreener|FetchAssetWitnessFailed|RootNotInStore"
```

The NoteScreener bypass design (see [`../ger-note-screening-bypass.md`](../ger-note-screening-bypass.md))
should handle the `RootNotInStore` class. If you see
`FetchAssetWitnessFailed` repeating *after* the split-submit path, the
bypass has regressed — escalate.

---

## Failure mode F — STATE-C orphan backfill (historic poisoned GERs)

The 27 race-poisoned GER rows on bali — `is_injected=TRUE`,
`mainnet_exit_root IS NULL`, `rollup_exit_root IS NULL` — exist from the
pre-`UseUpdateExitRoot` era. They are **not blocking current bridging**
(see ger-decomposition.md: newer GERs supersede older ones), so the
backfill is cosmetic.

To clean them up:

1. Find the earliest orphan's L1 block (the indexer needs to walk back
   to before the orphan was injected to re-emit the matching
   `UpdateL1InfoTree`). Use 30 days before today as a safe lower bound.
2. Patch the StatefulSet with `--l1-indexer-from-block=<N>` and wait for
   the indexer cursor to walk past current head.
3. Remove the flag.
4. Verify: `SELECT count(*) FROM ger_entries WHERE is_injected AND mainnet_exit_root IS NULL;` returns 0.

Full procedure is in
[`../REDEPLOY_RUNBOOK_BALI.md` step 4](../REDEPLOY_RUNBOOK_BALI.md#step-4--clean-up-the-27-historic-state-c-orphans-optional)
and is identical for any cluster.

---

## Failure mode G — Stale account lock

`miden_locked_accounts_detected_total > 0` at startup; no other
divergence symptoms. See section A.1 above for the `--unlock-miden-accounts`
procedure.

---

## Procedures we deliberately do NOT document

These need explicit case-by-case authorisation; copy-paste recovery
would be dangerous.

- **`--init` re-run on a deployed cluster.** Mints new on-chain
  infrastructure accounts and overwrites `bridge_accounts.toml`. Any
  asset balance held by the old accounts is unrecoverable. Only safe on
  greenfield deployments. Max owns the decision.
- **Manual SQL UPDATE against `ger_entries`.** The race-orphan backfill
  is preferred (failure mode F). A direct UPDATE bypasses the proof that
  the (mainnet, rollup) pair is consistent with the combined hash, which
  is the only safety property keeping bridge-service from accepting
  forged roots.
- **PgStore restore from `pg_dump`.** Splices live on-chain state with a
  stale snapshot of synthetic state. Likely to break the deterministic
  hash chain. Use the in-process `--restore` flag instead — it rebuilds
  from authoritative sources.
- **Manual `kubectl exec` against the running miden-agglayer pod to run
  miden-client CLI.** The pod's miden-client sqlite is locked by the
  long-lived `MidenClient` event loop; concurrent access from a CLI
  invocation will corrupt it. Use a separate diagnostic pod or local
  copy via `kubectl cp` of a snapshot.
