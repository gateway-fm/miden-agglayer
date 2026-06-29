# Running the e2e stack locally

The e2e stack is a full Miden↔AggLayer bridge: an Anvil L1 (replaying a Kurtosis
CDK snapshot), the Miden node microservices (validator / node / ntx-builder /
remote-prover), the `miden-agglayer` proxy (service under test), a patched
zkevm-bridge-service, AggLayer, and AggKit. It is driven by
`docker-compose.e2e.yml` + the `make e2e-*` targets.

Kurtosis is **only** used once to mint the L1 snapshot (fixtures); it is not part
of running the stack — at runtime everything talks to Anvil.

---

## 1. Prerequisites

Host tools: `docker` (Docker Desktop), `cast`/`anvil`/`forge` (foundry, on
`~/.foundry/bin`), `kurtosis` (v1.16+), `jq`, `python3`, `node` (LTS), `cargo`.

Sibling repos, cloned next to this checkout (`../`):

| dir | source | ref |
|---|---|---|
| `aggkit-proxy` | `github.com/mandrigin/aggkit-proxy` | `main` (provides `kurtosis/miden-cdk`) |
| `kurtosis-cdk` | `github.com/0xPolygon/kurtosis-cdk` | default |
| `miden-node-src` | `github.com/0xMiden/node` | **`v0.15.0`** |
| `zkevm-bridge-service` | `github.com/revitteth/zkevm-bridge-service` | `fix/pending-bridges-rollup-disambiguation` |

`run-all.sh` clones these automatically on a bare Ubuntu box. On macOS, clone
them by hand (its provisioning phase is Linux/apt-only).

### Apple Silicon (arm64) — required lockfile pin

