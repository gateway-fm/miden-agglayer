# Upgrading a v0.15.2 proxy to the reopen-92 build

Operator guide for the **in-place** upgrade of a running miden-agglayer proxy
from **v0.15.2** to the `reopen-92-synthetic-indexer-redesign` build (PR #94).
The procedure was rehearsed end-to-end on a seeded stack (verdict:
**upgrade-safe**): DB preserved, aggkit/bridge-service/node untouched, history
byte-identical, missed events healed automatically. A second, larger rehearsal
(seed 50 → upgrade → +200) validates the same protocol at scale.

**TL;DR:** swap ONLY the proxy image. Everything else — Postgres, the miden
node stack, aggkit, bridge-service, autoclaim — stays up and untouched. No
manual migrations, no state surgery, no aggkit redeploy.

---

## 0. What this upgrade brings (operator-visible)

| Change | Effect after upgrade |
|---|---|
| SyntheticProjector redesign | sole producer of synthetic blocks/logs (Miden-1:1); `projector_cursor` bootstraps automatically |
| Note-visibility reconciler + direct recovery | bridge-out events the old proxy silently missed are **healed retroactively** (emitted in new blocks; aggkit picks them up forward; stuck exits become claimable with no operator action) |
| `eth_blockNumber` fix | v0.15.2 serves a **frozen** tip (known bug — RD-940 mirror orphaned). It unfreezes immediately on upgrade |
| Single-owner store policy | external tooling must NEVER open the proxy's `store.sqlite3`; use `bridge-out-tool --create-wallet` (own store). WAL is deliberately not set — a `database is locked` line now means the contract was violated |
| Postgres migrations `008` + `009` | applied automatically at first boot (faucet metadata column; projector cursor) |
| New RPC | `eth_syncing`, `web3_clientVersion`, `net_version`; unsupported-method probes log at WARN |

## 1. Pre-flight checklist

- [ ] **Backup Postgres** (`pg_dump` of the agglayer store DB) and the proxy
      data dir (`store.sqlite3*`, `keystore/`, `bridge_accounts.toml`). The
      upgrade doesn't need them; the rollback path does.
- [ ] Confirm exactly **one** proxy replica exists (hard requirement — the
      projector + MidenClient are process-wide singletons).
- [ ] Note current state for later comparison:
      real tip `cast block-number` is UNRELIABLE on v0.15.2 (frozen-tip bug) —
      use `eth_getBlockByNumber("latest", false) | .number`; record
      bridge-service deposit/claim counts and the container IDs of all
      sibling services.
- [ ] Verify no external process opens the proxy's `store.sqlite3`
      (`lsof` / deployment review). If one exists, migrate it to an isolated
      store FIRST — post-upgrade it would violate the single-owner contract.
- [ ] RPC ports should be loopback/private-net bound (internet scanners
      actively probe JSON-RPC; see runbook §hardening).

## 2. Upgrade procedure (proxy-only swap)

```bash
# 1. Point the deployment at the new image (tag/digest of the reopen-92 build)
docker tag <registry>/miden-agglayer:<reopen92-tag> <deployed-image-ref>

# 2. Recreate ONLY the proxy container — no --build, no other services
docker compose up -d --no-deps miden-agglayer
#    (k8s: bump the proxy Deployment image; do NOT touch aggkit/bridge-service)

# 3. Wait healthy (rehearsal: ~7 s)
```

At first boot the proxy will, automatically and in order:
1. Run pending Postgres migrations (`008_faucet_metadata`,
   `009_synthetic_projector`) — idempotent, zero-error on a v0.15.2 store.
2. Bootstrap the projector cursor and start the reconciler's **genesis
   re-sweep** — this is the healing pass. Expect log lines:
   `note reconciler: imported network notes missed by sync`,
   `direct projection recovery`, `late-consumption sweep`.
3. Emit BridgeEvents for any exits the old proxy missed — in **new** blocks
   (history is never rewritten), after which autoclaim claims them normally.

## 3. Post-upgrade verification (10 minutes)

Run in order; all must hold:

```bash
# a) Tip coherence + liveness (the frozen-tip regression check)
./scripts/e2e-rpc-tip-consistency.sh          # PASS = coherent AND advancing

# b) Sibling services untouched
docker ps -q --filter name=aggkit             # container ID unchanged, RestartCount=0
# same for bridge-service / postgres / miden-node

# c) No store-contract violations
docker logs <proxy> | grep -c "database is locked"    # must be 0

# d) Healing converged: LET divergence gap must reach 0 when idle
docker logs <proxy> | grep "Cantina #9" | tail -1     # no line, or gap shrinking to 0

# e) History preserved + events complete (full independent audit)
BRIDGE_ID=<bridge account id 0x…> ALLOW_LATE=1 ./scripts/verify-event-completeness.sh
#    BRIDGE_ID is REQUIRED after an upgrade (the new container never logged the
#    bridge deployment) — read it from bridge_accounts.toml.
#    Gate: missing=0, extra=0. `late` entries are the HEALED events (expected
#    when the old proxy had dropped any) — present, claimable, later block.
```

Also confirm bridge-service claim counts resume advancing (aggkit kept
certifying across the boundary — rehearsal showed stuck pre-upgrade exits
being claimed within minutes of the swap, no redeploy).

## 4. Expected timeline (from the rehearsals)

| Step | Duration |
|---|---|
| Proxy recreate → healthy | seconds |
| Migrations | < 1 s |
| Healing re-sweep (per ~1k Miden blocks of history) | a few minutes; scales with history — chunked 200 blocks/tick, non-blocking, normal traffic continues |
| Full verification | ~10 min |

## 4b. Recommended: enable the writer worker at upgrade time

The large rehearsal surfaced a **pre-existing** (also-in-v0.15.2) race: a
node hiccup can make aggkit's ethtxmanager emit GER txs out of nonce order;
the proxy's R4 replay-guard rejects the early nonce and **aggkit's
ethtxmanager wedges permanently** (observed: no self-recovery in 20 min —
one rejected nonce and it stops sending). This build ships the fix — the
future-nonce wait — but it is only active with the writer worker enabled:

```
AGGLAYER_ENABLE_WRITER_WORKER=true    # + AGGLAYER_WRITER_QUEUE_DEPTH / _TX_TTL defaults
```

Enable it as part of the upgrade (or immediately after verification). Also
eliminates the benign nonce-mismatch log churn from autoclaim bursts.

**If the wedge is hit anyway** (writer worker off): symptom is a deposit
stuck `ready_for_claim=false` while aggkit logs show an R4 nonce rejection
and no sends since. Remedy, in order of preference: (1) replay aggkit's own
already-signed raw txs from its logs via `eth_sendRawTransaction` in nonce
order (safe — identical hashes, no keys involved; rehearsal-verified:
delivery completed within a minute), or (2) restart the aggkit container
(restart ≠ redeploy; state is in its DB).

## 5. Rollback

The upgrade is additive (no destructive schema change), but migrations 008/009
are not auto-reverted. Rollback = redeploy the v0.15.2 image; the extra
column/table are ignored by the old code. Events healed by the new build
remain in the store (they are real, verified exits — keep them). If a rollback
follows a suspected store corruption, restore the pre-upgrade Postgres backup
and the proxy data dir together, then follow runbook §recovery R2.

## 6. Known post-upgrade behaviors (do not alarm)

- **Pre-existing "ready-but-unclaimed" rows in the bridge-service may be
  cosmetic.** Old-proxy versions could synthesize a ClaimEvent late into an
  already-indexed block; the bridge-service cursor never sees it, so its API
  shows the deposit unclaimed forever even though funds were delivered.
  Before alarming, check on-chain truth: the proxy's `claimed_indices` for
  the deposit's global index, and the ClaimEvent receipt. (Rehearsal case:
  deposit "TT6" — delivered pre-upgrade, only the sync row missing.) The
  upgraded proxy cannot create new instances of this class
  (write-before-advance). Follow-up: a ready-unclaimed rescan in the
  bridge-service indexer.

