# External monitoring tools (instrumentation harness)

Independent-of-the-proxy watchdogs used during certification, upgrade testing and soaks.
They complement the IN-proxy completeness auditor (`synthetic_projector_completeness_missing_total`)
with an outside view: the node's own DB and the public RPC are the only inputs.

- `watch-completeness.sh` — every 10s, diff node-DB consumed B2AGG notes vs proxy
  `eth_getLogs` BridgeEvents up to (synthetic tip − margin). Prints
  `COMPLETENESS VIOLATION` on an unexplained miss (harness fast-fail grep target);
  deliberate emit refusals (poisoned-registry quarantine) classify as
  `EXPECTED-QUARANTINE`. WAL-aware node snapshots.
- `immutability-monitor.py` — records every block's log-set hash from `eth_getLogs`
  and flags ANY change after first exposure (the strong getLogs-immutability invariant);
  baseline resets on chain regression (fresh bringup).
- Usage: run detached alongside any e2e/loadtest/soak run; grep the output for
  `VIOLATION` as a stop-the-line signal.
