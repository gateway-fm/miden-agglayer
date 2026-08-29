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

The fixture is derived deterministically by `bin/validator/src/storage_key.rs`
(`tests::values_for`). Regenerate it with:

```sh
cargo test -p miden-validator --lib \
  storage_key::tests::write_insecure_storage_key_fixture -- --ignored
```
