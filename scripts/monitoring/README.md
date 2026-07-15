# External monitoring harness

These tools observe the Miden node database and the proxy's public RPC during
certification, upgrades and soak tests. They complement the in-process
`synthetic_projector_completeness_missing_total` metric with an independent
view.

| Tool | Purpose | Important output |
|---|---|---|
| `watch-completeness.sh` | Every `INTERVAL` seconds, compares consumed B2AGG notes in a WAL-aware node snapshot with synthetic `BridgeEvent` logs below `tip - MARGIN` | `COMPLETENESS VIOLATION` for an unexplained miss; deliberate quarantine is reported as `EXPECTED-QUARANTINE` |
| `immutability-monitor.py` | Hashes each synthetic block's log set and detects a change after first exposure | `IMMUTABILITY VIOLATION`; its final summary includes poll and violation counts |
| `soak-loop.sh` | Alternates clean and fault-injected mixed-load phases, runs a Miden-origin round trip each phase, and records a TSV ledger | Stops on completeness/immutability violations or a proxy tip that cannot recover within ten minutes |

The completeness watcher needs Docker access to the running node and proxy
containers, plus `curl`, Python 3 and SQLite support in Python. Override
`COMPOSE_PROJECT_NAME`, `PROXY_CONTAINER`, `NODE_CONTAINER`, `L2_RPC`,
`INTERVAL`, `MARGIN` or `B2AGG_ROOT` when the defaults do not match the stack.

```bash
# Run beside an existing Compose stack
COMPOSE_PROJECT_NAME=miden-agglayer \
  ./scripts/monitoring/watch-completeness.sh

# Monitor the default RPC for one hour (duration is seconds)
python3 ./scripts/monitoring/immutability-monitor.py 3600

# Alternate clean/chaos phases until the stop file is created
SOAK_DIR=/tmp/miden-soak ./scripts/monitoring/soak-loop.sh
touch /tmp/miden-soak/SOAK_STOP
```

Run the monitors detached when a test owns the foreground. Treat their
violation strings as stop-the-line signals; a monitor that never completed a
poll is not positive evidence of correctness.
