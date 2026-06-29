# Local patches for the 0.15 e2e node images

**None currently required.** The `miden-node` / `miden-validator` e2e images are
built from a clean clone of `0xMiden/node` at the ref pinned in the `Makefile`
(`MIDEN_NODE_GIT_REF`), with no patches applied.

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
