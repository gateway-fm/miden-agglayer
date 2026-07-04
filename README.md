# miden-agglayer

Bridges Polygon AggLayer (EVM L1) to Miden (ZK rollup). Exposes a JSON-RPC interface mimicking an EVM node, translates EVM transactions into Miden notes (CLAIM, GER, B2AGG), and manages bidirectional bridging state.

**[Architecture & main flows →](docs/ARCHITECTURE.md)** — component diagram plus
Mermaid sequence diagrams for the three main flows (GER injection, Claim,
B2AGG bridge-out) and the three recovery mechanisms (live note-recovery
ladder, startup `--restore`, account self-heal).

![architecture](docs/architecture.png)
*(legacy pre-redesign diagram — see [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) for the current architecture)*

## How it works

The service sits between the AggLayer tooling (aggoracle, aggsender, bridge-service) and a Miden node. It:

1. **Accepts standard EVM JSON-RPC calls** (`eth_sendRawTransaction`, `eth_getLogs`, `eth_call`, etc.)
2. **Translates bridge operations** into Miden transactions:
   - `claimAsset()` → CLAIM note (L1 deposit arrives on Miden)
   - `insertGlobalExitRoot()` → GER update note (exit root sync)
   - B2AGG note scanning → `BridgeEvent` log (Miden withdrawal reaches L1)
3. **Maintains synthetic EVM state** (block numbers, logs, receipts) so AggLayer components see a familiar EVM chain
4. **Runs background tasks**: `ClaimSettler` auto-claims settled deposits on L1, `BridgeOutScanner` detects Miden withdrawals

### Module structure

```
src/
├── service.rs              # JSON-RPC router + dispatch (~300 lines)
├── service_send_raw_txn.rs # claimAsset / insertGlobalExitRoot processing
├── service_eth_call.rs     # eth_call handler + L1 forwarding
├── service_get_logs.rs     # eth_getLogs with LogFilter
├── service_get_txn_receipt.rs
├── service_debug.rs        # debug_traceTransaction
├── service_zkevm.rs        # zkevm_getLatestGlobalExitRoot, zkevm_getExitRootsByGER
├── service_helpers.rs      # Shared helpers, sol! macros, error types
├── service_state.rs        # ServiceState (shared state for all handlers)
├── store/
│   ├── mod.rs              # Store trait (~25 async methods)
│   ├── memory.rs           # InMemoryStore (default, used in tests)
│   ├── postgres.rs         # PgStore (production, --features postgres)
│   └── postgres_tests.rs   # PgStore integration tests
├── l1_client.rs            # L1Client trait + AlloyL1Client + NoOpL1Client
├── miden_client.rs         # MidenClient (dedicated thread, MPSC channel)
├── claim.rs                # publish_claim (CLAIM note creation)
├── claim_settler.rs        # Background L2→L1 auto-claiming
├── ger.rs                  # GER insertion + L1 exit root fetching
├── bridge_out.rs           # BridgeOutScanner (B2AGG note → BridgeEvent)
├── restore.rs              # Disaster recovery (--restore flag)
├── metrics.rs              # Prometheus metrics + /health endpoint
├── amount.rs               # ETH↔Miden decimal scaling (18 vs 8 decimals)
├── address_mapper.rs       # ETH address → Miden AccountId derivation
└── main.rs                 # CLI entry point (clap)
```

### Bridge amount ceiling

Every L1 → Miden deposit is scaled from the L1 `uint256` wei amount down
to a Miden fungible-asset amount (an 8-decimal `Felt`). The maximum
representable Miden value is the protocol-level fungible-asset cap
[`miden_client::asset::FungibleAsset::MAX_AMOUNT`][miden-max] —
`2^63 − 2^31 ≈ 9.223 × 10^18` Miden base units. For the default 18 → 8
decimal layout this corresponds to approximately **9.22 billion ETH per
single deposit**, well above any realistic single-deposit value.

