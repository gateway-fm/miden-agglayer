# Bali v0.4.1 deploy + recovery runbook

Author: max.revitt@gateway.fm
Audience: SRE on call for the bali Miden agglayer testnet
Last-validated: 2026-05-19 against branch `feat/v0.4.0-self-heal`
                (HEAD `d241b0b`).

> **v0.4.1 vs v0.4.0:** code is identical. v0.4.1 is a re-tag that
> drops the moving `:{major}.{minor}` (`:0.4`) tag from the release
> workflow — both v0.4.0 release attempts failed at the Docker Hub
> push because of tag immutability (`:latest` on attempt 1, `:0.4` on
> attempt 2). The published image is `gatewayfm/miden-agglayer:0.4.1`.

## What this runbook is for

Deploying `v0.4.1` of `gatewayfm/miden-agglayer` to the bali production
proxy (`outpost-testnet-miden-testnet/miden-agglayer-0`). v0.4.1 fixes
the regression chain that has held bali's L1→L2 bridge in
`AccountDataNotFound` failure for ~20 wall-days, plus the underlying
race that triggered it. Two stuck deposits (marti's `cnt=1130654` and
`cnt=1131034`) come unstuck on the first aggoracle push after deploy.

## Pre-flight: confirm the state you're about to operate on

The investigation that produced this release was done against this snapshot.
Re-verify these still hold before deploying — if any of them have
changed, stop and re-investigate.

```bash
kubectl -n outpost-testnet-miden-testnet describe pod miden-agglayer-0 \
  | grep -E 'Image|Reason|Started:|Restart Count'
```

Expect: `Image: docker.io/gatewayfm/miden-agglayer:0.2.1`. If it's
already v0.4.1 something is out of band and you should ask Max.

```sql
-- agglayer-store (kubectl port-forward svc/miden-agglayer-db 15434:5432)
SELECT
  count(*) FILTER (WHERE is_injected) AS injected,
  count(*) FILTER (WHERE is_injected AND mainnet_exit_root IS NULL) AS state_c_orphans,
  count(*) FILTER (WHERE NOT is_injected AND mainnet_exit_root IS NOT NULL) AS state_b_pending
FROM ger_entries;
```

Expect ≈ `injected≈65 / state_c_orphans≈27 / state_b_pending>=2`. If
`injected` has grown, something is already pushing successfully and the
incident may have self-resolved.

```sql
-- bridge-db (kubectl port-forward svc/bridge-db 15435:5432)
SELECT deposit_cnt, ready_for_claim FROM sync.deposit
WHERE network_id = 0 AND dest_net = 73 ORDER BY deposit_cnt;
```

Expect `1127628..1127650 ready=true` (3 rows), `1130654 + 1131034
ready=false` (2 rows). marti's `1130654` is one of the stuck.

## Step 1 — build + push v0.4.1 image

The `release.yml` GitHub Actions workflow auto-builds and pushes
`gatewayfm/miden-agglayer:0.4.1` to Docker Hub on tag push. Local
fallback (if CI is unavailable):

```bash
cd ~/github/gateway/miden/miden-agglayer
git checkout main  # after PR #45 merge
docker buildx build --platform linux/amd64 \
  -t gatewayfm/miden-agglayer:0.4.1 \
  --push .
```

Tag the commit:

```bash
git tag -a v0.4.1 -m "v0.4.1 — same code as v0.4.0, re-tagged after dropping moving :major.minor tag from release workflow"
git push origin v0.4.1
```

Image digest from the push output — paste it into the StatefulSet
patch below.

## Step 2 — patch the StatefulSet image tag

```bash
kubectl -n outpost-testnet-miden-testnet patch statefulset miden-agglayer \
  --type='json' \
  -p='[{"op":"replace","path":"/spec/template/spec/containers/0/image","value":"docker.io/gatewayfm/miden-agglayer:0.4.1"}]'
```

Watch the rollout:

```bash
kubectl -n outpost-testnet-miden-testnet rollout status statefulset miden-agglayer --watch
```

Expected logs on the new pod's first boot (`kubectl logs -f`):

```
INFO migrator: applying migration: 005_l1_indexer_cursor.sql
INFO DB migrations complete applied=1 already_present=4
INFO L1InfoTreeIndexer starting
INFO L1InfoTreeIndexer cursor initialized start_block=... stored_cursor=0 l1_head=...
INFO RPC server listening on 0.0.0.0:8546
```

