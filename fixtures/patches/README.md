# Local patches for the e2e images

## zkevm-bridge-service (`zkevm-bridge-service/`)

`run-all.sh` clones upstream `0xPolygon/zkevm-bridge-service` at
`BRIDGE_SVC_REF` (currently `v0.6.4-RC4`, commit-pinned via
`BRIDGE_SVC_COMMIT`), applies these with `git am`, and builds
`zkevm-bridge-service:<ref>-pendingbridges`. Neither change is upstream as of
`v0.6.4-RC4`:

1. **`0001` — already-claimed check disambiguated by source rollup.** Without
   it, `/pending-bridges` treats a deposit as claimed when a claim with the
   same deposit count exists from a DIFFERENT source rollup, which breaks
   L2<->L2 (two origins share deposit counts).
2. **`0002` — autoclaim `SourceNetworkID`.** Lets a sponsor serve exactly one
   source network, so the L1 and L2B sponsors do not race each other.

Both previously lived on the `revitteth/zkevm-bridge-service`
`fix/pending-bridges-rollup-disambiguation` branch (on top of `v0.6.4-RC2`);
they apply cleanly to `v0.6.4-RC4`, and `0002`'s unit test passes there. To
rebase onto a newer tag: cherry-pick them, `git format-patch`, bump
`BRIDGE_SVC_REF`/`BRIDGE_SVC_COMMIT`.

## Miden node images

**None.** The `miden-node` / `miden-validator` / `miden-ntx-builder` /
`miden-remote-prover` e2e images are built from a verified, UNMODIFIED clone of
`0xMiden/node` at the ref pinned in the `Makefile` (`MIDEN_NODE_GIT_REF`,
commit-pinned via `MIDEN_NODE_GIT_COMMIT`). `run-all.sh` refuses a dirty build
worktree and refuses a node ref older than `v0.16.0-rc.4` (see below).

## History (resolved at node `v0.16.0-rc.4`)

- **ntx-builder remote-prover timeout `sed` patch** (10s -> 180s, applied
   in-place by `run-all.sh`; issue #180) — upstream hardcoded
   `DEFAULT_PROVER_TIMEOUT` with no flag, and bridge CLAIM-consumption proofs
   routinely exceed 10s against a remote prover. **Fixed upstream at
   `v0.16.0-rc.4`** (0xMiden/node#2527 / #2537): the timeout is the
   `--tx-prover.timeout` flag, which `docker-compose.e2e.yml` sets to `180s`.
   The remaining inline `from_secs(10)` in `actor/mod.rs` is inside a
   `#[cfg(test)]` constructor and never runs in the binary.

## History (resolved at node `v0.15.0`)

Two local patches were carried while the e2e stack tracked the pre-release node
rev `6649a4ce` (protocol 0.15.0 / VM 0.23.1). Both are obsolete at the `v0.15.0`
tag and were removed:

1. **`0001-node-store-callback-vault-key.patch`** — fixed a store bug where the
   partial account-delta path keyed fungible-balance lookups by *faucet id*
   instead of the full `AssetVaultKey`, so assets from callbacks-enabled faucets
   (the AggLayer faucets) underflowed on `apply_block` and every L2→L1 bridge-out
   was silently dropped. **Fixed upstream at `v0.15.0`:** the buggy
   `select_vault_balances_by_faucet_ids` is replaced by
   `select_vault_balances_by_vault_keys`, which keys by the full vault key.

2. **`vendor-miden-agglayer` + Cargo.lock alignment** — the pre-release rev built
   against base crates 0.15.0/VM 0.23.1 while our service resolved 0.15.2/0.23.3,
   so B2AGG MAST roots diverged; and protocol 0.15.0 hardcoded `MIDEN_NETWORK_ID
   = 77` in the MASM, which clashed with the network-1 L1 fixture. **Both gone at
   `v0.15.0`:** the node now builds against base crates **0.15.3** (matching our
   service, so MAST roots agree natively), and protocol **0.15.3** makes the
   AggLayer network id a per-account **runtime storage slot** set at
   bridge-account creation — so no MASM patch is needed and the network id is
   chosen at genesis/init time.
