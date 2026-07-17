# Running the E2E stack locally

The local stack is defined by `docker-compose.e2e.yml` and driven by the
`e2e-*` Make targets. It contains:

- Anvil with replayed CDK contract-deployment transactions;
- Miden validator, sequencer (`miden-node`), network-transaction builder, and
  remote transaction prover;
- the `miden-agglayer` service under test and its Postgres store;
- bridge-service and its Postgres store;
- AggLayer, AggKit, and the standalone `bridge-autoclaim` process.

Kurtosis is used only to generate the gitignored L1 fixtures. It is not part of
the running Compose stack.

## Important: startup is destructive

`make e2e-up`, `make test-e2e`, and most standalone `make e2e-*` targets call
`e2e-clean-data`. That target deletes `.miden-agglayer-data/`,
`.b2agg-store/`, and the `miden-agglayer_node_data` Docker volume before
starting a new chain. Run scripts directly against an existing stack when its
state must be retained.

## Prerequisites

The normal host needs Docker with Compose, Foundry (`cast`), Bash, `jq`, Python
3, Node.js, and Rust 1.93 or newer. Fixture generation also needs Kurtosis.

Compose expects four locally built Miden images and one patched bridge-service
image:

| Image | Source used by repository scripts |
|---|---|
| `miden-validator` | `https://github.com/0xMiden/node.git` at `v0.15.0` |
| `miden-node` | same checkout and ref |
| `miden-ntx-builder` | same checkout and ref |
| `miden-remote-prover` | same checkout and ref |
| `zkevm-bridge-service:v0.6.4-RC2-pendingbridges` | `revitteth/zkevm-bridge-service`, branch `fix/pending-bridges-rollup-disambiguation` |

`run-all.sh` is the repository's supported bootstrap for a bare Ubuntu host. It
installs/checks tools, clones companion repositories next to this checkout,
patches the ntx-builder prover timeout used by this test environment, builds
missing images, and generates missing fixtures:

```bash
./run-all.sh
```

It also runs static checks and a bridge round trip. Use `SKIP_STATIC=1` when
only provisioning and the E2E run are wanted, or `SKIP_PROVISION=1` when all
images and fixtures already exist. Read the environment controls at the top of
`run-all.sh` before using it on a managed host.

### Manual image build

If provisioning manually, clone the node as `../miden-node-src`, check out the
ref printed by `make miden-node-image-coords`, and build the image names that
Compose consumes:

```bash
cd ../miden-node-src
for spec in \
  "miden-validator 50101" \
  "miden-node 57291" \
  "miden-ntx-builder 50301" \
  "miden-remote-prover 50051"
do
  set -- $spec
  docker build --build-arg BIN="$1" --build-arg PORT="$2" -t "$1" .
done

cd ../zkevm-bridge-service
docker build \
  -t zkevm-bridge-service:v0.6.4-RC2-pendingbridges \
  -f Dockerfile .
```

The current `run-all.sh` also raises the ntx-builder's remote-prover timeout to
180 seconds before building. Apply the same source change when reproducing its
manual build; otherwise a slow B2AGG proof can exceed the upstream client
timeout.

## Generate fixtures

The generator defaults to
`../aggkit-proxy/kurtosis/miden-cdk`. That package's `params.yaml` must select
the L1-snapshot-only mode (`miden.deploy_miden_services: false`), and its local
Kurtosis package replacement requires a sibling `../kurtosis-cdk` checkout.

```bash
kurtosis engine start
KURTOSIS_CDK_DIR=../aggkit-proxy/kurtosis/miden-cdk \
  ./scripts/setup-fixtures.sh
./scripts/ensure-e2e-secrets.sh
./scripts/ensure-sponsor-key.sh
```

The scripts generate the gitignored files consumed by Compose, including:

- `fixtures/combined.json`;
- `fixtures/l1-raw-txs.txt` and `fixtures/l1-transactions.json`;
- `fixtures/*.keystore`;
- `fixtures/.env`.

The tracked TOML files in `fixtures/` are templates/current defaults; the
fixture generator rewrites the deployment-specific values. Do not commit the
generated keystores or `.env`.

## Start and inspect the stack

```bash
make e2e-up
docker compose -f docker-compose.e2e.yml --env-file fixtures/.env ps
```

