# miden-agglayer operations runbook

How to **run** the proxy and how to **recover** it. For architecture,
flows, and the recovery-mechanism diagrams, read
[`../ARCHITECTURE.md`](../ARCHITECTURE.md) first — this doc does not
duplicate it. For pre-fix snapshot collection (logs / SQL / pod state),
use [`diagnostics.md`](./diagnostics.md). For what to scrape and alert
on, see [`monitoring.md`](./monitoring.md).

Structure:

- **Part 1 — Running the proxy**: startup surface, hard deployment
  constraints, bootstrap.
- **Part 2 — Recovery**: the three recovery mechanisms (R1 automatic
  ladder, R2 `--restore`, R3 account self-heal) and when each applies.
- **Part 3 — Failure-mode catalogue**: step-by-step procedures for
  specific incidents (bali-era content, still applicable).

---

# Part 1 — Running the proxy

## 1.1 Startup surface

All flags are defined in `src/main.rs` (`struct Command`). The reference
deployment is the `miden-agglayer` service in
[`../../docker-compose.e2e.yml`](../../docker-compose.e2e.yml) — copy
its shape for new environments.

### Core

| Flag | Env | Default | Notes |
|---|---|---|---|
| `--port` | — | `8546` | JSON-RPC HTTP listener. **Bind loopback / private net only** — see §1.2. |
| `--miden-node` | — | `http://localhost:57291` | Miden node gRPC URL, or a network name `devnet` / `testnet`. |
| `--miden-store-dir` | — | `$HOME/.miden` | Directory holding `store.sqlite3`, the keystore, and `bridge_accounts.toml`. **Proxy-private** — see §1.2. |
| `--chain-id` | `CHAIN_ID` | `2` | EVM chain ID for `eth_chainId`. |
| `--network-id` | `NETWORK_ID` | `1` | Rollup network ID from the RollupManager (`networkID()`). NOT the chain id. |
| `--database-url` | `DATABASE_URL` | unset | Postgres connection string. Set ⇒ `PgStore` (durable synthetic chain, required in production); unset ⇒ `InMemoryStore` (everything lost on restart). Migrations run in-process at startup. |
| `--bridge-address` | `BRIDGE_ADDRESS` | built-in default | L1 bridge contract address used for synthetic log emission. |

### L1 indexer (GER exit-root resolution)

| Flag | Env | Default | Notes |
|---|---|---|---|
| `--l1-rpc-url` | `L1_RPC_URL` | unset | L1 RPC for resolving exit roots. Without it, GERs injected via legacy `insertGlobalExitRoot` store `(NULL, NULL)` roots (see [`../ger-decomposition.md`](../ger-decomposition.md)). |
| `--ger-l1-address` | `GER_L1_ADDRESS` | unset | L1 GER contract address the indexer scrapes for `UpdateL1InfoTree`. |
| `--l1-indexer-from-block` | `L1_INDEXER_FROM_BLOCK` | unset | Operator override: force a forward walk from this L1 block on next boot (STATE-C orphan backfill — Part 3, failure mode F). Remove once the cursor has walked past it. |

### Miden proving

| Flag | Env | Default | Notes |
|---|---|---|---|
| `--miden-prover-url` | `MIDEN_PROVER_URL` | unset | gRPC URL of a remote tx-prover. **Set it in production** — in-process local proving is the documented bali OOM cause and `--require-hardening` refuses to start without it. |
| `--miden-prover-timeout-secs` | `MIDEN_PROVER_TIMEOUT_SECS` | `120` | Per-request remote-prover timeout. |
| `--miden-prover-fallback-to-local` | `MIDEN_PROVER_FALLBACK_TO_LOCAL` | `false` | Retry a failed remote proof in-process. Trades OOM safety for availability; default OFF preserves the OOM fix. |
| `--miden-api-key` | `MIDEN_API_KEY` | unset | `authorization: Bearer` header on outbound Miden gRPC (gateway rate-limit bypass). Redacted in logs. |

