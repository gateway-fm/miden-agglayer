"""Evaluate one boot's evidence without assuming a visible HTTP recovery window."""

import argparse
import json
import re
from pathlib import Path


def boot_evidence(log, claim_tx, saw_withheld=False):
    log = re.sub(r"\x1b\[[0-9;]*[A-Za-z]", "", log)
    seed = repair = serving = None
    backlog = None
    for line in log.splitlines():
        timestamp = line.split(maxsplit=1)[0] if line.strip() else ""
        if "recovery readiness gated:" in line:
            match = re.search(r"claims_awaiting_calldata\s*[:=]\s*(\d+)", line)
            if match:
                seed, backlog = timestamp, int(match[1])
        if ("persisted authoritative full claimAsset calldata" in line
                and f"tx_hash: {claim_tx.lower()}" in line.lower()):
            repair = repair or timestamp
        if "Service started, address:" in line:
            serving = serving or timestamp
    # RFC3339 timestamps from the same tracing formatter sort chronologically.
    before_bind = bool(seed and repair and serving and seed < repair < serving)
    return {
        "seeded_backlog": backlog,
        "seed_at": seed,
        "claim_repaired_at": repair,
        "http_started_at": serving,
        "repaired_before_http": before_bind,
        "observed_503_with_backlog": saw_withheld,
        "accepted": bool(backlog and (saw_withheld or before_bind)),
    }


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("boot_log", type=Path)
    parser.add_argument("claim_tx")
    parser.add_argument("--saw-withheld", action="store_true")
    args = parser.parse_args()
    result = boot_evidence(args.boot_log.read_text(), args.claim_tx, args.saw_withheld)
    print(json.dumps(result, indent=2))
    raise SystemExit(0 if result["accepted"] else 1)
