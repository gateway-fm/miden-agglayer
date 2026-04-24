---
name: test-bridging
description: Run an end-to-end L1→L2 bridge test against a Miden agglayer environment. Derives a deterministic Miden claimer account from a seed, broadcasts a deposit on the source chain, monitors the agglayer claim flow in Grafana/Loki, and verifies the destination Miden account is funded. First env supported is Bali (Sepolia → Miden testnet); add new envs under `envs/<name>.env`.
---

# Test Bridging — End-to-end L1→L2 verification

Run a real deposit through the bridge, watch the agglayer pick it up and produce a CLAIM note, and confirm the funds land on a Miden account the user controls.

## Input

`$ARGUMENTS` — optional. If empty, default to the `bali` environment. Otherwise, the first token names the environment (must match `envs/<name>.env`); remaining tokens are passed through as `KEY=VALUE` overrides (e.g. `bali AMOUNT_ETH=0.0005`).

## Environment

@.claude/skills/test-bridging/envs/bali.env

The skill loads `envs/<env>.env` first, then `envs/<env>.env.local` (gitignored, holds secrets) if present, then any `KEY=VALUE` from `$ARGUMENTS`.

**Required secrets** (the env file lists them at the bottom but must not contain values):
- `FUNDER_PRIVATE_KEY` — user's funded Sepolia EOA. Only used to top up the disposable bridge wallet; never used directly to broadcast the deposit.
- `CLAIMER_SEED` — long unique phrase (controls the deterministic Miden account)

Defaulted (override only if needed):
- `SEPOLIA_RPC_URL` defaults to `https://ethereum-sepolia-rpc.publicnode.com` (no auth, verified working). Set your own Infura/Alchemy URL in `.env.local` for higher rate limits.
- `MIDEN_RPC_URL` defaults to `https://rpc.testnet.miden.io:443` (no auth, the real Miden testnet node). The gateway.fm-fronted alternative `https://miden-testnet.eu-central-8.gateway.fm:443` requires auth-header support that's still in flight (miden-agglayer PR #29 + miden-client PR #2101).

`BRIDGE_WALLET_PRIVATE_KEY` is generated and persisted to `.env.local` automatically on first run by `scripts/fund-bridge-wallet.sh` — the user does not need to set it.

If any of the two required secrets above are missing after loading env files + shell, **stop and tell the user exactly which to set and where** (suggest `.claude/skills/test-bridging/envs/bali.env.local`). Do not try to make one up.

## Procedure

### 1. Load env

```bash
ENV_NAME="${1:-bali}"
ENV_FILE=".claude/skills/test-bridging/envs/${ENV_NAME}.env"
LOCAL_FILE=".claude/skills/test-bridging/envs/${ENV_NAME}.env.local"
[[ -f "$ENV_FILE" ]] || { echo "no env file at $ENV_FILE"; exit 1; }
set -a; source "$ENV_FILE"; [[ -f "$LOCAL_FILE" ]] && source "$LOCAL_FILE"; set +a
```

Apply `KEY=VALUE` overrides from `$ARGUMENTS` after the source. Print the resolved non-secret config (env name, BRIDGE_ADDRESS, DEST_NETWORK, AMOUNT_ETH, MIDEN_RPC_URL) so the user can sanity-check before anything broadcasts.

### 2. Build the bundled `claim-note` binary

The skill ships its own claim-note binary pinned at `miden-client v0.14.4` (same pin the agglayer itself uses). **Do not** use `aggkit-proxy/target/release/claim-note` — that tree is pinned to `miden-client 0.14.0-alpha.1` (`Falcon512Rpo` auth), which the current Poseidon2 testnet node rejects with `AcceptHeaderError(NoSupportedMediaRange)` AND produces a different AccountId from the same seed.

```bash
[[ -x "$CLAIM_NOTE_BIN" ]] || (cd "$SKILL_DIR/claim-note" && cargo build --release)
```

First build pulls miden-client from GitHub + resolves the `=0.14.4` protocol/standards pins; expect ~1–3 min on a warm cargo cache. Re-uses the cache on subsequent runs (~1s).

### 3. Derive the user's Miden + Eth-padded addresses (offline)

```bash
"$CLAIM_NOTE_BIN" derive-address
```

Capture both lines from the output:
- `Miden: 0x<30 hex>` → use as `DEST_MIDEN` for the deposit script
- `Eth:   0x<40 hex>` → this is what will appear as `destinationAddress` in the bridge event and what `address_mapper` decodes back to the AccountId

Show both to the user and explain that anyone with `CLAIMER_SEED` can re-derive the same account from any machine — that's how funds become recoverable.

### 4. Top up the disposable bridge wallet

