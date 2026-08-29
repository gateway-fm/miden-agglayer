# Insecure storage key

These files hold a deterministic **2-of-3** threshold storage key for the miden
node's encrypted-store feature. The e2e Compose stack runs a SINGLE validator;
the participant-1 share (`secret-share.wire`, identical to
`validator-1/secret-share.wire`) is what that validator consumes. The
`validator-{1,2,3}/` directories and the 2-of-3 combiner requirements date from
the older three-validator topology and are kept only so multi-validator
tooling (e.g. the benchmark smoke test) keeps working unchanged.

Layout:

- `setup-context.wire`, `public-key-set.wire` — the shared public setup.
- `secret-share.wire` — participant 1's secret share; THE share the
  single-validator Compose stack mounts.
- `validator-{1,2,3}/secret-share.wire` — the three participant shares
  (participant 1's is duplicated at the top level).

This key is public and must not be used outside tests.
bootstrapped. This fixture-only check binds the secret share to its expected participant index. Production bundles must
use `miden-validator dkg validate`, which also checks genesis, the ceremony manifest, and signed transcript.

## Regenerating

The fixture is derived deterministically by `bin/validator/src/storage_key.rs` (`tests::values_for`). Regenerate it
with:

```sh
cargo test -p miden-validator --lib \
  storage_key::tests::write_insecure_storage_key_fixture -- --ignored
```
