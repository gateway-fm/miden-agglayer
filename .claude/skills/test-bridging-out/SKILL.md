---
name: test-bridging-out
description: Run an end-to-end L2→L1 bridge-out test against a Miden agglayer environment. Consumes Miden-side ETH from a deterministically-derived wallet, creates a B2AGG note targeting the Miden bridge account, submits it, and monitors the agglayer for the synthetic BridgeEvent that the L1 bridge-service will index. First env supported is Bali (Miden testnet → Sepolia).
---

# Test Bridging-Out — End-to-end L2→L1 verification

Counterpart to `/test-bridging`. Takes funds that are already on a Miden wallet (put there by `/test-bridging` or any prior claim) and burns them via a B2AGG note so the agglayer emits a `BridgeEvent` that the L1 side can later claim.

**Status**: L2→L1 has never been exercised on the deployed Bali testnet (0 Loki matches for `BridgeOutScanner` / `B2AggNote consumed` over 7 days). Expect unknowns; this skill is scaffolded so the first run produces diagnostic output rather than a silent success.

## Input

`$ARGUMENTS` — optional. If empty, defaults to `bali`. Otherwise, the first token names the env (`envs/<name>.env`); remaining tokens are passed as `KEY=VALUE` overrides (e.g. `bali BRIDGE_OUT_AMOUNT=5000`).

## Environment

@.claude/skills/test-bridging-out/envs/bali.env

Loads `envs/<env>.env` then `envs/<env>.env.local` (gitignored, holds secrets).

**Required config** (set in `.env.local` or shell):

- `CLAIMER_SEED` — same phrase used for `/test-bridging`. Derives the wallet that holds the ETH to bridge out.
- `MIDEN_BRIDGE_ID` — the Miden bridge account id (bech32 `mlcl1…` or hex `0x<30-hex>`). **Not publicly queryable on Bali** — ask Igor / the operator, or `cat /var/lib/miden-agglayer-service/bridge_accounts.toml` inside the agglayer pod.
- `MIDEN_FAUCET_ID` — the ETH faucet id on Miden. Auto-captured by `/test-bridging` from the claim note's "Assets to receive" line (e.g. `0xa88a59eb97990060612bc4a6c2f0dc` on Bali).

**Defaulted**:

- `DEST_L1_ADDRESS` defaults to `0x0000…0000` (placeholder). Override in `.env.local` with your own Sepolia address — e.g. the funder EOA from `/test-bridging`.
- `BRIDGE_OUT_AMOUNT` defaults to `10000` Miden-ETH units (= 0.0001 ETH after scale=10 reverse).
- `DRY_RUN=1` by default.

If `MIDEN_BRIDGE_ID` is missing, stop and tell the user which to set and where. Do not invent.

## Prerequisites (one-time or from prior run)

- A populated miden-client store with the claimer wallet deployed on-chain. `/test-bridging` leaves one at `/tmp/miden-claim-attempt-1`. If you don't have that, run `/test-bridging` first or copy an existing store to `$STORE_DIR`.
- The bridge-out tool binary. Built from the miden-agglayer workspace (`src/bin/bridge_out_tool.rs`), so the protocol pin matches the deployed service automatically.

## Procedure

### 1. Load env

```bash
ENV_NAME="${1:-bali}"
ENV_FILE=".claude/skills/test-bridging-out/envs/${ENV_NAME}.env"
LOCAL_FILE=".claude/skills/test-bridging-out/envs/${ENV_NAME}.env.local"
[[ -f "$ENV_FILE" ]] || { echo "no env file at $ENV_FILE"; exit 1; }
set -a; source "$ENV_FILE"; [[ -f "$LOCAL_FILE" ]] && source "$LOCAL_FILE"; set +a
```

Apply `KEY=VALUE` overrides from `$ARGUMENTS`. Print the resolved non-secret config (env, wallet, bridge, faucet, amount, dest, DRY_RUN).

### 2. Build the `bridge-out-tool` binary

The tool is a bin target of the miden-agglayer crate, so it rebuilds against whatever protocol version the agglayer is on (no separate Cargo.toml drift risk).