The miden libraries must use **plonky3 `p3-* 0.5.3`**, not `0.5.2`. `p3-goldilocks
0.5.2` computes wrong Goldilocks field values on ARM, so the proxy crash-loops at
init with `value for key 0x… not present in the advice map` (the deploy commitment
computed on arm64 doesn't match). `Cargo.lock` must have all `p3-*` at `0.5.3`
(the node already pins them). x86_64 is unaffected. Verify:

```bash
awk '/name = "p3-/{n=$3} /version/{if(n){print n,$3;n=""}}' Cargo.lock | grep 0.5.2  # must be empty
```

---

## 2. Build the node + bridge images (not on any registry)

```bash
cd ../miden-node-src
for p in "miden-validator 50101" "miden-node 57291" "miden-ntx-builder 50301" "miden-remote-prover 50051"; do
  set -- $p; docker build --build-arg BIN=$1 --build-arg PORT=$2 -t $1 .; done
cd ../zkevm-bridge-service && docker build -t zkevm-bridge-service:v0.6.4-RC2-pendingbridges -f Dockerfile .
```

(`run-all.sh` also patches the ntx-builder remote-prover client timeout 10s→180s so
the ~12.5s B2AGG proof on a slow host isn't cancelled.) The `miden-agglayer`
service image is built by `make e2e-up`. First build is ~45 min total on arm64.

---

## 3. Generate fixtures (one-time Kurtosis L1 snapshot)

`scripts/setup-fixtures.sh` deploys a Kurtosis CDK enclave, extracts the L1
state, and templates the service configs. It produces (all gitignored —
regenerate, don't hand-edit):

- `combined.json` — L1 contract addresses (RollupManager, bridge, GER, rollup, POL, sequencer)
- `*.keystore` — aggoracle, aggregator, claimsponsor, **and sequencer** (the aggsender
  signs certs with the sequencer key; missing it ⇒ `"expected proposer…"` cert failure)
- `l1-raw-txs.txt` — ~32 raw L1 deploy txs; Anvil **replays** these at startup
- `.env` — contract addrs + `ADMIN_API_KEY` (+ `SPONSOR_PRIVATE_KEY` after `ensure-sponsor-key.sh`)
- `bridge/agglayer/aggkit/autoclaim-config.toml` — rendered service configs

Prereqs that must hold or it aborts:
- `kurtosis engine start` (uses Docker Desktop)
- siblings present: `../aggkit-proxy/kurtosis/miden-cdk` **and** `../kurtosis-cdk`
  (miden-cdk's `kurtosis.yml` `replace`s `0xPolygon/kurtosis-cdk` → `../../../kurtosis-cdk`)
- `params.yaml` has `miden: { deploy_miden_services: false }` — **L1-snapshot only**.
  Without it Kurtosis demands local miden/aggkit images that don't exist and aborts
  during validation. This gate is on `aggkit-proxy` main.

```bash
kurtosis engine start
KURTOSIS_CDK_DIR=../aggkit-proxy/kurtosis/miden-cdk ./scripts/setup-fixtures.sh
./scripts/ensure-e2e-secrets.sh && ./scripts/ensure-sponsor-key.sh
```

Deterministic: Kurtosis CDK uses a fixed mnemonic, so addresses/keys are identical
every run; re-running is safe/idempotent. Already handled inside the script (don't
re-discover): indexer scan-start blocks = 1 (Anvil replays from genesis),
`rollupCreationBlockNumber` = deploy block, sequencer-keystore extraction.

---

## 4. Bring up the stack (left running)

```bash
make e2e-up      # cleans data, builds the service image, starts ~12 services, --waits healthy
docker compose -f docker-compose.e2e.yml --env-file fixtures/.env ps
```

Endpoints: L1 `:8545`, L2 proxy `:8546`, bridge-service `:18080`, postgres `:5432`.
Give the proxy ~45s after "healthy" to write `bridge_accounts.toml`.

---

## 5. Manual deposit / withdraw

```bash
source fixtures/.env; export PATH="$HOME/.foundry/bin:$PATH"
C=miden-agglayer-miden-agglayer-1
ACCT=$(docker exec $C cat /var/lib/miden-agglayer-service/bridge_accounts.toml)
WALLET=$(echo "$ACCT" | sed -n 's/.*wallet_hardhat = "\(.*\)"/\1/p')
BRIDGE=$(echo "$ACCT" | sed -n 's/.*bridge = "\(.*\)"/\1/p')
FAUCET=$(echo "$ACCT" | sed -n 's/.*faucet_eth = "\(.*\)"/\1/p')

# L2->L1 withdraw (bridge-autoclaim settles on L1):
docker exec $C bridge-out-tool --store-dir /var/lib/miden-agglayer-service \
  --node-url http://miden-node:57291 --wallet-id $WALLET --bridge-id $BRIDGE \
  --faucet-id $FAUCET --amount 500 --dest-address 0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266 --dest-network 0

# check L2 balance (dry probe):
docker exec $C bridge-out-tool --store-dir /var/lib/miden-agglayer-service \
  --node-url http://miden-node:57291 --wallet-id $WALLET --bridge-id $BRIDGE \
  --faucet-id $FAUCET --amount 999999999 --dest-address 0xdead --dest-network 0 2>&1 | grep 'wallet balance:'
```

L1→L2 deposits land via `cast send $BRIDGE_ADDRESS 'bridgeAsset(...)'` and are
auto-claimed by the service — see `scripts/e2e-l1-to-l2.sh` for the exact form.

---

## 6. Run the e2e suites

Individual flows (each `make` target re-cleans + re-ups a fresh stack):

```bash
make e2e-l1-to-l2        # L1->L2 deposit + CLAIM/MINT/P2ID
make e2e-l2-to-l1        # L2->L1 bridge-out -> cert -> settle -> claimAsset
make e2e-restore         # disaster recovery
make e2e-rd940           # async writer worker (6 scripts)
# ... see `make help`
```

Full regression matrix (10 suites, fresh stack each, ~1-3h):

```bash
./run-regression.sh      # writes out/REGRESSION-SUMMARY.txt + out/regr-*.log
```

To poke a running stack without re-cleaning, run the scripts directly
(`./scripts/e2e-l1-to-l2.sh`) rather than the `make` target.

---

## 7. Troubleshooting

- **`advice map` crash-loop on arm64** → plonky3 0.5.2 in `Cargo.lock`; pin `p3-*`
  to `0.5.3` and rebuild the service (§1).
- **`bridge-autoclaim` restarting** → `SPONSOR_PRIVATE_KEY` not set; run
  `./scripts/ensure-sponsor-key.sh` and restart the service.
- **Cert rejected `"expected proposer…"`** → missing `sequencer.keystore`; regenerate
  fixtures.
- **`DeadlineExceeded` pulling a base image** → Docker registry/auth hiccup; retry.
- **macOS** is supported with the arm64 pin above; `/var`→`/private/var` symlink
  handling in `sanitize_store_dir` is already in place.
