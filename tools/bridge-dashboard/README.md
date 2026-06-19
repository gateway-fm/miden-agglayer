# agglayer → miden bridge dashboard

Single-file static HTML dashboard showing realtime agglayer → miden bridging activity.
Gateway terminal/nerdcore aesthetic — JetBrains Mono, sharp edges, gateway purple.

## Run it

```sh
# anything that serves a static file works
python3 -m http.server -d bridge-dashboard 8088
# then open http://localhost:8088/
```

`file://` also works in most browsers, but Firefox blocks `fetch()` to other origins
from a `file://` page — use a static server.

## Networks

The dropdown lists preconfigured networks from `index.html`'s `window.NETWORKS = [...]`
catalog. Each entry has a `status`:

- **`live`** — endpoint URLs were confirmed working at catalog-author time.
- **`needs-port-forward`** — endpoints assume `kubectl port-forward` is running.
- **`placeholder`** — template; edit the URL fields before use.

**Bali specifically** ships with the public endpoint `https://miden-agglayer.dev.eu-north-3.gateway.fm`,
which works — *except* that endpoint does not currently send `Access-Control-Allow-Origin`,
so browsers block agglayer JSON-RPC calls and only the L1 side of the timeline populates.
Verified live as of 2026-05-28: L1 head + L1 deposits stream fine from Sepolia publicnode;
agglayer calls surface as `POLL_ERROR` rows in the stream.

Two ways to unlock the agglayer side:
1. **Switch network to `bali-localhost`** and run a local port-forward yourself:
   ```sh
   kubectl -n outpost-testnet-miden-testnet port-forward svc/miden-agglayer 8546:8546
   ```
2. **Or** get gateway.fm to add `Access-Control-Allow-Origin: *` to the public endpoint
   (preferred — that's the one ordinary users will hit).

Add your own networks via the `[+] add network` button — they persist to localStorage
under `agglayer-dashboard:user-networks`.

## What it actually polls

| Source | RPC method / endpoint | Cadence | Produces |
|---|---|---|---|
| L1 chain | `eth_blockNumber` | every tick | `L1_HEAD` event |
| L1 chain | `eth_getLogs` against GER contract | every tick (chunked ≤10k blocks) | `L1_DEPOSIT` event per log |
| agglayer proxy | `zkevm_getLatestGlobalExitRoot` | every tick | `GER_COMPUTED` on new hash |
| agglayer proxy | `zkevm_getExitRootsByGER(hash)` | for each pending GER | `GER_RESOLVED` when non-null |

Default tick = 3s. Configurable in the footer.

## What it does NOT show

These need Postgres access that the browser can't have directly:

- `ger_entries.is_injected` (i.e. "Miden tx committed for this GER")
- `transactions.status` from the proxy DB
- `sync.deposit.ready_for_claim` from bridge-service Postgres

If you need them, drop a thin Go relay alongside this HTML that exposes the postgres
columns as JSON over CORS — out of scope for v1.

## State

Everything lives in localStorage under keys `agglayer-dashboard:*`:

- `:events:${slug}` — last 1000 events for this network
- `:cursor:${slug}` — poll cursor (last L1 block, pending GERs, seen ids)
- `:user-networks` — networks the user added via the form
- `:cadence-ms` — poll interval

Clear via the footer's `[clear]` button or `localStorage.clear()`.
