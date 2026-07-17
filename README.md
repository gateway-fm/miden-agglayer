# miden-agglayer

`miden-agglayer` connects Polygon AggLayer tooling to a Miden rollup. It exposes
an EVM-shaped JSON-RPC service, translates supported bridge transactions into
Miden notes, and projects consumed Miden notes into an immutable synthetic EVM
history for AggKit and bridge-service.

See [the architecture guide](docs/ARCHITECTURE.md) for the component and flow
diagrams. All repository diagrams are text-based, GitHub-compatible Mermaid.

## What runs

The repository builds these binaries:

| Binary | Purpose |
|---|---|
| `miden-agglayer-service` | JSON-RPC proxy, Miden client, writer, projector, reconciler, monitors, and optional L1 InfoTree indexer |
| `bridge-out-tool` | Creates an isolated Miden wallet or submits a B2AGG bridge-out note |
| `bridge-autoclaim` | Watches this rollup's synthetic `BridgeEvent` logs and sponsors the corresponding L1 `claimAsset` calls |
| `check-burn-root` | Prints/checks the BURN script root used for protocol compatibility diagnostics |

The service supports the EVM-shaped methods used by AggKit and bridge-service,
including `eth_sendRawTransaction`, transaction/receipt lookups, block lookups,
`eth_getLogs`, `eth_call`, `eth_estimateGas`, `eth_syncing`, `eth_chainId`,
`net_version`, and the `zkevm_*` GER methods. The supported method list in
[`src/service.rs`](src/service.rs) is authoritative.

## Runtime model and constraints

- The `SyntheticProjector` is the only live/steady-state producer of synthetic
  logs and the only live owner of the synthetic tip. One-shot `--restore`
  reconstructs those events while the normal service is offline, then exits.
- Run exactly **one service replica per Miden store and synthetic Postgres
  store**. Multiple projectors are not supported.
- The Miden sqlite store, its keystore, and `bridge_accounts.toml` are one
  persistent unit. Do not share that directory between live processes.
- When `DATABASE_URL` is set, the service applies the SQL files embedded from
  `migrations/` at startup under a Postgres advisory lock. Do not add a separate
  migration container.
- The production write path is a bounded single writer. Accepted signed
  envelopes, nonce reservations, and exact note handoffs are persisted in the
  store. If a submission remains pending after a restart, rebroadcast the
  **same signed transaction**; do not construct a replacement with the same
  nonce.

Operational deployment, recovery, and alert guidance lives in
[`docs/operations/`](docs/operations/README.md).

## Prerequisites

- Rust 1.93 or newer (the release container currently builds with Rust nightly)
- Docker with Compose for the integration stack
- Foundry (`cast`) for integration scripts
- `jq`, Python 3, and Bash for the scripts

The one-time fixture generator additionally needs Kurtosis and a compatible CDK
checkout. See [Running the E2E stack](docs/RUNNING-E2E.md).

## Build and inspect the CLI

```bash
make build
./target/debug/miden-agglayer-service --help
./target/debug/bridge-out-tool --help
./target/debug/bridge-autoclaim --help
```

`--help` is the authoritative flag/default reference. Important service
settings include:

| Setting | Environment variable | Purpose |
|---|---|---|
| `--bind`, `--port` | `BIND_ADDR`, none | HTTP listen address and port; defaults are `0.0.0.0:8546` |
| `--miden-node` | none | Miden gRPC URL, or `devnet`/`testnet`; defaults to local port `57291` |
| `--miden-store-dir` | none | Persistent miden-client sqlite, keystore, and account config directory |
| `--database-url` | `DATABASE_URL` | Enables the production Postgres store; omission selects the in-memory store |
| `--chain-id` | `CHAIN_ID` | Value returned by `eth_chainId` |
| `--network-id` | `NETWORK_ID` | AggLayer rollup network ID stored in the bridge account |
| `--bridge-address` | `BRIDGE_ADDRESS` | Address stamped on synthetic bridge logs |
| `--l1-rpc-url` | `L1_RPC_URL` | L1 reads, metadata recovery, and GER decomposition |
| `--ger-l1-address` | `GER_L1_ADDRESS` | L1 GER contract used by the InfoTree indexer |
| `--miden-prover-url` | `MIDEN_PROVER_URL` | Remote Miden transaction prover |
| `--admin-api-key` | `ADMIN_API_KEY` | Bearer token for `admin_*`; without it all admin calls are disabled |
| `--allowed-signers` | `ALLOWED_SIGNERS` | Comma-separated EVM submitter allow-list; without it all signed submissions are rejected |
| `--require-hardening` | `REQUIRE_HARDENING` | Refuses startup unless admin auth, signer allow-list, non-wildcard CORS, and a reachable remote prover are configured |
| `--read-only` | `AGGLAYER_READ_ONLY` | Allows reads/reindexing while refusing every Miden transaction submission |

