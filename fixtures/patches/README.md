# Local patches for the 0.15 e2e node images

Patches applied on top of `0xMiden/node` @ `6649a4ce774bc842c08e6bdc314f6ddafb816282`
(the rev pinned in the Makefile) before building the `miden-node` / `miden-validator`
docker images for the e2e stack. Drop each patch once the upstream fix ships.

## 0001-node-store-callback-vault-key.patch

**Bug (exists at upstream `main` as of 2026-06-10, reported to the Miden team):**
the store's partial account-delta path keys its fungible-balance bookkeeping by
*faucet id* instead of the full `AssetVaultKey`:

- `crates/store/src/db/models/queries/accounts/delta.rs` —
  `select_vault_balances_by_faucet_ids` reconstructs lookup keys via
  `FungibleAsset::new(faucet_id, 0).vault_key()`, which defaults
  `AssetCallbackFlag::Disabled`. Assets from callbacks-**enabled** faucets (the
  AggLayer faucets register with `callbacks=true`) are stored under the
  callbacks-enabled vault key, so the lookup misses and returns balance 0.
- `crates/store/src/db/models/queries/accounts.rs` (`prepare_partial_account_update`)
  then computes `prev_asset.sub(delta)` = `0 - amount` → `AssetError` underflow →
  `apply_block` fails (`upsert_accounts ... error: asset error`) → the block is
  rebuilt without the tx and the tx is silently dropped (the submitting client only
  ever sees a successful `SubmitProvenTx`).

**Observed effect:** every L2→L1 bridge-out (a wallet spending its bridged-in,
callbacks-enabled asset into a B2AGG note) was accepted into the mempool but never
committed. Bridge-in is unaffected (the wallet's first state materialization ships
full account state, which takes the non-delta path).

**Fix:** key the balance lookup and the reconstructed `FungibleAsset`s by the
delta's own `AssetVaultKey` (which carries the callback flag), preserving the flag
with `.with_callbacks(vault_key.callback_flag())`.

The patch also carries a **Cargo.lock alignment**: the node rev locks
`miden-agglayer/protocol/standards 0.15.0` + VM family `0.23.1`, while the client
ecosystem (miden-client 0.15 branch, our service) resolves `0.15.2` + `0.23.3`.
The bridge-out MASM differs between 0.15.0 and 0.15.2, so notes built with 0.15.2
reference procedure MAST roots the 0.15.0-built bridge/executor doesn't have —
the bridge's B2AGG consumption fails with `procedure with root digest 0x… could
not be found` (CLAIM happens to be unchanged, which masks the skew on bridge-in).
The lock update pins the node to the same crate set (`cargo update -p … --precise
0.15.2 / 0.23.3`), restoring MAST-root agreement. Mirrors the 0.14-era
BurnNote-root lockstep rule documented in the service Cargo.toml.

## Second finding for upstream (version-skew, not a code bug)

The 0.15 node rc revs should exact-pin (or release in lockstep with) the
`miden-agglayer` base crate: any patch-version drift between the node build and
client-side note builders changes MASM MAST roots and silently breaks
note consumption across the network boundary.

## Third finding for upstream (deployment topology, question raised)

Protocol 0.15 hardcodes `MIDEN_NETWORK_ID = 77` in the agglayer MASM
(`asm/agglayer/common/constants.masm`; the Rust constant is generated from it
at build time). The local L1 fixture (kurtosis CDK snapshot) registers the
Miden rollup as **network 1** on the RollupManager, and the AggLayer
pessimistic proof rejects certificates whose imported exits carry a different
destination network (`InvalidImportedBridgeExit { InvalidExitNetwork }`) —
while the unpatched MASM rejects network-1 claims with
`ERR_CLAIM_LEAF_DESTINATION_NETWORK_MISMATCH`. No single network id satisfies
both stock ends.

**Local workaround:** `vendor-miden-agglayer/` (in this repo AND in the local
node checkout — the two copies MUST stay byte-identical, or note-script MAST
roots diverge from the genesis bridge) is the published miden-agglayer 0.15.2
with `MIDEN_NETWORK_ID = 1`, wired in via `[patch.crates-io]` in both
Cargo.tomls. The genesis `.mac` files under `fixtures/genesis/` are regenerated
from the patched crate (`cargo check -p miden-node-store` in the node checkout
runs the build.rs generator, then copy the samples here). Drop the vendored
copies and regenerate genesis once the Miden team's 0.15 deployment reconciles
the on-chain network id with the MASM constant.

To regenerate after editing the node checkout: `git -C <node-clone> diff > 0001-node-store-callback-vault-key.patch`