Host endpoints are loopback-only:

| Service | Endpoint |
|---|---|
| Anvil L1 | `http://127.0.0.1:8545` |
| miden-agglayer JSON-RPC | `http://127.0.0.1:8546` |
| Miden sequencer gRPC | `http://127.0.0.1:57291` |
| bridge-service HTTP | `http://127.0.0.1:18080` |
| bridge-service gRPC | `127.0.0.1:19090` |
| bridge-service Postgres | `127.0.0.1:5433` |
| miden-agglayer Postgres | `127.0.0.1:5434` |
| AggLayer gRPC/read RPC | `127.0.0.1:4443` / `127.0.0.1:4444` |
| AggKit exposed port | `127.0.0.1:5576` |

Use service names instead of generated container names:

```bash
docker compose -f docker-compose.e2e.yml --env-file fixtures/.env \
  logs -f miden-agglayer

curl -fsS http://127.0.0.1:8546/health
curl -fsS http://127.0.0.1:8546/metrics
```

## Run tests

The main targets are:

```bash
make test-e2e          # fresh base stack, full script suite, teardown
make e2e-test          # full script suite against an already-running stack
make e2e-l1-to-l2      # fresh stack, deposit/GER/CLAIM/MINT flow
make e2e-l2-to-l1      # fresh stack, funds wallet, B2AGG/certificate/L1 claim flow
make e2e-restore       # fresh stack, restore regression
make e2e-rd940         # fresh writer-configured stack, six writer scenarios
make e2e-security      # fresh stack, security scenarios
make e2e-l2l2-up       # fresh base stack plus second-rollup overlay
make e2e-l2l2          # L2-to-L2 group; overlay must already be running
make help              # current complete target list
```

`scripts/e2e-test.sh` also accepts a named group, such as `l1-to-l2`,
`l2-to-l1`, `tip-consistency`, `security`, or `l2l2`. Its `case` statement is
the authoritative list.

For an isolated regression matrix in which every suite gets a fresh stack:

```bash
./run-regression.sh
```

Results are written under the gitignored `out/` directory.

To run one script without resetting the chain:

```bash
./scripts/e2e-rpc-tip-consistency.sh
./scripts/e2e-l1-to-l2.sh
./scripts/e2e-l2-to-l1.sh
```

Scripts make assumptions about prior funding and test order; read the header of
the selected script first.

## Manual wallet work

Never point `bridge-out-tool` at the proxy's live store. Create an isolated
wallet store, fund that wallet through the normal L1-to-L2 path, and reuse the
same isolated store for bridge-out:

```bash
docker compose -f docker-compose.e2e.yml --env-file fixtures/.env \
  exec miden-agglayer bridge-out-tool \
  --store-dir /tmp/manual-wallet \
  --node-url http://miden-node:57291 \
  --create-wallet
```

The tool prints the wallet ID. Read `bridge_accounts.toml` through
`docker compose exec miden-agglayer` to obtain the bridge/faucet IDs, then use
`bridge-out-tool --help` for the current bridge-out arguments. The automated
and less error-prone reference is `scripts/lib-isolated-wallet.sh` plus
`scripts/e2e-l2-to-l1.sh`.

## Stop the stack

```bash
make e2e-down
```

This removes Compose volumes. Use a plain Compose `stop` only when retaining
the local chain for later inspection.

## Troubleshooting

- If `e2e-clean-data` reports that `node_data` is in use, stop the old stack
  before retrying. The target intentionally refuses to start a nominally fresh
  chain on stale node state.
- If `bridge-autoclaim` exits because the sponsor key is missing, run
  `./scripts/ensure-sponsor-key.sh` and recreate that service.
- If a certificate proposer is rejected, confirm that
  `fixtures/sequencer.keystore` exists and that AggKit mounts it as configured
  in `docker-compose.e2e.yml`.
- If Miden and the proxy disagree on script roots or genesis, confirm every
  local node image was built from the URL/ref printed by
  `make miden-node-image-coords`, then recreate the fresh stack.
- If the proxy fails with a cross-device sqlite rename error, retain the bind
  mount and `TMPDIR=/var/lib/miden-agglayer-service/tmp` arrangement from the
  checked-in Compose file.