The writer queue is configured with `AGGLAYER_WRITER_QUEUE_DEPTH` (default
`64`) and `AGGLAYER_WRITER_TX_TTL` in seconds (default `300`). Queue-wait TTL
can fail work only before dispatch when no durable handoff exists; the same TTL
also controls eviction of old terminal entries from the in-memory status map.
It never expires queued/submitting work from the maintenance sweeper or turns
an ambiguous post-handoff submission into a failure.

### Store-directory containment

Absolute `--miden-store-dir` paths are supported. `..` traversal is rejected.
For deployments that template this path, set `MIDEN_STORE_BASE` to a trusted
root; the resolved store directory must remain inside that root, including
after symlink resolution.

## Start a development service

The first start initializes Miden accounts automatically when
`bridge_accounts.toml` is absent. `--init` forces initialization and exits, so
do not pass it to an existing deployment unless creating new account identities
is intentional.

```bash
./target/debug/miden-agglayer-service \
  --miden-node http://127.0.0.1:57291 \
  --miden-store-dir ./.miden-dev \
  --bind 127.0.0.1 \
  --port 8546
```

This is a read-capable development configuration. Because the signer allow-list
is fail-closed, `eth_sendRawTransaction` is rejected until `--allowed-signers`
is configured. `--insecure-allow-any-signer` exists only for a loopback/private
development boundary and is incompatible with `--require-hardening`.

A production-shaped invocation should use Postgres, a persistent store,
explicit signer/admin policy, L1 GER settings, and a remote prover:

```bash
DATABASE_URL="$DATABASE_URL" \
ADMIN_API_KEY="$ADMIN_API_KEY" \
ALLOWED_SIGNERS="$ALLOWED_SIGNERS" \
MIDEN_PROVER_URL="$MIDEN_PROVER_URL" \
miden-agglayer-service \
  --miden-node "$MIDEN_NODE_URL" \
  --miden-store-dir /var/lib/miden-agglayer-service \
  --chain-id "$CHAIN_ID" \
  --network-id "$NETWORK_ID" \
  --bridge-address "$BRIDGE_ADDRESS" \
  --l1-rpc-url "$L1_RPC_URL" \
  --ger-l1-address "$GER_L1_ADDRESS" \
  --bind 127.0.0.1 \
  --require-hardening
```

Supply secrets through the deployment secret mechanism, not literal shell
history. If a reverse proxy or sidecar is the network boundary, adapt `--bind`
to that topology while keeping port `8546` private.

## Health and metrics

The same listener serves:

- `POST /` — JSON-RPC
- `GET /health` — background Miden-client liveness (`503` after node connection loss)
- `GET /metrics` — Prometheus exposition

All routes share the configured per-IP rate limit and 256 KiB request-body
limit. The default rate is 500 requests/second with a burst of 500.

## L2 to L1 auto-claimer

`bridge-autoclaim` discovers exits from this proxy's `BridgeEvent` logs, checks
the rollup-qualified L1 `isClaimed` state, obtains a proof from bridge-service,
simulates `claimAsset`, and submits it with a sponsor key. Its sqlite cursor is
only a scan checkpoint; L1 `isClaimed` is the idempotency authority.

```bash
SPONSOR_PRIVATE_KEY="$SPONSOR_PRIVATE_KEY" bridge-autoclaim \
  --l2-rpc-url http://127.0.0.1:8546 \
  --l1-rpc-url "$L1_RPC_URL" \
  --l1-bridge-address "$L1_BRIDGE_ADDRESS" \
  --l2-bridge-address "$L2_BRIDGE_ADDRESS" \
  --bridge-service-url "$BRIDGE_SERVICE_URL" \
  --network-id "$NETWORK_ID" \
  --cursor-db /var/lib/bridge-autoclaim/cursor.sqlite
```

The private key is read from the environment variable named by
`--sponsor-key-env` (default `SPONSOR_PRIVATE_KEY`); there is deliberately no
private-key flag.

## Tests and development

```bash
make test-unit       # library unit tests
make test-scripts    # shell syntax and release-acceptance guards
make test-postgres   # PgStore tests; requires DATABASE_URL
make test-e2e        # fresh full stack, E2E suite, teardown
make e2e-up          # fresh stack left running
make e2e-test        # run the main E2E script against that stack
make e2e-down
make help            # every current target
```

`make e2e-up` deliberately removes local Miden/wallet state and the Docker node
volume before startup. Do not use it as a generic restart command when local
test state must be retained.

See [Running the E2E stack](docs/RUNNING-E2E.md) for required local images and
fixture generation.

## Recovery and upgrades

- [Operations runbook](docs/operations/runbook.md)
- [Diagnostics](docs/operations/diagnostics.md)
- [Monitoring](docs/operations/monitoring.md)
- [In-place upgrade guide](docs/UPGRADE.md)

`--unlock-miden-accounts`, `--reset-miden-store`, `--resweep-from-genesis`, and
`--restore` have different blast radii. Read the runbook before using them.
Never delete the keystore or `bridge_accounts.toml` during recovery.

## License

This project is available under either the
[Apache License 2.0](LICENSE-APACHE) or the [MIT License](LICENSE-MIT), at your
option. The vendored `axum-jrpc` crate retains its own
[MIT license](axum-jrpc/LICENSE).
