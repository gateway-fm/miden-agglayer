# Code-health tooling

This page documents the checks that are present on `main`. It is a reference,
not a tooling roadmap.

## Pull-request CI

`.github/workflows/check.yml` runs one check job for pull requests and pushes to
`main` or `master`:

1. `make install-tools`
2. `make lint`
3. `make build`
4. `make test-unit`
5. `make test-scripts`

The crate declares Rust **1.93** as its minimum supported version in
`Cargo.toml`. The repository does not pin a rustup toolchain, so developers and
CI must select a compatible toolchain themselves.

## What the commands check

| Command | Current checks | Extra requirements |
|---|---|---|
| `make lint` | Rust formatting, TOML formatting, spelling and Clippy with warnings denied | `rustfmt`, `clippy`, Taplo and `typos` |
| `make build` | All workspace binaries with the default feature set | None beyond Rust dependencies |
| `make test-unit` | Workspace library tests with the default feature set | No Docker |
| `make test-scripts` | Shell syntax and the release-acceptance guard harness | Bash |
| `make test-nextest` | The same library-test scope through `cargo-nextest` | `make install-tools` |
| `make test-postgres` | Tests selected by `pgstore`, with the `postgres` feature enabled | `DATABASE_URL` pointing at PostgreSQL |
| `make test-docs` | Rust documentation tests | No Docker |
| `make test-e2e` | A clean local stack followed by the full E2E entry point | Docker Compose and the fixture secrets |

`make install-tools` installs mdBook, `typos`, `cargo-nextest` and Taplo, and
adds the `clippy` and `rustfmt` components to the active Rust toolchain.

## CI boundaries

The default PR check does **not** run PostgreSQL integration tests,
documentation tests, `cargo-nextest`, or Docker E2E tests. Those commands are
available for targeted validation and release certification. The L2-to-L2 E2E
workflow in `.github/workflows/e2e-l2l2.yml` is separately dispatched or
enabled by its workflow label; it is not part of the default check job.

The optional `postgres` feature is therefore validated only when
`make test-postgres` (or an equivalent feature-enabled build) is run.

## Useful entry points

```bash
# Fast local parity with the repository-defined lint and unit checks
make lint
make test-unit
make test-scripts

# PostgreSQL-backed store tests
DATABASE_URL=postgres://... make test-postgres

# Full clean-stack integration suite
make test-e2e
```

See `Makefile` for the authoritative command definitions and
`docs/RUNNING-E2E.md` for the current integration-test topology and workflows.