- `note reconciler: import silently dropped consumed notes; attempting direct
  projection recovery` at WARN — this is the workaround for a miden-client
  0.15 defect doing its job (counted in
  `synthetic_reconciler_import_dropped_total`); hundreds per busy hour is
  normal. Investigate only `unverified_consumption` / `missing_not_consumed`.
- Nonce-mismatch churn from autoclaim bursts (benign; eliminated when the
  RD-940 writer worker is enabled).
- The LET-divergence watchdog fires transiently under load; it must converge
  to 0 when idle — sustained idle gap = page (see monitoring.md).
- **A one-time burst of `Cantina MA#3` WARNs** ("no recorded consumer" /
  legacy-emission review), roughly one per pre-upgrade bridge-out. Cause:
  v0.15.2 stored externally-consumed notes WITHOUT consumer metadata, and the
  new build's reclaim gate re-reviews already-processed notes during the
  genesis re-sweep — it can't re-verify who consumed the legacy notes, so it
  surfaces each for operator review instead of silently accepting. Benign
  when (a) the count ≈ pre-upgrade bridge-out volume, (b) those exits have
  settled L1 claims, and (c) the event audit shows `extra=0` (no phantom
  exits). Rehearsal observed 24/24 matching the seed's bridge-outs. These do
  NOT emit events and do not recur after the sweep completes.