### Security / hardening

| Flag | Env | Default | Notes |
|---|---|---|---|
| `--admin-api-key` | `ADMIN_API_KEY` | unset | Gates `admin_*` JSON-RPC (Bearer token, constant-time compare). Unset ⇒ `admin_*` rejected entirely. |
| `--allowed-signers` | `ALLOWED_SIGNERS` | unset | Comma-separated signer allow-list for `eth_sendRawTransaction`. Unset = open mode — only safe behind a private network boundary. |
| `--cors-allowed-origins` | `CORS_ALLOWED_ORIGINS` | unset | Omit to disable CORS (safe production default). `*` is DEV ONLY. |
| `--rate-limit-per-second` / `--rate-limit-burst` | `RATE_LIMIT_PER_SECOND` / `RATE_LIMIT_BURST` | `500` / `500` | Per-IP rate limit. |
| `--reject-zero-padding-addresses` | `REJECT_ZERO_PADDING_ADDRESSES` | `false` | Refuse the address-mapper zero-padding fallback (production posture). |
| `--disable-hardhat-alias` | `DISABLE_HARDHAT_ALIAS` | `false` | Refuse the well-known Hardhat address remap (Cantina MA#8). **MUST be set in production.** |
| `--require-hardening` | `REQUIRE_HARDENING` | `false` | Startup invariant: refuse to boot unless `ADMIN_API_KEY`, `ALLOWED_SIGNERS`, `MIDEN_PROVER_URL`, `DISABLE_HARDHAT_ALIAS` are set and CORS is not `*`. Set it on any internet-adjacent deployment. |
| `--miden-debug` | `MIDEN_DEBUG` | `false` | Verbose Miden VM traces. Disable in production. |

### Writer worker (RD-940)

| Flag / env | Default | Notes |
|---|---|---|
| `--enable-writer-worker` / `AGGLAYER_ENABLE_WRITER_WORKER` | `false` | Async `eth_sendRawTransaction` dispatch. Runtime toggle; see the RD-940 section at the end of this doc for the flag-flip procedure and restart-loss contract. Also enables the bounded future-nonce wait (out-of-order submissions wait up to 30 s for the missing nonce instead of erroring). |
| `AGGLAYER_WRITER_QUEUE_DEPTH` (env only) | `64` | mpsc capacity. |
| `AGGLAYER_WRITER_TX_TTL` (env only) | `300` | Seconds before a stuck job is forced to Failed. |
| `AGGLAYER_CLAIM_RECEIPT_EXPIRATION_BLOCKS` (env only) | `120` | Miden-block lifetime of pending claim receipts waiting for the projector to observe claim-note consumption. |

### One-shot / recovery flags

Never present in steady state (a pod showing any of these in `Args` is
mid-recovery):

| Flag | Effect |
|---|---|
| `--init` | Greenfield bootstrap: mints on-chain infrastructure accounts, writes `bridge_accounts.toml` into `--miden-store-dir`. **Never re-run on a deployed cluster** (Part 3, "Procedures we deliberately do NOT document"). |
| `--restore` | Rebuild the proxy store (synthetic events, GER set, deposit_count) from the Miden node, then exit. See §R2. |
| `--reset-miden-store` | Wipe `store.sqlite3` (+ WAL/SHM) before starting. Keystore and `bridge_accounts.toml` are preserved. Combine with `--restore`. See §R2. |
| `--unlock-miden-accounts` | Clear the `locked` flag on every miden-client account row, then exit. See §R3. |

### Environment requirements

- **`TMPDIR` must be on the same device as `--miden-store-dir`.** rusqlite
  (and rocksdb) perform atomic renames from `TMPDIR` into the store dir;
  a cross-device `TMPDIR` fails at startup with
  `Invalid cross-device link`. The compose file sets
  `TMPDIR: /var/lib/miden-agglayer-service/tmp` inside the same bind
  mount for this reason. Named docker volumes trip the same error — use
  bind mounts.
- `RUST_LOG=info` is the operational baseline; the health line and all
  recovery chatter documented in `monitoring.md` are INFO/WARN.

## 1.2 Hard deployment constraints

### Exactly ONE proxy replica

The proxy **cannot be horizontally scaled**. Two structural reasons:

1. **`MidenClient` is a process-wide singleton** — the single owner of
   `store.sqlite3`. All Miden work (sync, claims, GER, proving)
   serializes through its one event loop. An in-process guard refuses a
   second client (`"a MidenClient is already live — MidenClient must be
   a process-wide singleton"`), but nothing stops a second *replica*
   from opening its own copy of the store and submitting conflicting
   transactions from the same bridge accounts (nonce/mempool conflicts,
   the postmortem's IAIC class).
2. **The `SyntheticProjector` is the sole producer** of the synthetic
   chain (one synthetic block per Miden block, write-before-advance).
   Two projectors against one Postgres would race the cursor and the
   tip; two projectors against two stores would serve two diverging
   chains to aggkit.

Deploy as a StatefulSet/deployment with `replicas: 1` and no HPA. If an
orchestrator ever runs two pods concurrently against the same
`--miden-store-dir`, expect `database is locked` in the logs (must be
0 — see `monitoring.md`).

### The miden store + keystore are proxy-private

`store.sqlite3`, the keystore, and `bridge_accounts.toml` under
`--miden-store-dir` belong to the proxy process **only**:

- **External B2AGG wallets must NEVER share `store.sqlite3`.** This is
  both the production topology (withdrawing users run their own wallet
  client with their own store and keys — see the external-wallet lane in
  `ARCHITECTURE.md`) and the DB-lock isolation result: any tool opening
  the proxy's sqlite concurrently contends with the live `MidenClient`
  and produces `database is locked` / corruption. The prod-faithful
  loadtest (`scripts/e2e-bridge-loadtest-isolated.sh`) exists precisely
  to prove the proxy generates zero lock errors when nothing external
  touches its store.
- Same rule for humans: **never run `miden-cli` against the live store**
  (Part 3, "Procedures we deliberately do NOT document"). Snapshot-copy
  the file first.

### Bind RPC to loopback / private networks

The JSON-RPC port must not be exposed to the internet. A host that
briefly bound `0.0.0.0` received continuous wallet-scanner probes
(`web3_clientVersion`, `parity_netPeers`, `debug_*` — visible as
`JSON-RPC unsupported method` error lines in the logs). The compose file
binds `127.0.0.1:8546:8546`; keep that shape, or front the port with a
private network / firewall. If any non-trusted network can reach the
port, set `--require-hardening` plus `ALLOWED_SIGNERS` and
`ADMIN_API_KEY` — the rate limiter alone is not an exposure strategy.

## 1.3 Bootstrap (greenfield only)

One-time, per fresh environment:

```bash
# creates + funds the bridge / ger_manager / faucet accounts on Miden and
# writes bridge_accounts.toml into --miden-store-dir
miden-agglayer-service --init \
  --miden-node=<grpc-url> \
  --miden-store-dir=<dir> \
  [--miden-prover-url=<prover>]
```

Then start normally (without `--init`). The compose e2e stack does this
automatically. Re-running `--init` on an existing deployment mints new
accounts and orphans all balances held by the old ones — see Part 3.

---

# Part 2 — Recovery

Three mechanisms at three layers. Diagrams and full rationale:
[`../ARCHITECTURE.md` → "Recovery flows"](../ARCHITECTURE.md#recovery-flows).

| | Mechanism | Trigger | Operator action |
|---|---|---|---|
| **R1** | Live note-recovery ladder (reconciler + late sweep + direct recovery) | Automatic, every projector tick | **None** |
| **R2** | `--restore` / `--reset-miden-store --restore` | Operator, after data loss / store divergence | Run the one-shot, restart |
| **R3** | Account self-heal (`import_account_by_id` + retry) | Automatic, per failed submission | None (escalate only if it loops) |

## R1 — Live recovery ladder (automatic — needs NO operator action)

Externally-created network notes (B2AGG bridge-outs) that are committed
*and* consumed between two proxy sync points are invisible to
miden-client's interest-based sync. The projector heals this in-process,
every tick, with three escalating catchers (late-consumption sweep →
note reconciler → direct spent-before-import recovery).

**What its chatter means** — these logs/metrics are the ladder *working*,
not failing:

| Signal | Meaning | Action |
|---|---|---|
| `note reconciler: imported network notes missed by sync` + `synthetic_reconciler_notes_imported_total` | Catcher 2 back-filled notes the sync missed. Normal background healing. | None |
| WARN `note reconciler: import silently dropped consumed notes; attempting direct projection recovery` + `synthetic_reconciler_import_dropped_total` | miden-client 0.15 silently drops imports of already-spent notes; the ladder escalates to catcher 3 automatically. WARN-level **normal**. | None |
| `spent-before-import recovery: bridge-consumed B2AGG verified via sync_transactions` + `synthetic_reconciler_direct_recovered_total` | Catcher 3 recovered a fast-consumed note with on-chain consumer proof. | None |
| `late-consumption sweep: projecting notes discovered after their block` | Catcher 1 projected a note into the first unexposed block. Consumers cannot have skipped it (write-before-advance). | None |
| WARN `... consumed B2AGG was NOT consumed by any bridge transaction at its spend block` + `synthetic_reconciler_unverified_consumption_total` | MA#3 fail-closed gate: consumption exists but the bridge did not execute it — sender reclaim or unknown consumer. **No BridgeEvent is emitted on purpose.** | **Investigate** (see `monitoring.md`) |
| ERROR `... note missing from store but its nullifier is unspent` + `synthetic_reconciler_missing_not_consumed_total` | A note the reconciler expected is neither imported nor consumed. Restart re-sweeps from genesis and retries. | **Investigate** if it repeats |

The ladder is idempotent and retry-safe: a restart re-sweeps from
genesis (known ids are skipped), so transient failures self-heal on the
next boot. The end-to-end health line is
`synthetic projector tick: caught up to Miden tip` with
`miden_tip == projector_cursor == synthetic_tip` (see `monitoring.md`).

## R2 — Startup restore (operator-driven disaster recovery)

Rebuilds the proxy store (synthetic events, GER set, hash chain,
deposit_count) from the Miden node. Runs as a **one-shot**: the process
replays and exits; you then start the proxy normally.

**When to use which:**

| Situation | Command |
|---|---|
| Proxy store (Postgres) lost/corrupt; miden-client sqlite fine | `--restore` |
| miden-client sqlite diverged/corrupt (AccountDataNotFound after R3 failed, structural divergence, disk loss) | `--reset-miden-store --restore` |
| Only symptom is a stale account lock | Don't — use `--unlock-miden-accounts` (§R3) |

**Exact commands** (compose deployment; k8s variant in Part 3, failure
mode A.2):

```bash
# 1. Stop the running proxy (never run restore concurrently with it).
docker compose -f docker-compose.e2e.yml --env-file fixtures/.env stop miden-agglayer

# 2. One-shot. `docker compose run` overrides the command, so re-supply
#    the base flags your deployment uses.
docker compose -f docker-compose.e2e.yml --env-file fixtures/.env \
  run --rm --no-deps miden-agglayer \
  --port=8546 \
  --miden-node=http://miden-node:57291 \
  --miden-store-dir=/var/lib/miden-agglayer-service \
  --l1-rpc-url=http://anvil:8545 \
  --ger-l1-address=<ger-l1-address> \
  --reset-miden-store \
  --restore

# 3. Expected log markers, in order:
#    reset_miden_store: deleted .../store.sqlite3       (only with --reset-miden-store)
#    === RESTORE: starting state reconstruction ===
#    Phase 0: re-importing bridge accounts from Miden node...
#    reimported from node                                (per network-tracked account)
#    Phase 1: sync ...
#    === RESTORE: complete ===
#    then the process exits 0.

# 4. Start the proxy normally.
docker compose -f docker-compose.e2e.yml --env-file fixtures/.env start miden-agglayer
```

The operator-faithful rehearsal of this procedure is
`scripts/e2e-reset-restore-recovery.sh`.

**What survives:**

- The **keystore** and **`bridge_accounts.toml`** — explicitly preserved
  by `--reset-miden-store` (only `store.sqlite3` + WAL/SHM are deleted).
- The Postgres store's dedup keys — replay is idempotent, existing
  events are not duplicated.
- All on-chain state (obviously).

**Caveat — R2 replays the LOCAL view.** Restore's replay reads the
consumed-note view that sync can deliver; notes that were invisible to
sync (the fast-consumption class) are *not* recovered by the replay
itself. That is fine: after startup, **R1's reconciler re-sweeps from
genesis and heals the remainder** automatically. Expect a burst of
`synthetic_reconciler_notes_imported_total` /
`synthetic_reconciler_direct_recovered_total` in the first minutes after
a restore — that is the system finishing the job, not a new problem.

**Phase 0 requirement:** account reimport works only for accounts
deployed `Public` (all infra accounts deployed by current `--init` are).
`Private` legacy accounts fail reimport with `AccountIsPrivate` — see
Part 3, failure mode A.2 caveat.

## R3 — Account self-heal (automatic)

On `AccountDataNotFound` / `IncorrectAccountInitialCommitment` during a
claim or GER submission, the proxy re-imports the affected account's
live public state from the node (`import_account_by_id`) and retries the
submission once. Watch `miden_account_reimport_total{account,outcome}`
and the paired logs `reimported from node` / `account reimport failed`.

- One firing per incident is the mechanism working. **No action.**
- Repeated firings for the same account in steady state = chronic
  divergence → escalate to R2 (`--reset-miden-store --restore`).

Related surgical tool — stale lock only:

```bash
# one-shot: clears the `locked` flag on every managed account row, exits.
miden-agglayer-service --unlock-miden-accounts --miden-store-dir=<dir>
```

Use when `miden_locked_accounts_detected_total > 0` at startup and there
is no other divergence symptom (Part 3, failure mode A.1 has the k8s
procedure).

---

# Part 3 — Failure-mode catalogue

Step-by-step recovery procedures for the failure modes we know about,
originally written for the bali cluster. Section A's *symptoms* are now
largely pre-emptied by R3 (self-heal) — reach for these procedures when
the automatic mechanisms have demonstrably failed.

## How to use this catalogue

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

> **R3 first:** on current builds the runtime self-heal
> (`miden_account_reimport_total`) fires on exactly these errors and
> retries once. A single firing that clears the symptom needs no
> operator action. Proceed below only when the reimport itself fails or
> the symptom recurs.

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
local sqlite has structurally diverged. This is recovery mechanism
**R2** (Part 2) — the compose-shaped command lives there; the k8s
procedure follows.

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

**Post-restore:** expect a burst of R1 reconciler activity
(`synthetic_reconciler_notes_imported_total`,
`synthetic_reconciler_direct_recovered_total`) — the replay only covers
the local sync view; the ladder's genesis re-sweep heals the
fast-consumed remainder. See Part 2, §R2.

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
   timestamp bucket — that's the regression signature. A second replica
   or an external tool sharing the miden store produces the same
   signature — re-check the §1.2 constraints before blaming the code.
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

1. **`BridgeEvent` not emitted on L2.** On projector-era builds the
   event is emitted by the `SyntheticProjector` (with the R1 recovery
   ladder catching fast-consumed notes), so a genuinely missing
   BridgeEvent is now rare and *audit-able*: run
   [`scripts/verify-event-completeness.sh`](./diagnostics.md#10-event-integrity-audit--verify-event-completenesssh)
   to prove presence/absence against the node DB. If it is missing,
   check `synthetic_reconciler_unverified_consumption_total` (MA#3
   fail-closed reclaim gate — intentional non-emission) and
   `bridge_out_unknown_faucet_total` (faucet not in registry —
   quarantined by design; permanently stuck until the registry is
   updated).
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

(Never scale back above 1 — see §1.2.)

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
`L1_RPC_URL` + `GER_L1_ADDRESS` (or `--l1-rpc-url` / `--ger-l1-address`)
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

---

# RD-940 writer worker — operational scenarios

Last updated: 2026-05-27 (RD-940 rollout).

For incident scenarios touching the bridge as a whole, also consult
`docs/POSTMORTEM_2026-05-11_IAIC_TO_ADNF.md` and the linked Cantina
audit notes.

## Failure mode I — forever-pending tx after restart (RD-940)

**Symptom:** `eth_getTransactionReceipt(hash)` returns JSON `null`
indefinitely for a hash that `eth_sendRawTransaction` previously
returned. aggkit's ethtxmanager polls the receipt forever and the tx
never transitions to Committed or Failed.

**Cause:** The writer worker (`AGGLAYER_ENABLE_WRITER_WORKER=true`) keeps
in-flight WriteJobs in a bounded `tokio::sync::mpsc(64)` channel + a
DashMap inflight cache. **There is no on-disk durable queue in v1.**
When the proxy restarts via SIGKILL, k8s OOM-kill, or host eviction,
every job that hadn't yet been `txn_commit`-ed to the store is lost. The
tx-hashes we returned to callers are not recoverable; the work must be
re-submitted by the caller.

A graceful SIGTERM shutdown will:
1. signal the worker to stop accepting new dispatches,
2. wait up to 20 s for in-flight Miden round-trips to complete,
3. snapshot the count of still-non-terminal jobs to
   `/tmp/agglayer-writer-queue-snapshot`, and
4. emit `agglayer_writer_drain_outcome_total{outcome=partial}`.

On the next boot we read that snapshot and increment
`agglayer_writer_dropped_on_restart_total` by the count. **This counter
is the v1 tripwire — every increment is real, unrecovered work.** Hard
page on `increase(agglayer_writer_dropped_on_restart_total[1h]) > 0`.

A SIGKILL leaves the tmpfile absent — the counter stays at 0. Combined
with the `agglayer_writer_queue_depth` history just before the kill,
that's still enough to size the loss window.

### Response

1. **Identify the lost cohort.** Cross-reference
   `agglayer_writer_queue_depth` for the 30 s window before the
   restart against the proxy's structured logs
   (`target=writer_worker::job` events with `kind` and `signer` fields).
   Any hash that appears in a `writer_worker: job committed` or
   `writer_worker: job failed` log line before the restart was already
   terminal in the store; those are not lost.
2. **Notify the affected callers.** Today this is aggoracle (the only
   on-proxy signer in aggkit's stack — see
   `docs/design/RD-940-async-writer.md` Spec E). aggoracle's
   ethtxmanager will eventually time out at its `WaitTxToBeMined = 2 m`
   threshold and re-broadcast; the tx-hash dedup early-return
   (`service_send_raw_txn.rs`) ensures the re-broadcast is idempotent
   if it lands within the receipt's lifetime, otherwise it gets a
   fresh nonce and proceeds normally.
3. **Document each incident.** Increments of
   `agglayer_writer_dropped_on_restart_total` must be triaged into
   Linear under RD-940 follow-up so the v1.5 durable-queue prioritisation
   stays honest.

### Resolution roadmap

v1.5 (RD-940 follow-up) lands a `worker_jobs` table or WAL-style journal.
`WriteJob` already implements `Clone` + carries an ULID `job_id` so the
on-disk shape is additive. Until then, **every accepted tx hash is at
risk of being lost on restart** — operators must treat this as the
explicit contract.

## Coordinated downstream change — k8s `terminationGracePeriodSeconds`

The graceful drain path in `main.rs` waits up to **20 s** before
snapshotting residual jobs. Kubernetes' default
`terminationGracePeriodSeconds = 30 s` includes the time between the
SIGTERM and the SIGKILL that follows; with axum's own shutdown delay
plus the 20 s drain plus a small buffer, **bali's pod spec MUST set
`terminationGracePeriodSeconds: 45`** before `AGGLAYER_ENABLE_WRITER_WORKER`
is flipped to `true` in production.

This change lives in the downstream `gateway-deploy` repo (not in
miden-agglayer). Coordinate the deploy-spec edit with the agglayer
flag-flip; rolling them out in lockstep avoids a window where the drain
is silently truncated by SIGKILL.

There is no HPA or PDB on the miden-agglayer pod today, so the bump has
no cascading effect.

## Flag-flip procedure (enabling the writer worker)

1. **Pre-flight checks.**
   - `kubectl get deploy/miden-agglayer -o yaml` shows
     `terminationGracePeriodSeconds: 45`.
   - Latest miden-agglayer build includes the RD-940 commits.
   - Prometheus is scraping the new metrics
     (`agglayer_writer_queue_depth` should be present even when the
     flag is off — registered unconditionally).
   - Alerts in `monitoring.md` are armed.
2. **Set the env var.** `AGGLAYER_ENABLE_WRITER_WORKER=true` in the
   bali deployment spec. Restart the pod.
3. **First 10 minutes — eyes on the dashboard.**
   - `agglayer_writer_queue_depth` should stay well under 0.5 × cap
     (32 with the default cap of 64).
   - `agglayer_writer_job_failures_total{reason="ttl"}` should stay 0.
     Any non-zero rate means a Miden submission is stuck longer than
     `AGGLAYER_WRITER_TX_TTL` (default 300 s).
   - `agglayer_writer_dropped_on_restart_total` must be 0 (this is the
     first boot under the flag; non-zero here means the previous boot
     was already running the worker and left residue).
4. **First 24 hours.** Compare `agglayer_writer_job_duration_seconds`
   p99 against aggkit's `WaitTxToBeMined` budget (2 m). Stay below 60
   s; alert if the p99 climbs above 90 s for 10 min.
5. **Rollback.** Set `AGGLAYER_ENABLE_WRITER_WORKER=false` and restart.
   The proxy reverts to the legacy synchronous handler with zero code
   change. The flag is a runtime toggle, not a build feature.

## Environment-variable overrides

| Env var | Default | Effect |
|---|---|---|
| `AGGLAYER_ENABLE_WRITER_WORKER` | `false` | Master toggle for the RD-940 async path. Also enables the bounded future-nonce wait (30 s) that absorbs out-of-order autoclaim submissions. |
| `AGGLAYER_WRITER_QUEUE_DEPTH` | `64` | mpsc capacity. At 64 + p50 commit ≈ 10 s, sustainable throughput tops near 6 jobs/s. Bump if `queue_full_rejections` rate climbs. |
| `AGGLAYER_WRITER_TX_TTL` | `300` (5 min) | Seconds before the TTL sweeper forcibly transitions a stuck non-terminal hash to Failed + writes a `status:0x0` receipt. Sits inside aggkit's `WaitTxToBeMined = 2 m` with margin. |
| `AGGLAYER_CLAIM_RECEIPT_EXPIRATION_BLOCKS` | `120` | Miden-block lifetime for pending claim receipts that wait for the projector to observe claim-note consumption. Increase if valid claims expire before projection under load. |
