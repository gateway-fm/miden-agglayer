#!/usr/bin/env python3
"""Wait for this load's L1 funding deposits, with bounded E2E certificate nudges.

The stock bridge index can miss an L2 GER notification before L1 catches up.
A new L2B certificate retries that notification through ordinary chain events.
No index writes, restarts or resets are performed. Readiness is not delivery;
the load's balance, receipt and exact-event gates still decide its verdict.
"""
import argparse
import fcntl
import importlib.util
import json
import os
from pathlib import Path
import re
import subprocess
import time
import urllib.request


def funding_status(deposits, hashes, destination):
    selected = {}
    for deposit in deposits:
        tx = deposit['tx_hash'].lower()
        if tx not in hashes:
            continue
        if (int(deposit['network_id']) != 0 or int(deposit['dest_net']) != 1
                or deposit['dest_addr'].lower() != destination.lower()
                or int(deposit['amount']) <= 0 or tx in selected
                or not isinstance(deposit['ready_for_claim'], bool)):
            raise ValueError('funding deposit identity/content mismatch')
        selected[tx] = deposit
    return {'expected': len(hashes), 'indexed': len(selected),
            'ready': sum(d['ready_for_claim'] for d in selected.values()),
            'missing': sorted(hashes - selected.keys()),
            'unready': sorted(tx for tx, d in selected.items() if not d['ready_for_claim'])}


def wait_ready(sample, nudge, record, timeout=600, clock=time.monotonic,
               pause=time.sleep, grace=60, interval=75, budget=6):
    deadline = clock() + timeout
    next_nudge = clock() + grace
    attempts = 0
    while clock() < deadline:
        status = sample()  # Unreadable or malformed evidence cannot authorize a send.
        record({'event': 'sample', **status})
        if status['ready'] == status['expected'] and status['expected'] > 0:
            return {'status': 'funding-ready', 'nudge_attempts': attempts, **status}
        if (status['indexed'] == status['expected'] and status['unready']
                and clock() >= next_nudge and attempts < budget):
            attempts += 1
            record({'event': 'nudge-intent', 'attempt': attempts})
            nudge(attempts)  # An ambiguous/failed send stops; never retry it blindly.
            record({'event': 'nudge-submitted', 'attempt': attempts})
            next_nudge = clock() + interval
        pause(min(5, max(0, deadline - clock())))
    raise RuntimeError('funding readiness deadline expired; workload must not start')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--project', required=True)
    parser.add_argument('--destination', required=True)
    parser.add_argument('--tx-hashes', type=Path, required=True)
    parser.add_argument('--bridge-url', required=True)
    parser.add_argument('--evidence', type=Path, required=True)
    args = parser.parse_args()
    if not re.fullmatch(r'[a-zA-Z0-9][a-zA-Z0-9_.-]*', args.project):
        raise ValueError('invalid project')
    if not re.fullmatch(r'0x[0-9a-fA-F]{40}', args.destination):
        raise ValueError('invalid destination')
    values = args.tx_hashes.read_text().lower().splitlines()
    if (not values or len(values) > 100 or len(set(values)) != len(values)
            or any(not re.fullmatch(r'0x[0-9a-f]{64}', v) for v in values)):
        raise ValueError('expected unique successful funding transaction hashes')
    hashes = set(values)
    scripts = Path(__file__).resolve().parent
    spec = importlib.util.spec_from_file_location('fee', scripts/'e2e-fee-budget.py')
    fee = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(fee)
    fee.local_url(args.bridge_url)
    args.evidence.mkdir(parents=True, exist_ok=False)

    def record(value):
        with (args.evidence/'events.jsonl').open('a') as f:
            f.write(json.dumps({'at': time.time(), **value}) + '\n')

    def owned(service):
        info = json.loads(fee.run('docker', 'inspect', f'{args.project}-{service}-1'))[0]
        labels = info['Config']['Labels']
        if (labels.get('com.docker.compose.project') != args.project
                or labels.get('com.docker.compose.service') != service
                or info['State']['Status'] != 'running' or info['State']['Paused']
                or info['State']['Restarting']):
            raise ValueError('fixture ownership/liveness mismatch: ' + service)
        return info

    fee.validate_binding(owned('bridge-service'), args.bridge_url, 8080)
    nudge_token = ''

    def nudge(attempt):
        nonlocal nudge_token
        # Only the local test topology may spend its fixture key. Recheck for
        # every attempt; never wake a similarly named foreign deployment.
        l2b = os.environ.get('L2B_RPC', 'http://localhost:9545')
        fee.validate_binding(owned('anvil-l2b'), l2b, 8545)
        fee.validate_chain(l2b, 31338)
        owned('aggkit-l2b')
        env = dict(os.environ, COMPOSE_PROJECT_NAME=args.project, NDG=nudge_token)
        script = '''set -euo pipefail
PROJECT_DIR="$PWD"
SCRIPT_DIR="$PWD/scripts"
source "$SCRIPT_DIR/lib-l2l2.sh"
if [[ -z "$NDG" ]]; then l2l2_deploy_nudge_token; fi
nudge_cert
printf 'NUDGE_TOKEN=%s\\n' "$NDG"
'''
        with (args.evidence/f'nudge-{attempt}.log').open('w') as f:
            result = subprocess.run(['bash', '-c', script], cwd=scripts.parent,
                                    env=env, stdout=f, stderr=subprocess.STDOUT, timeout=90)
        if result.returncode:
            raise RuntimeError('certificate nudge failed; inspect retained evidence')
        output = (args.evidence/f'nudge-{attempt}.log').read_text()
        match = re.search(r'^NUDGE_TOKEN=(0x[0-9a-fA-F]{40})$', output, re.M)
        if not match:
            raise RuntimeError('nudge result ambiguous; refusing another send')
        nudge_token = match[1]

    def sample():
        # The load provisions a fresh wallet and <=100 funding transactions.
        # Reject pagination rather than mistake a partial page for all deposits.
        url = args.bridge_url.rstrip('/') + '/bridges/' + args.destination + '?limit=100'
        with urllib.request.urlopen(url, timeout=10) as response:
            body = json.load(response)
        if int(body.get('total_cnt', len(body['deposits']))) > 100:
            raise ValueError('funding wallet exceeds the expected single page')
        return funding_status(body['deposits'], hashes, args.destination)

    with open(f'/tmp/{args.project}-funding-readiness.lock', 'a') as lock:
        fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        try:
            result = wait_ready(sample, nudge, record)
        except Exception as error:
            record({'event': 'failed', 'reason': str(error)})
            raise
        (args.evidence/'result.json').write_text(json.dumps(result, indent=2) + '\n')
        print(json.dumps(result), flush=True)


if __name__ == '__main__':
    main()