```bash
[[ -x "$BRIDGE_OUT_BIN" ]] || (cd "$GATEWAY_MIDEN_DIR/miden-agglayer" && cargo build --release --bin bridge-out-tool)
```

Cold build is ~5–10 min (full agglayer dep tree). Warm cache is seconds.

### 3. Derive the sender wallet address

Use the bundled `/test-bridging` claim-note binary for consistency — both skills derive the same account from `CLAIMER_SEED`:

```bash
"$CLAIM_NOTE_BIN" derive-address
#  Miden: 0x<30 hex>                  ← use as WALLET_ID
#  Eth:   0x<40 hex>                  ← not used in bridge-out, kept for parity
```

Confirm the Miden address matches the account that has ETH on-chain (from `/test-bridging`'s earlier claim).

### 4. Locate the store dir

```bash
# Preferred: reuse the store /test-bridging last used. The claimer account is
# already deployed there, which skips the "deploy a fresh account" round-trip.
STORE_DIR="${STORE_DIR:-/tmp/miden-claim-attempt-1}"
[[ -d "$STORE_DIR" && -f "$STORE_DIR/store.sqlite3" ]] || {
    echo "No usable miden-client store at $STORE_DIR."
    echo "Run /test-bridging first (which leaves a store at /tmp/miden-claim-attempt-1)"
    echo "or point STORE_DIR at an existing one that has the claimer wallet."
    exit 1
}
```

### 5. Dry-run the bridge-out

```bash
if [[ "$DRY_RUN" == "1" ]]; then
    echo "[dry-run] would submit B2AGG note:"
    echo "  store:       $STORE_DIR"
    echo "  node:        $MIDEN_RPC_URL"
    echo "  wallet:      $WALLET_ID"
    echo "  bridge:      $MIDEN_BRIDGE_ID"
    echo "  faucet:      $MIDEN_FAUCET_ID"
    echo "  amount:      $BRIDGE_OUT_AMOUNT  ($(awk "BEGIN {printf \"%.10f\", $BRIDGE_OUT_AMOUNT/10^8}") ETH-equivalent)"
    echo "  dest (L1):   $DEST_L1_ADDRESS (network $DEST_NETWORK)"
    exit 0
fi
```

**Pause for explicit user confirmation before broadcasting.** Never auto-broadcast.

### 6. Submit the B2AGG note

```bash
BROADCAST_TIME_UTC=$(date -u +%Y-%m-%dT%H:%M:%SZ)
echo "$BROADCAST_TIME_UTC" > /tmp/bridge-out-time.txt

"$BRIDGE_OUT_BIN" \
    --store-dir "$STORE_DIR" \
    --node-url "$MIDEN_RPC_URL" \
    --wallet-id "$WALLET_ID" \
    --bridge-id "$MIDEN_BRIDGE_ID" \
    --faucet-id "$MIDEN_FAUCET_ID" \
    --amount "$BRIDGE_OUT_AMOUNT" \
    --dest-address "$DEST_L1_ADDRESS" \
    --dest-network "$DEST_NETWORK" 2>&1 | tee /tmp/bridge-out.log
```

Expected output lines:
- `[bridge-out] wallet balance: <n>`
- `[bridge-out] B2AGG note created`
- `[bridge-out] transaction submitted: 0x<tx-id>`
- `[bridge-out] wallet balance after: <n - amount>`

Capture the L2 Miden tx id — it's the hash of the tx that submitted the B2AGG note.

### 7. Monitor the agglayer for BridgeEvent emission

The agglayer's `BridgeOutScanner` polls each sync cycle for consumed B2AGG notes, then emits a synthetic `BridgeEvent` EVM log. Expected log progression:

1. `INFO bridge_out.rs:* detected consumed B2AGG note` — scanner picked it up.
2. `INFO bridge_out.rs:* emitted BridgeEvent` — synthetic log persisted.
3. `INFO log_synthesis.rs:* bridge event recorded at block_num=<N>` — indexed.

Loki query (regex-case-insensitive):

```
{namespace="$LOKI_NAMESPACE", service_name="$LOKI_SERVICE"} |~ `(?i)BridgeOutScanner|emitted BridgeEvent|B2AggNote consumed|bridge_out\.rs`
```

The same `watch-claim.sh` pattern could be adapted — for now poll manually:

```bash
ETH_ADDR_NO_0X="${DEST_L1_ADDRESS#0x}"
ETH_ADDR_NO_0X="${ETH_ADDR_NO_0X,,}"     # lowercase

# one-shot probe
curl -sS "$GRAFANA_BASE_URL/api/datasources/proxy/uid/$LOKI_DATASOURCE_UID/loki/api/v1/query_range?..." \
    -b "$COOKIE_JAR"
```

(A dedicated `scripts/watch-bridge-out.sh` is a follow-up; the L1→L2 skill's `watch-claim.sh` is a good template.)

**Timing unknown**: no prior runs on testnet to calibrate against. Poll every 30s for up to ~5 minutes — the scanner runs on the client sync cadence.

### 8. (Optional — follow-up) Claim on L1

Once `BridgeEvent` is emitted and indexed, aggkit should relay it to the L1 bridge-service, which produces a claimable `merkle proof`. The user then calls `claimAsset(...)` on the Sepolia bridge contract (`$BRIDGE_ADDRESS`) with:
- the proof
- the origin network (L2 rollup id = 73)
- the dest network (0 = L1)
- the recipient (`DEST_L1_ADDRESS`)
- the amount (18-decimal ETH form — reverse the scale=10)

This skill does NOT implement the L1 claim step yet — it's a separate flow that needs the bridge-service + aggkit machinery verified first.

## Output

Single status block:

```
Env:           <name>
L2 wallet:     <0x30hex>
Miden tx:      <0x64hex>   ← submitted B2AGG
Amount:        <n> Miden units (= <eth> ETH)
Dest L1:       <0x40hex>
BridgeEvent:   <seen|pending>
Grafana:       <url to loki query>
L1 claim:      <manual — see step 8>
```

## Known unknowns / first-run caveats

- **aggsender dependency (blocking end-to-end)**: The L1-side settlement of a bridge-out relies on Ivan's `aggsender` + downstream bridge-service being live on this deployment. As of 2026-04-24 that isn't up. Running the skill **will** produce useful partial output (steps 1–7: the B2AGG submission on Miden + the synthetic `BridgeEvent` in the agglayer Loki stream), but step 8 (the `claimAsset` on Sepolia) cannot complete until aggsender is deployed. Hold on running end-to-end until Ivan confirms aggsender is up.
- **Bridge account discovery**: Igor's pod has `/var/lib/miden-agglayer-service/bridge_accounts.toml`. No public endpoint exposes it. Skill blocks on `MIDEN_BRIDGE_ID` until you ask.
- **B2AGG consumption**: The bridge account itself doesn't auto-consume B2AGG notes. Consumption is driven by the NTX (network tx) builder running inside miden-node. If the testnet node isn't running the NTX-enabled image, the note will sit unconsumed and the `BridgeOutScanner` will never see a consumption to emit against.
- **Scanner tick vs submission timing**: the scanner's `on_post_sync` hook fires every sync cycle (default ~30s). Don't panic before 2× that.
- **Store concurrency**: the tool's comments note potential SQLite contention between the tool and the agglayer's background sync loop on the same store. Not a concern here since we use a *client-side* store (`/tmp/miden-claim-attempt-1`), not the agglayer's server-side one.

## Adding a new environment

Copy `envs/bali.env` → `envs/<name>.env`. Secrets stay in `.env.local` (gitignored).

## Security

- **Never commit `.env.local`** — contains `CLAIMER_SEED` and `MIDEN_BRIDGE_ID`. The skill's `.gitignore` matches `envs/*.env.local` (was a footgun in a previous draft — see test-bridging/SKILL.md commit).
- The `CLAIMER_SEED` controls funds. Treat like a mnemonic.