This ceiling supersedes the earlier `u32::MAX` (~42.94 ETH) limit that
was reported under [RD-702][rd-702]. The cap is now enforced by
`EthAmount::scale_to_token_amount` (used inside `scale_claim_amount` at
`src/claim.rs`), not by an intermediate `u32` truncation — see the
boundary regression in
[`cantina_12_amount_cap_pins_fungible_asset_max`][cantina-12]
(`src/claim.rs:838-867`) and the above-old-ceiling acceptance test
[`accepts_amount_above_old_u32_ceiling`][accepts] (`src/claim.rs:802-807`).
Both will fail loud if a future refactor tightens or loosens the cap.

The flow is:

1. `claimAsset()` arrives over `eth_sendRawTransaction` with a `uint256
   amount` (wei).
2. `scale_claim_amount` (in `src/claim.rs`) divides by `10^scale`
   (where `scale = origin_token_decimals − miden_decimals`, usually
   `18 − 8 = 10`), truncating sub-unit wei.
3. `EthAmount::scale_to_token_amount` rejects the scaled value if it
   exceeds `FungibleAsset::MAX_AMOUNT`.
4. On rejection, `scale_claim_amount` returns
   `claim amount is not representable on Miden: <inner error>`. That
   error bubbles out of `publish_claim` as an `anyhow::Error`, the JSON-RPC
   handler maps it to a standard EVM error response, and the
   `try_claim` lock is released by the RAII drop guard so the caller
   may resubmit a smaller deposit. No CLAIM note is written, no Miden
   transaction is submitted, and the eth tx hash is not recorded as
   successful in the store.

Cantina #12 (see the source comment on the regression test) also bounds
the upstream MASM verifier's safe range; values that would land in the
`[2^123, 2^128)` MASM gap are rejected at this same step before any
note is built.

[miden-max]: https://github.com/0xMiden/miden-base
[rd-702]: https://linear.app/gatewayfm/issue/RD-702
[cantina-12]: src/claim.rs
[accepts]: src/claim.rs

## L2→L1 auto-claimer (`bridge-autoclaim`)

`bridge-autoclaim` is a standalone binary (`src/bin/bridge_autoclaim.rs`,
logic in `src/l2_to_l1_claimer.rs`) that closes the L2→L1 loop: it watches for
withdrawals leaving our rollup and submits the matching `claimAsset` on L1 so
users don't have to claim manually.

### Why it isn't the upstream `zkevm-autoclaimer`

The stock autoclaimer discovers work via the bridge-service `/pending-bridges`
endpoint. That endpoint's already-claimed gate matches a recorded L1 claim on
`(destination_network, leaf_index)` **only** — it drops the source rollup. On a
shared agglayer rollup manager every rollup claims its L2→L1 exits on the *same*
L1 bridge under `network_id = 0`, and per-rollup leaf indices overlap (every
rollup has an exit `#23`). So once any co-tenant claims their `#23`, that single
`(network=0, index=23)` row masks ours **forever**, and `/pending-bridges`
returns zero rows for ready, unclaimed exits.

The bug is in the bridge-service SQL and is present identically on upstream
`v0.6.4-RC2`/`main`/`develop`. **Polygon declined to fix it** — they classify
shared-manager L2→L1 auto-claiming as an "unsupported mode of operation" — so
the patched-fork-image path is dead. This binary is the replacement.

### How it sidesteps the bug

1. **Discovery — the proxy's own `BridgeEvent` via `eth_getLogs`.** The proxy
   emits a synthetic `BridgeEvent` for every B2AGG bridge-out it processes, and
   it only ever processes *our* rollup's exits. Discovering from those logs is
   rollup-scoped **by construction** — no co-tenant data is ever in scope, so
   there's nothing to disambiguate and no `SourceNetworkID` knob to get wrong.
2. **Already-claimed gate — on-chain `isClaimed`.** Each exit is gated on the L1
   bridge's `isClaimed(leafIndex, sourceBridgeNetwork)`, which is rollup-
   qualified and authoritative — structurally immune to the leaf-index
   collision that poisons `/pending-bridges`.
3. **Proofs — `/merkle-proof`.** Backed by the bridge-service `GetClaim` path,
   which was always correctly rollup-qualified and never had the bug.
