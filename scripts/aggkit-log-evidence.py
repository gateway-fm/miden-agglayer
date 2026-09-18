#!/usr/bin/env python3
"""Read timestamped Aggkit logs; never confuse certificate/monitor IDs with tx hashes.

stdout is a TSV record: monitor_id, GER, signed_hashes (comma separated), retry
count, retry span, last-retry age, decision. Missing identities are '-'. This
module only parses evidence; callers must check durable admission separately.
"""

import argparse
import datetime
import json
import re
import sys
import time

HASH = r"0x[0-9a-fA-F]{64}"
INJECTION = re.compile(
    rf"\binject GER transaction (submitted|already exists in monitoring DB) "
    rf"with ID: ({HASH})\. GER: ({HASH})(?![0-9a-fA-F])"
)
SENT = re.compile(rf"\bsigned tx sent to the network: ({HASH})(?![0-9a-fA-F])")


def evidence(lines, now, *, latest=False, min_repeats=10, min_age=60, max_idle=30):
    injections = {}
    broadcasts = {}
    for line in lines:
        try:
            timestamp = datetime.datetime.fromisoformat(
                line.split()[0].replace("Z", "+00:00")
            ).timestamp()
            metadata = json.loads(line[line.index("{"):])
        except (ValueError, IndexError):
            continue
        if timestamp > now or not isinstance(metadata, dict):
            continue
        match = INJECTION.search(line)
        if match and metadata.get("module") == "aggoracle":
            kind, monitor, ger = match.groups()
            monitor, ger = monitor.lower(), ger.lower()
            item = injections.setdefault(monitor, {"ger": ger, "last": timestamp, "retries": []})
            # Ambiguous identity must never authorize a destructive operation.
            if item["ger"] != ger:
                item["ambiguous"] = True
            item["last"] = max(item["last"], timestamp)
            if kind != "submitted":
                item["retries"].append(timestamp)
        sent = SENT.search(line)
        monitor = metadata.get("monitoredTxId", "")
        if sent and isinstance(monitor, str) and re.fullmatch(HASH, monitor):
            broadcasts.setdefault(monitor.lower(), set()).add(sent[1].lower())

    if not injections:
        return ["-", "-", "-", 0, 0, 0, "no-injection"]
    monitor, item = max(injections.items(), key=lambda pair: pair[1]["last"])
    retries = item["retries"]
    span = max(retries) - min(retries) if retries else 0
    age = now - max(retries) if retries else 0
    hashes = sorted(broadcasts.get(monitor, []))
    reason = "candidate"
    if item.get("ambiguous"):
        reason = "ambiguous-monitor"
    elif not latest and (not retries or age > max_idle):
        reason = "no-recent-retries"
    elif not latest and len(retries) < min_repeats:
        reason = "insufficient-repeats"
    elif not latest and span < min_age:
        reason = "too-recent"
    elif not hashes:
        reason = "no-signed-hash"
    return [monitor, item["ger"], ",".join(hashes) or "-", len(retries), int(span), int(age), reason]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--now", type=float, default=None)
    parser.add_argument("--latest", action="store_true", help="Map the latest injection for post-heal admission proof")
    parser.add_argument("--min-repeats", type=int, default=10)
    parser.add_argument("--min-age", type=int, default=60)
    parser.add_argument("--max-idle", type=int, default=30)
    args = parser.parse_args()
    if args.min_repeats < 1 or args.min_age < 0 or args.max_idle < 0:
        parser.error("invalid evidence thresholds")
    result = evidence(sys.stdin, time.time() if args.now is None else args.now,
                      latest=args.latest, min_repeats=args.min_repeats,
                      min_age=args.min_age, max_idle=args.max_idle)
    print("\t".join(map(str, result)))


if __name__ == "__main__":
    main()
