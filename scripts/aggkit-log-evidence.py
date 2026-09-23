#!/usr/bin/env python3
"""Read timestamped Aggkit logs; never confuse certificate/monitor IDs with tx hashes.

stdout is a TSV record: monitor_id, GER, signed_hashes (comma separated), retry
count, retry span, last-retry age, decision. Missing identities are '-'. This
module only parses evidence; callers must check durable admission separately.
"""

import argparse
import datetime
import json
import math
from pathlib import Path
import re
import sys
import time

HASH = r"0x[0-9a-fA-F]{64}"
INJECTION = re.compile(
    rf"\binject GER transaction (submitted|already exists in monitoring DB) "
    rf"with ID: ({HASH})\. GER: ({HASH})(?![0-9a-fA-F])"
)
SIGNED = re.compile(
    rf"\b(?:signed tx sent to the network: ({HASH})(?![0-9a-fA-F])"
    rf"|failed to send tx ({HASH}) to network:)"
)



def evidence(lines, now, *, latest=False, min_repeats=10, min_age=60, max_idle=30,
             since=0, identities=None):
    # Only signed identities persist across rolling log windows. Retry age and
    # count must always come from the current snapshot, never from cached chatter.
    identities = {} if identities is None else identities
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
        if timestamp < since or timestamp > now or not isinstance(metadata, dict):
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
        sent = SIGNED.search(line)
        monitor = metadata.get("monitoredTxId", "")
        if sent and isinstance(monitor, str) and re.fullmatch(HASH, monitor):
            broadcasts.setdefault(monitor.lower(), set()).add((sent[1] or sent[2]).lower())

    for monitor, item in injections.items():
        previous = identities.get(monitor)
        ambiguous = item.get("ambiguous", False) or bool(
            previous and (previous["ger"] != item["ger"] or previous["ambiguous"])
        )
        hashes = set(broadcasts.get(monitor, []))
        if previous:
            hashes.update(previous["hashes"])
        # Never discard a replacement hash and still authorize a heal: a
        # bounded overflow is ambiguous until a new monitor is observed.
        ambiguous = ambiguous or len(hashes) > 64
        item["ambiguous"] = ambiguous
        identities[monitor] = {"ger": item["ger"], "hashes": sorted(hashes)[:64],
                               "ambiguous": ambiguous, "last": item["last"]}
    # Bound the cache independently of soak duration. Only the latest injection
    # in fresh logs can authorize a probe; evicting old identities fails closed.
    for monitor in sorted(identities, key=lambda key: identities[key]["last"])[:-512]:
        del identities[monitor]
    if not injections:
        return ["-", "-", "-", 0, 0, 0, "no-injection"]
    monitor, item = max(injections.items(), key=lambda pair: pair[1]["last"])
    retries = item["retries"]
    span = max(retries) - min(retries) if retries else 0
    age = now - max(retries) if retries else 0
    hashes = identities[monitor]["hashes"]
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
    parser.add_argument("--since", default=None, help="Ignore logs from before this process start (ISO timestamp)")
    parser.add_argument("--state", type=Path, help="Bounded signed-identity cache; never stores retry evidence")
    parser.add_argument("--generation", help="Exact container ID, restart count and process start")
    args = parser.parse_args()
    if args.min_repeats < 1 or args.min_age < 0 or args.max_idle < 0:
        parser.error("invalid evidence thresholds")
    if args.state and not (args.generation and args.since):
        parser.error("--state requires --generation and --since")
    since = datetime.datetime.fromisoformat(args.since.replace("Z", "+00:00")).timestamp() if args.since else 0
    now = time.time() if args.now is None else args.now
    identities = {}
    if args.state and args.state.exists():
        cached = json.loads(args.state.read_text())
        if cached["generation"] == args.generation:
            identities = cached["identities"]
            # Corrupt or hand-edited state cannot become SQL or heal authority.
            if not isinstance(identities, dict) or len(identities) > 512:
                raise ValueError("invalid identity cache")
            for monitor, item in identities.items():
                if (not re.fullmatch(HASH, monitor) or not re.fullmatch(HASH, item["ger"])
                    or not isinstance(item["ambiguous"], bool)
                    or not isinstance(item["hashes"], list) or len(item["hashes"]) > 64
                    or not all(isinstance(h, str) and re.fullmatch(HASH, h) for h in item["hashes"])
                    or not isinstance(item["last"], (int, float))
                    or not math.isfinite(item["last"]) or not since <= item["last"] <= now):
                    raise ValueError("invalid cached injection identity")
    result = evidence(sys.stdin, now,
                      latest=args.latest, min_repeats=args.min_repeats,
                      min_age=args.min_age, max_idle=args.max_idle, since=since,
                      identities=identities)
    if args.state:
        temporary = args.state.with_suffix(args.state.suffix + ".tmp")
        temporary.write_text(json.dumps({"generation": args.generation, "identities": identities}))
        temporary.replace(args.state)
    print("\t".join(map(str, result)))


if __name__ == "__main__":
    main()