4. **Submission — `claimAsset` on L1** with a sponsor wallet.

Note `sourceBridgeNetwork` (the source *rollup* = our network id, e.g. 76) is
distinct from the asset's `originNetwork` (0 for native ETH); and the agglayer
`globalIndex` for our exits is `((network_id − 1) << 32) | leafIndex`.

### Design decisions

- **Readiness = attempt-and-retry (option b).** Implemented as a pre-flight
  `eth_call` simulation of `claimAsset`. A not-yet-settled GER reverts with
  `GlobalExitRootInvalid`; we classify that as transient and retry on the next
  poll rather than burning gas on a doomed send. Only a clean simulation is
  followed by a signed submission. (We deliberately did *not* pre-check the GER
  via the GlobalExitRootManager — the simulation covers readiness *and* every
  other revert reason in one call.)
- **Idempotency = block cursor + on-chain `isClaimed`.** A tiny sqlite file
  (`--cursor-db`) records the last L2 block scanned so a restart resumes instead
  of re-scanning from genesis. It is **not** a claim ledger — the authoritative
  double-spend guard is `isClaimed`, so the claimer is correct even if the
  cursor is lost, reset, or rewound.
- **Sponsor key = `--sponsor-key-env`.** The private key is read from the
  environment variable *named* by that flag (default `SPONSOR_PRIVATE_KEY`),
  which deployment populates from the secret store. The key is never a CLI flag
  and is never logged. The sponsor EOA must hold L1 funds — it pays gas for
  `claimAsset`.

### Run

```bash
SPONSOR_PRIVATE_KEY=<from-secret-store> \
bridge-autoclaim \
  --l2-rpc-url http://localhost:8546 \
  --l1-rpc-url http://localhost:8545 \
  --bridge-address 0x<bridge> \
  --bridge-service-url http://localhost:18080 \
  --network-id 1
```

All flags also read from env (`L2_RPC_URL`, `L1_RPC_URL`, `BRIDGE_ADDRESS`,
`BRIDGE_SERVICE_URL`, `NETWORK_ID`, `POLL_INTERVAL_SECS`, `MAX_RANGE`,
`START_BLOCK`, `CURSOR_DB`, `SPONSOR_KEY_ENV`).

## Prerequisites