```bash
ENV_LOCAL_FILE="$LOCAL_FILE" \
    .claude/skills/test-bridging/scripts/fund-bridge-wallet.sh
```

This generates `BRIDGE_WALLET_PRIVATE_KEY` on first run (saved to `.env.local`, chmod 600) and tops it up from `FUNDER_PRIVATE_KEY` to cover `AMOUNT_ETH + GAS_BUFFER_ETH`. Re-source `$LOCAL_FILE` after this step so `BRIDGE_WALLET_PRIVATE_KEY` is in your env. Print the bridge-wallet address and the funding tx hash (if any) so the user can see where their funds went.

### 5. Dry-run the deposit (using the bridge wallet)

```bash
DRY_RUN=1 \
    SEPOLIA_PRIVATE_KEY="$BRIDGE_WALLET_PRIVATE_KEY" \
    DEST_MIDEN="$MIDEN_ADDR" \
    AMOUNT_ETH="$AMOUNT_ETH" \
    "$DEPOSIT_SCRIPT"
```

Note `bali-l1-deposit.sh` reads `SEPOLIA_PRIVATE_KEY`, so we override it inline with the bridge wallet's key — the user's `FUNDER_PRIVATE_KEY` is never passed to the deposit script. Verify the `Destination` line shows `<MIDEN_ADDR> (Miden) -> <ETH_PADDED_ADDR> (Eth)`; if it shows `(sender)`, stop.

**Pause for explicit user confirmation before broadcasting.** Never auto-broadcast.

### 6. Broadcast

```bash
DRY_RUN=0 \
    SEPOLIA_PRIVATE_KEY="$BRIDGE_WALLET_PRIVATE_KEY" \
    DEST_MIDEN="$MIDEN_ADDR" \
    AMOUNT_ETH="$AMOUNT_ETH" \
    "$DEPOSIT_SCRIPT"
```

Capture the L1 tx hash. Record the broadcast wall-clock time (UTC) immediately so the Loki window has a hard lower bound:

```bash
BROADCAST_TIME_UTC=$(date -u +%Y-%m-%dT%H:%M:%SZ)
echo "$BROADCAST_TIME_UTC" > /tmp/broadcast-time.txt
echo "broadcast at $BROADCAST_TIME_UTC"
```

### 7. Monitor the agglayer claim flow in Grafana

**Preflight — pick a monitoring mode.** In order of preference:

1. **cmux browser (preferred, automated)** — `command -v cmux` and `$CMUX_WORKSPACE_ID` set. Use `scripts/watch-claim.sh` (below) for automated stage polling. If a prior surface titled `browse: bridge-test <env>` already exists in your workspace and points at `grafana.dev.eu-north-3.gateway.fm` (check with `cmux --json tree`), **reuse it** via `cmux browser surface:N navigate <url>` — the Google SSO session is preserved, saving a re-auth.
2. **claude-in-chrome MCP** — extension connected and a Grafana tab already authenticated. Poll via `mcp__claude-in-chrome__javascript_tool` with `fetch(credentials: "include")`.
3. **Human-driven fallback** — neither available. Print the Grafana URL and ask the user to open it themselves and watch for the three-stage progression. **Degrade gracefully; do not error.**

Build the Loki query targeting the user's eth-padded address (last 30 minutes from broadcast time):

```
{namespace="$LOKI_NAMESPACE", service_name="$LOKI_SERVICE"} |~ `(?i)<eth_padded_lowercase_no_0x>`
```

Use the full 40-character lowercase hex (no `0x`). **Important:** this regex only matches stage 1 (`creating CLAIM note`) — the subsequent pipeline lines (`GER propagation wait complete`, `submitted claim note txn`, `claim published`) do **not** include the address. After stage 1 fires, parse `global_index: <N>` out of that line and widen the query to `|~ \`miden_agglayer_service::claim\` |~ \`src/claim.rs\`` to track the rest of the pipeline.

Use the helper (Python one-liner) to construct the URL:

```python
import json, urllib.parse
expr = f'{{namespace="{LOKI_NAMESPACE}", service_name="{LOKI_SERVICE}"}} |~ `(?i){eth_addr_no_0x}`'
panes = {"vp5": {
  "datasource": LOKI_DATASOURCE_UID,
  "queries": [{"refId": "A", "expr": expr, "queryType": "range",
               "datasource": {"type": "loki", "uid": LOKI_DATASOURCE_UID},
               "editorMode": "code", "direction": "backward", "maxLines": 2000}],
  "range": {"from": "now-30m", "to": "now"}, "compact": False}}
url = f"{GRAFANA_BASE_URL}/explore?schemaVersion=1&panes=" + urllib.parse.quote(json.dumps(panes, separators=(",",":"))) + "&orgId=1"
```