> Note (post-#17 hardening): the binary now defaults to `127.0.0.1`. A
> network-facing deployment like this one must set `--bind 0.0.0.0` (or
> `BIND_ADDR=0.0.0.0`) explicitly — the log line above will then match.

```
... (within ~30s)
INFO GER injection: submitting to Miden... ger: 0x<combined>
WARN GER injection: recoverable account error, reimporting ger_manager and retrying err: account data wasn't found for account id 0xe9a21e616d9ed59016d481c7001393
INFO reimported from node account=ger_manager account_id=0xe9a21e616d9ed59016d481c7001393
INFO UpdateGerNote created note_id=0x...
INFO UpdateGerNote submitted, waiting for commit...
INFO UpdateGerNote transaction committed
```

The `WARN reimporting` line firing once and the subsequent
`UpdateGerNote transaction committed` is the cure event. After that,
aggoracle pushes should succeed normally.

## Step 3 — verify recovery

Wait ~60 seconds, then:

```sql
-- agglayer-store: should see new is_injected=TRUE rows
SELECT count(*) FROM ger_entries WHERE is_injected;
-- Expect: > 65 (whatever it was before, plus at least one new)
```

```sql
-- bridge-db: marti's deposit must now be ready
SELECT deposit_cnt, ready_for_claim FROM sync.deposit
WHERE network_id = 0 AND deposit_cnt IN (1130654, 1131034);
-- Expect: both rows ready_for_claim = t
```

If both are `t`: **the deploy succeeded and the backlog cleared**.
Marti can submit `claimAsset` for his deposit; the claimsponsor will
also pick it up automatically.

## Step 4 — clean up the 27 historic STATE-C orphans (OPTIONAL)

These are race-poisoned GERs from the RD-862 era (proxy blocks 95k-130k
on bali) with `(mainnet_exit_root IS NULL, rollup_exit_root IS NULL,
is_injected = TRUE)`. They are NOT blocking any current user deposits
— marti's came unstuck in Step 3 via a fresh GER, not via these
orphans. Cleaning them up is cosmetic.

If you do want a clean ledger:

```bash
# Find the earliest orphan's approximate L1 block.
# (proxy block_number is synthesized, NOT wall-clock; use aggoracle's
# Sepolia sync logs around the orphan's timestamp instead, or just
# pick a safely-old block — 30 days before today is fine).

kubectl -n outpost-testnet-miden-testnet patch statefulset miden-agglayer \
  --type='json' \
  -p='[{"op":"add","path":"/spec/template/spec/containers/0/args/-","value":"--l1-indexer-from-block=<N>"}]'

# Wait for indexer to walk past current head:
kubectl -n outpost-testnet-miden-testnet logs -f miden-agglayer-0 \
  | grep -E 'L1InfoTreeIndexer batch processed|cursor advanced'

# Once the cursor has caught up to current head, remove the flag:
kubectl -n outpost-testnet-miden-testnet patch statefulset miden-agglayer \
  --type='json' \
  -p='[{"op":"remove","path":"/spec/template/spec/containers/0/args/<index-of-flag>"}]'

# Verify orphans cleared:
psql ... -c "SELECT count(*) FROM ger_entries WHERE is_injected AND mainnet_exit_root IS NULL;"
# Expect: 0
```

## Rollback

If the new image fails to start or behaves worse than v0.2.1:

```bash
kubectl -n outpost-testnet-miden-testnet patch statefulset miden-agglayer \
  --type='json' \
  -p='[{"op":"replace","path":"/spec/template/spec/containers/0/image","value":"docker.io/gatewayfm/miden-agglayer:0.2.1"}]'
```

The new migration (`005_l1_indexer_cursor.sql`) is forward-compatible
with v0.2.1 — it adds a table that v0.2.1 simply doesn't query, so
rolling back leaves the schema as a superset and does not require a
DB downgrade.

The new code path in `init.rs` (`storage_mode = Public`) only affects
**new** account deployments. v0.2.1 will keep working against the
accounts deployed by v0.4.0 (they have a stricter storage mode but the
proxy uses them identically).

## Known scope limits of v0.4.0

These are NOT fixed by this release; they're acknowledged trade-offs
or out-of-scope follow-ups:

1. **Existing bali accounts remain Private.** v0.4.0's
   `storage_mode = Public` only applies to fresh deployments. The
   account IDs already in `bridge_accounts.toml` on bali were created
   pre-`dbe5c2d` and are unrecoverable from a fresh sqlite loss. The
   runtime self-heal mitigates this (it tries `import_account_by_id`
   on every recoverable error) — but for a Private account that call
   returns `AccountIsPrivate`. If bali's sqlite is wiped again **and**
   no other restore path is available, the only recourse is a
   full re-init with new account IDs + an aggkit config rewrite
   pointing at the new IDs. Plan a future maintenance window for
   that swap; until then v0.4.0's heal is your safety net.

2. **STATE-C orphans aren't auto-cured.** The 27 historic
   `(NULL, NULL)` rows need the operator backfill flag in Step 4.

3. **Sepolia archival apiKey rotation.** `RUST_LOG=debug` was leaking
   this key for the lifetime of the deployment. v0.4.0 stops the leak
   going forward, but the already-leaked key should be rotated via the
   secret store (AWS Secrets Manager) as a separate ticket. The new
   value flows in through `miden-agglayer-secret.l1_rpc_url` — no proxy
   redeploy required.

## Loki queries worth running post-deploy

```logql
# Confirm cure event fired
{namespace="outpost-testnet-miden-testnet", pod=~"miden-agglayer.*"}
  |~ "reimporting ger_manager|reimported from node"

# Watch for new failures the heal can't handle
{namespace="outpost-testnet-miden-testnet", pod=~"miden-agglayer.*"}
  |~ "AccountIsPrivate|AccountNotFoundOnChain|account reimport failed"
```

The first one should fire once per pod restart during the heal sequence.
The second one should never fire on bali for the infrastructure accounts;
if it does, run the rollback above and ping Max.
