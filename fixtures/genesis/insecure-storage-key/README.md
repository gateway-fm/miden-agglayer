# Insecure storage key

These files hold a deterministic **2-of-3** threshold storage key for the miden
node's encrypted-store feature. The e2e Compose stack runs a SINGLE validator;
the participant-1 share (`secret-share.wire`) is what that validator mounts.

Layout:

- `setup-context.wire`, `public-key-set.wire` — the shared public setup.
- `secret-share.wire` — participant 1's secret share; THE share the
  single-validator Compose stack mounts.

This key is public and must not be used outside tests.

Production bundles must use `miden-validator dkg validate`, which also checks
genesis, the ceremony manifest, and signed transcripts.

## Regenerating

The fixture is derived deterministically by the node's
`bin/validator/src/storage_key.rs` (`tests::values_for`). Run this INSIDE the
pinned node checkout (`$WORK/miden-node-src`), then copy participant 1's share
here under the root filename:

```sh
cargo test -p miden-validator --lib \
  storage_key::tests::write_insecure_storage_key_fixture -- --ignored
# the generator also rewrites setup-context.wire + public-key-set.wire —
# copy ALL three so the fixture never mixes key material
cp scripts/testdata/insecure-storage-key/{setup-context.wire,public-key-set.wire} \
  /path/to/miden-agglayer/fixtures/genesis/insecure-storage-key/
cp scripts/testdata/insecure-storage-key/validator-1/secret-share.wire \
  /path/to/miden-agglayer/fixtures/genesis/insecure-storage-key/secret-share.wire
```
