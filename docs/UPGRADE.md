# In-place upgrade guide (v0.15.7 → this release)

Operator guide for upgrading a running miden-agglayer proxy **in place** — same store,
same chain, no data loss — plus rollback and re-upgrade. Verified end-to-end by
`scripts/e2e-upgrade-test.sh` (release bringup → upgrade → rollback → re-upgrade, with
live traffic and invariant checks at every step).

## What an upgrade must preserve (the invariants)

1. **No data loss** — the proxy's store (`--miden-store-dir`, deployed as a volume/bind
   mount) carries the synthetic chain, cursors, bridge accounts and the faucet registry.
   After the swap the reconciler must **resume from its persisted cursor** — a reconciler
   log window starting at `from: 1` means a genesis re-sweep, i.e. the store was lost.
2. **getLogs immutability across the swap** — every block exposed before the upgrade must
   serve byte-identical logs after it. aggkit/agglayer re-query historical ranges; a
   changed answer is a consensus-level fault.
3. **Liveness** — the synthetic tip advances and a bridge round-trip completes on the
   new version.

## Prerequisites

- The new proxy image (same entrypoints: `miden-agglayer-service`, `bridge-out-tool`,
  `bridge-autoclaim`).
- **Schema check:** v0.15.7 → this release adds **zero store migrations** (identical
  `migrations/` set), so the store is readable by BOTH versions and **rollback is
  schema-safe**. For future upgrades, repeat this check:
  `diff <(git ls-tree --name-only <old> migrations/) <(git ls-tree --name-only <new> migrations/)`
  — if the new version adds migrations, rollback needs a store backup taken before the
  upgrade instead.
- **CLI flag compatibility:** this release ADDS flags (e.g. the native-faucet set:
  `--create-native-faucet`, `--faucet-reconciler-poll-secs`). **Old binaries reject
  unknown flags and crash-loop** — so the deployment's command line is version-specific.
  Keep the old command line recorded for rollback (see
  `scripts/upgrade/docker-compose.upgrade-release.yml` for the v0.15.7 set).

## Procedure (docker compose deployment)

```sh
# 0. Record the pre-upgrade fingerprint (for the immutability check):
TIP=$(cast block-number --rpc-url $PROXY_RPC)      # or eth_blockNumber
# hash all logs up to $TIP for the three topics (BridgeEvent / ClaimEvent /
# UpdateHashChain) — scripts/e2e-upgrade-test.sh:logs_hash() is a reference impl.

# 1. Point the service at the new image (compose file or image tag), keeping the
#    SAME volumes and the NEW command line, then recreate only the proxy:
docker compose up -d miden-agglayer

# 2. Verify (checklist below). Other services (node, aggkit, bridge-service) keep
#    running; their RPC connections reconnect automatically.
```

## Post-upgrade verification checklist

| Check | How | Expect |
|---|---|---|
| Store resumed | `docker logs <proxy> \| grep "note reconciler"` | windows resume from the pre-upgrade cursor; **no `from: 1`** |
| Tip advances | `eth_blockNumber` twice, 10 s apart | strictly increasing |
| Immutability | re-hash logs over the pre-upgrade range `[0, TIP]` | identical to the pre-upgrade fingerprint |
| Completeness auditor | `synthetic_projector_completeness_missing_total` metric (or absence of `completeness violation` in logs) | present and `0` |
| Traffic | one L1→L2 and one L2→L1 round-trip | both complete |

## Rollback

Same swap in reverse — **with the OLD command line** (the old binary rejects the new
flags):

```sh
docker compose -f <compose> -f <old-command-override> up -d miden-agglayer
```

Because this upgrade introduces no migrations, the store written by the new version is
fully readable by v0.15.7. Re-run the verification checklist (minus the auditor row —
v0.15.7 has no auditor). Features introduced by the new version (native-faucet
bridging) are unavailable while rolled back; already-emitted events remain valid and
immutable.

## Re-upgrade

Identical to the upgrade procedure; verified idempotent by the test (phase U2).

## Reference: the automated test

`scripts/e2e-upgrade-test.sh` runs the full cycle against the e2e stack:

- **R** — fresh deployment ON v0.15.7 (`scripts/upgrade/docker-compose.upgrade-release.yml`
  pins image + release command line) + L1→L2 + L2→L1 round-trips
- **U1** — swap to the new image (same bind-mounted store): asserts no genesis re-sweep,
  identical getLogs hash over the pre-swap range, auditor clean, traffic round-trips
- **RB** — rollback to v0.15.7: same assertions (minus auditor), traffic round-trips
- **U2** — re-upgrade: same assertions, traffic round-trips

Run it from the repo root with both images present
(`miden-agglayer-e2e:v0.15.7`, `miden-agglayer-e2e:latest`).