- [Rust](https://rustup.rs) (1.90+, nightly for Docker builds)
- [Docker](https://docs.docker.com/get-docker/) + Docker Compose (for E2E tests)
- [Foundry](https://getfoundry.sh) (`cast` CLI, for E2E tests)

Optional dev tools (install with `make install-tools`):

- `cargo-nextest` (faster test runner)
- `taplo` (TOML formatting)
- `typos-cli` (spell checker)

## Quick start

```bash
# Build
make build

# Run (connects to a local miden-node)
./target/debug/miden-agglayer-service \
    --miden-node http://localhost:57291 \
    --port 8546
```

### CLI flags

| Flag | Env var | Default | Description |
|------|---------|---------|-------------|
| `--port` | | `8546` | JSON-RPC HTTP port |
| `--miden-node` | | `http://localhost:57291` | Miden node gRPC URL (or `devnet`/`testnet`) |
| `--miden-store-dir` | `MIDEN_STORE_BASE` (containment) | `$HOME/.miden` | Directory for miden-client data. Absolute paths are accepted (deployments rely on it); `..` traversal and symlink-escape are rejected. Set `MIDEN_STORE_BASE` to require the store dir to live inside a fixed root — see note below. |
| `--chain-id` | `CHAIN_ID` | `2` | EVM chain ID for `eth_chainId` |
| `--network-id` | `NETWORK_ID` | `1` | Rollup network ID from RollupManager |
| `--l1-rpc-url` | `L1_RPC_URL` | | L1 RPC URL (enables GER verification + claim forwarding) |
| `--database-url` | `DATABASE_URL` | | PostgreSQL URL (enables PgStore; omit for InMemoryStore) |
| `--bridge-address` | `BRIDGE_ADDRESS` | | L1 bridge contract address |
| `--l1-ger-address` | `L1_GER_ADDRESS` | `0x1f7a...2674` | L1 GER contract address |
| `--rollup-manager-address` | `ROLLUP_MANAGER_ADDRESS` | `0x6c6c...da43` | RollupManager (eth_call forwarding) |
| `--rollup-address` | `ROLLUP_ADDRESS` | `0x414e...0e4e` | Rollup contract (eth_call forwarding) |
| `--restore` | | | Reconstruct store from miden-node + L1, then exit |
| `--init` | | | Initialize accounts config, then exit |
| `--reset-miden-store` | | | Wipe miden-client sqlite before startup (preserves keystore + config) — see [Recovery](#recovery) |
| `--unlock-miden-accounts` | | | Clear stale `locked` flags in miden-client sqlite, then exit — see [Recovery](#recovery) |

> **Store-directory containment (`MIDEN_STORE_BASE`).** `--miden-store-dir`
> always rejects `..` traversal and, for an existing directory, resolves
> symlinks and re-checks. Absolute paths are allowed by design — every
> deployment passes one (`/var/lib/miden-agglayer-service`, the `$HOME/.miden`
> default). For defence-in-depth when the store dir may come from a
> less-trusted source, set `MIDEN_STORE_BASE=/var/lib/miden-agglayer-service`
> (or your chosen root): the resolved store directory must then live inside it,
> and anything escaping the base — absolutely or via symlink — is rejected at
> startup. Unset = unchanged behaviour. (Cantina MA#20.)

### ClaimSettler env vars

| Env var | Description |
|---------|-------------|
| `CLAIM_SETTLER_ENABLED` | `true` to enable background L2→L1 claiming |
| `CLAIM_SETTLER_PRIVATE_KEY` | Private key for signing L1 claim transactions |
| `BRIDGE_SERVICE_URL` | Bridge-service REST API (default: `http://bridge-service:8080`) |
| `CLAIM_SETTLER_WATCH_ADDRESSES` | Comma-separated addresses to watch (default: signer address) |

## Testing

### Run everything

```bash
make test
```

This runs unit tests, then spins up the full docker-compose stack (Anvil, Miden node, PostgreSQL, bridge-service, AggLayer, AggKit), runs both L1→L2 and L2→L1 E2E tests with exact balance assertions, and tears everything down.

### Individual targets

```bash
# Unit tests only (fast, no docker)
make test-unit

# E2E only — spins up stack, tests, tears down
make test-e2e

# Individual E2E directions (spins up stack if needed)
make e2e-l1-to-l2        # Deposit on L1, verify exact L2 balance
make e2e-l2-to-l1        # Bridge out from L2, verify exact L1 balance delta

# Disaster recovery test
make e2e-restore          # Populate → wipe PG → restore → verify

# PgStore integration tests (needs running PostgreSQL)
DATABASE_URL=postgres://... make test-postgres

# Manage the E2E stack manually
make e2e-up               # Start stack
make e2e-down             # Tear down stack
make e2e-logs             # Tail all service logs
```

### What the E2E tests verify

**L1→L2 (`e2e-l1-to-l2.sh`):**
1. Deposits `10^13 wei` on L1 via `bridgeAsset()`
2. Waits for bridge-service to detect the deposit as `ready_for_claim`
3. Waits for ClaimTxManager to auto-submit a CLAIM note
4. Waits for CLAIM to commit on Miden
5. Asserts the L2 wallet balance equals **exactly 1000 Miden units** (`10^13 / 10^10`)

**L2→L1 (`e2e-l2-to-l1.sh`):**
1. Creates a B2AGG bridge-out note on Miden (half the wallet balance)
2. Waits for `BridgeEvent` to appear in L2 proxy logs
3. Waits for AggLayer certificate settlement
4. Waits for ClaimSettler auto-claim on L1
5. Asserts the L1 balance delta equals **exactly `bridge_amount * 10^10` wei**

Both tests fail immediately on any balance mismatch.

### Docker-compose stack

The E2E environment (`docker-compose.e2e.yml`) runs:

| Service | Image | Port | Purpose |
|---------|-------|------|---------|
| `anvil` | foundry | 8545 | L1 EVM chain with pre-deployed bridge contracts |
| `miden-node` | miden-node | 57291 | Miden ZK rollup node |
| `miden-agglayer` | (built from repo) | 8546 | Service under test |
| `agglayer-postgres` | postgres:16 | 5434 | miden-agglayer PgStore |
| `postgres` | postgres:16 | 5433 | bridge-service database |
| `bridge-service` | zkevm-bridge-service | 18080 | Polygon bridge REST API |
| `agglayer` | agglayer | 4443 | AggLayer certificate aggregation |
| `aggkit` | aggkit | 5576 | aggoracle + aggsender |

### Observability

The service exposes:
- `GET /health` — returns `{"status": "ok"}`
- `GET /metrics` — Prometheus metrics (request counts by method, latencies, claims/GERs/bridge-outs processed)

## Development

```bash
make check         # cargo check
make fmt           # Format Rust + TOML
make lint          # format-check + toml-check + typos-check + clippy
make lint-fix      # Auto-fix lint issues
make doc           # Generate docs
make install-tools # Install dev tools (nextest, taplo, typos)
```

### Database

With `--database-url` / `DATABASE_URL`, the service uses PostgreSQL instead of the in-memory store. Apply the schema:

```bash
psql $DATABASE_URL -f migrations/001_initial.sql
```

The docker-compose E2E stack handles this automatically via the `agglayer-migrate` service.

### Disaster recovery

If the PostgreSQL store is lost, reconstruct state from authoritative sources:

```bash
./target/release/miden-agglayer-service \
    --miden-node http://... \
    --l1-rpc-url http://... \
    --database-url postgres://... \
    --bridge-address 0x... \
    --restore
```

This scans the Miden node and L1 to rebuild claims, bridge-outs, GER entries, and synthetic logs.

## Recovery

When miden-client's local sqlite diverges from the node, the first tx submission
surfaces it as an opaque `transaction conflicts with current mempool state` /
`initial account commitment ... does not match current commitment ...` error.
On startup, the proxy checks every managed account's lock status via the
miden-client `AccountReader` API and logs an ERROR with a recovery hint if any
account is locked. The `miden_locked_accounts_detected_total` metric is
incremented too, so an alert can be wired to it.

Two recovery modes are available:

#### Surgical unlock — `--unlock-miden-accounts`

Clears the `locked` flag on every row in miden-client's sqlite
(`latest_account_headers` + `historical_account_headers`) and exits. Use this
when the only symptom is a stale lock and the underlying on-chain state is
actually fine.

```bash
./target/release/miden-agglayer-service \
    --miden-store-dir /var/lib/miden \
    --unlock-miden-accounts
# then restart the proxy normally
```

Fast (milliseconds) and keeps all local state. Reaches into miden-client's
private schema, so the operation may warn if miden-client bumps its schema;
that's logged and non-fatal.

#### Full reset — `--reset-miden-store`

Deletes `store.sqlite3` (plus the `-wal`/`-shm` sidecars) so startup rebuilds
an empty sqlite and re-syncs from the node. Keystore (private keys) and
`bridge_accounts.toml` (on-chain account IDs) are preserved — wiping either
would permanently lose control of the on-chain accounts.

```bash
./target/release/miden-agglayer-service \
    --miden-node http://... \
    --miden-store-dir /var/lib/miden \
    --database-url postgres://... \
    --reset-miden-store \
    --restore
```

Combine with `--restore` to also rebuild the proxy's Postgres/in-memory store
from on-chain notes in the same startup — otherwise the proxy resumes from a
stale PgStore checkpoint.

Caveat: after a reset the miden-client has an empty set of tracked accounts.
Public accounts re-attach automatically via sync. Private accounts (if any)
cannot be re-imported from the node alone — they would need a fresh `--init`
(which mints new on-chain accounts and invalidates existing balances), so
prefer `--unlock-miden-accounts` first when the divergence is recoverable.