For automated stage-by-stage monitoring (cmux mode), run:

```bash
ETH_ADDR_NO_0X="<40-char lowercase hex, no 0x>" \
    CMUX_SURFACE="surface:N" \
    .claude/skills/test-bridging/scripts/watch-claim.sh
```

It polls every 30s, captures `global_index` at stage 3, tracks the rest of the pipeline via `claim.rs` source (since post-stage-3 lines don't embed the address), emits one line per stage transition, exits 0 on `ClaimEvent recorded` (writes matching lines to `/tmp/claim-logs.txt`), and exits 1 on 20-min timeout.

The expected log progression for a successful claim is:
1. `INFO claim.rs:224 creating CLAIM note dest_address=<eth_padded>` — agglayer saw the deposit and is preparing to claim.
2. `INFO claim.rs:* GER propagation wait complete, submitting CLAIM note` — bridge state caught up.
3. `INFO service_send_raw_txn.rs:* claim published and ClaimEvent recorded` — claim submitted and observable on L2.

If progression stalls at (1) or shows `ERROR no known Miden AccountId`, the destination address didn't decode — check that `Eth:` from step 3 matches `dest_address` in the log exactly (lowercase comparison).

If progression stalls before (1) entirely (no "creating CLAIM note" line ever appears), suspect the publish_claim DNS bug fixed in PR #33 (`fix/dns-retry-claim-ger-build`). The agglayer needs to be running v0.1.3 or the PR-#33 build for L1→L2 claims to dispatch at all. Check the deployed image tag in Grafana with the "Startup Command" query in §11.

**Timing**: L1→L2 finality on Sepolia is ~6 min before aggkit relays the deposit; expect step (1) to appear 6–10 min after broadcast, and steps (2)–(3) within ~30s of (1). Total budget: 15 minutes from broadcast to claim. Poll Loki on a ~30s loop. Report the L2 tx id (`miden_tx`) when found.

**Other useful Loki queries** (curated from RD-856 debugging):

| Purpose | Regex filter |
|---|---|
| GER pipeline health | `UpdateGerNote transaction committed\|inserted GER with eth txn` |
| Claim pipeline health | `claimAsset call\|publish_claim\|ClaimEvent recorded\|claim published` |
| Miden client errors | `(?i)dns error\|failed to lookup\|MidenClient::sync` |
| Startup args (image tag) | `src/main.rs:\d+: Command` (api-key auto-redacted) |
| Bridge-out (L2→L1) | `BridgeOutScanner\|emitted BridgeEvent` |
| Heartbeat (v0.1.2+) | `miden_agglayer_service::heartbeat` |

### 8. (Optional) Verify L2 state

```bash
L2_RPC="$L2_RPC" "$L2_STATUS_SCRIPT"
```

**Known issue**: `bali-l2-status.sh` returns HTTP 404 at the root of `$L2_RPC` on some deployments — the JSON-RPC endpoint path has changed. If responses come back empty, skip this step and rely on the Loki signal from step 7. The agglayer log `claim published and ClaimEvent recorded` is the authoritative confirmation per the upstream docs.

### 9. Consume the claim note

`MIDEN_RPC_URL` defaults to `https://rpc.testnet.miden.io:443` — the public Miden testnet node. No port-forward, no auth header, no VPN: this works from any laptop. (The gateway.fm-fronted alternative still needs PR #29 to land before it accepts unauthenticated access without rate-limiting you.)

**Warning — the agglayer log's `claim_note_id` is NOT the on-chain Miden NoteId.** It's an internal tracking hash; the real NoteId is assigned when the note is committed to a block and is only discoverable by syncing the claimer account and listing its consumable notes. Procedure:

```bash
# Step 9a — run "claim" with any placeholder NoteId to trigger a sync + consumable-notes dump.
# The binary prints "DEBUG: Consumable notes for …" — grab the real NoteId from there.
MIDEN_STORE_PATH="/tmp/miden-claim-$(date -u +%s)" \
  "$CLAIM_NOTE_BIN" claim 0x0000000000000000000000000000000000000000000000000000000000000000 2>&1 | tee /tmp/consume-discover.out

# Step 9b — extract the real on-chain NoteId from the debug dump and re-run.
NOTE_ID=$(grep -oE 'Note ID: 0x[0-9a-f]{64}' /tmp/consume-discover.out | head -1 | awk '{print $3}')
echo "on-chain NOTE_ID=$NOTE_ID"

MIDEN_STORE_PATH="/tmp/miden-claim-$(date -u +%s)" \
  "$CLAIM_NOTE_BIN" claim "$NOTE_ID"
```

(Future improvement: add a `list-consumable` subcommand to the bundled `claim-note` so you don't need the two-step discovery dance.)

After the claim tx commits, sync once more and confirm the account holds the expected balance. **Decimal scaling**: origin is 18-decimal ETH on Sepolia; Miden ETH faucet is 8-decimal with `scale=10`. So a deposit of `AMOUNT_ETH * 10^18` wei lands as `AMOUNT_ETH * 10^8` Miden ETH units (e.g. `0.001 ETH = 100_000` units).

**Block-explorer caveat**: don't rely on `testnet.midenscan.com` to confirm the Miden tx — it returns empty Next.js shells for txs that ARE on-chain (private notes like `UpdateGerNote` aren't indexed). The authoritative confirmation is the agglayer log line `claim published and ClaimEvent recorded` plus `client.sync_state()` returning `TransactionStatus::Committed`.

## Output

Report a single status block at the end:

```
Env:        <name>
L1 tx:      <sepolia tx hash>           [https://sepolia.etherscan.io/tx/...]
Miden acct: <0x30hex>
Eth padded: <0x40hex>
Claim tx:   <miden tx id>  or  <pending — last seen at log line ...>
Balance:    <miden units>  or  <not yet consumed>
Grafana:    <url to the loki query window used>
```

If anything failed, name the step and the exact error, plus the Grafana query URL so the user can drill in further.

## Adding a new environment

Copy `envs/bali.env` to `envs/<name>.env`, swap the L1 contract addresses + L2 RPC + Loki namespace. Secrets stay in `envs/<name>.env.local` (gitignored).

## Notes on the bug-class this skill exists to catch

The agglayer's `address_mapper::resolve_address` has three resolution paths; only one of them works for arbitrary destinations: the **zero-padded form** that embeds the Miden AccountId in the EVM 20-byte slot. If `bali-l1-deposit.sh` is run without `DEST_MIDEN`, it falls back to the L1 sender's address — a normal EOA whose first byte is non-zero — and the agglayer retries the claim forever with `no known Miden AccountId for Ethereum address ...`. This skill always sets `DEST_MIDEN` from the deterministically-derived account in step 3, which keeps the destination in the supported form.

## Deployment / version reference

| Repo | Branch / tag | Notes |
|---|---|---|
| `gateway-fm/miden-agglayer` | `v0.1.2` deployed; `v0.1.3` pending | Heartbeat + sync-debug ungag (#31), proto fix (#30); DNS publish_claim fix (#33) en route |
| `0xMiden/miden-client` | pinned at `v0.14.4` in this repo AND in `claim-note/Cargo.toml` | Proto-build 0.14.9. Also: `miden-protocol =0.14.4` (EXACT pin) so we get `Falcon512Poseidon2` auth — 0.14.5 is API-identical but we pin exact to match what the agglayer deploys. |
| `0xMiden/miden-node` | `v0.14.8` on testnet | Returns plain-text 5xx via gateway.fm proxy → use `rpc.testnet.miden.io` |
| `mandrigin/aggkit-proxy` | pinned at `miden-client 0.14.0-alpha.1` (branch `agglayer-integration-tests`) | **Incompatible with current testnet.** Alpha.1 uses `Falcon512Rpo` auth; testnet node rejects its Accept header AND derives a different AccountId from the same seed. Do not use `$GATEWAY_MIDEN_DIR/aggkit-proxy/target/release/claim-note` — the skill bundles its own v0.14.4 binary under `claim-note/`. |

Linear: [RD-856](https://linear.app/gateway-fm/issue/RD-856) tracks the auth-header + DNS-fix line of work.

### Auth scheme gotcha (burned us once)

Between miden-protocol 0.14.0-alpha.1 → 0.14.4 the auth variant `AuthScheme::Falcon512Rpo` was renamed to `Falcon512Poseidon2` (and the hash function changed accordingly). Because the auth commitment feeds into the account-id hash, **the same `CLAIMER_SEED` produces a different account address under each scheme.** If you deposit to an address derived with an alpha-era binary and then try to claim with a v0.14.4 binary, the note is locked to an account the new binary can't reproduce. Always use the skill's bundled `claim-note` binary for both `derive-address` and `claim` to stay on Poseidon2.

## Security

- **Never commit `.env.local`** (gitignored) or any Sepolia private key / gateway.fm API key. The skill enforces this by separating funder/bridge keys and persisting only the disposable bridge wallet.
- The miden-agglayer L2 RPC accepts `eth_sendRawTransaction` from **any signer with a valid secp256k1 sig and the right chain_id+nonce** — there's no balance check on L2. The bridge wallet only needs Sepolia ETH to pay L1 gas; on L2 it acts as a stateless dispatcher.
