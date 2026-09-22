#!/usr/bin/env python3
"""Detect per-network bridge synchronizer stalls and recover the E2E service.

Check-only is the default. --watch permits bounded restarts of the existing
container, with its database retained. No resync, database write or image swap.
Fresh iteration timestamps matter; checkReorg block numbers may legitimately
stay old on an empty chain. sync.status alone can be stale after a thread dies.
"""
import argparse
import datetime
import fcntl
import gzip
import json
import os
from pathlib import Path
import re
import subprocess
import time
import urllib.request

ANSI = re.compile(r'\x1b\[[0-9;]*[mK]')
ITERATION = re.compile(r'\bINFO\s+synchronizer/synchronizer.go:\d+\s+NetworkID:\s*(\d+)[,.]\s+(?:\[checkReorg function\] Checking Block \d+|Syncing block: \d+)\b')


def timestamp(value):
    return datetime.datetime.fromisoformat(value.replace('Z', '+00:00')).timestamp()


def progress(lines, networks, started, now, max_age=120):
    last = {}
    for line in lines:
        line = ANSI.sub('', line)
        match = ITERATION.search(line)
        if not match:
            continue
        try:
            observed = timestamp(line.split()[0])
        except (ValueError, IndexError):
            continue
        network = int(match[1])
        if network in networks and started <= observed <= now:
            last[network] = max(last.get(network, 0), observed)
    ages = {str(network): now - last[network] if network in last else None for network in networks}
    stale = [network for network in networks if network not in last or now - last[network] > max_age]
    return {'ages': ages, 'stale': stale, 'healthy': not stale, 'process_age': now - started}


def command(*args, timeout=30):
    result = subprocess.run(args, text=True, capture_output=True, timeout=timeout)
    if result.returncode:
        raise RuntimeError(f'{args[0]} failed ({result.returncode}): {result.stderr[-1500:]}')
    return result.stdout


def generation(info):
    state = info['State']
    return info['Id'], state['StartedAt'], info['RestartCount'], state['Status'], state['Paused'], state['Restarting']


def inspect(name):
    return json.loads(command('docker', 'inspect', name))[0]


def snapshot(container, networks):
    before = inspect(container)
    started = timestamp(before['State']['StartedAt'])
    result = subprocess.run(['docker', 'logs', '--timestamps', '--since', '10m', '--tail', '30000', container],
                            text=True, capture_output=True, timeout=30)
    if result.returncode:
        raise RuntimeError('bridge log snapshot unavailable')
    logs = result.stdout + result.stderr
    if generation(inspect(container)) != generation(before):
        raise RuntimeError('bridge process changed during snapshot')
    report = progress(logs.splitlines(), networks, started, time.time())
    report['generation'] = generation(before)
    report['running'] = before['State']['Status'] == 'running' and not before['State']['Paused'] and not before['State']['Restarting']
    report['healthy'] = report['healthy'] and report['running']
    return report, logs


def can_restart(report, dependencies_ready):
    return report['running'] and report['process_age'] >= 180 and bool(report['stale']) and dependencies_ready


def rpc_ready(port):
    request = urllib.request.Request(f'http://127.0.0.1:{port}',
        data=b'{"jsonrpc":"2.0","id":1,"method":"eth_blockNumber","params":[]}',
        headers={'Content-Type': 'application/json'})
    with urllib.request.urlopen(request, timeout=5) as response:
        return int(json.load(response)['result'], 16) >= 0


def dependencies(project, pg, ports):
    info = inspect(pg)
    if info['Config']['Labels'].get('com.docker.compose.project') != project:
        return False
    if info['State']['Status'] != 'running' or info['State']['Paused'] or info['State']['Restarting']:
        return False
    if command('docker', 'exec', pg, 'psql', '-U', 'bridge_user', '-d', 'bridge_db', '-tAX', '-c', 'SELECT 1').strip() != '1':
        return False
    services = {8545: ('anvil', 8545), 8546: ('miden-agglayer', 8546), 9545: ('anvil-l2b', 8545)}
    for port in ports:
        service, internal = services[port]
        info = inspect(f'{project}-{service}-1')
        if info['Config']['Labels'].get('com.docker.compose.project') != project:
            return False
        if info['State']['Status'] != 'running' or info['State']['Paused'] or info['State']['Restarting']:
            return False
        bindings = info['NetworkSettings']['Ports'].get(f'{internal}/tcp') or []
        if not any(int(binding['HostPort']) == port for binding in bindings):
            return False
    if 8546 in ports:
        # The proxy may still answer eth_blockNumber from its cache during the
        # deliberate node partition. Do not treat that as a healthy dependency.
        node = inspect(f'{project}-miden-node-1')
        if (node['Config']['Labels'].get('com.docker.compose.project') != project
            or node['State']['Status'] != 'running' or node['State']['Paused'] or node['State']['Restarting']):
            return False
        proxy = inspect(f'{project}-miden-agglayer-1')
        if not set(node['NetworkSettings']['Networks']) & set(proxy['NetworkSettings']['Networks']):
            return False
    return all(rpc_ready(port) for port in ports)


def recover(project, container, pg, networks, ports, directory):
    directory.mkdir(parents=True, exist_ok=False)
    before, logs = snapshot(container, networks)
    (directory/'before.json').write_text(json.dumps(before, indent=2))
    with gzip.open(directory/'before.log.gz', 'wt') as output:
        output.write(logs)
    if not can_restart(before, dependencies(project, pg, ports)):
        return 'skipped'
    # A consistent logical DB backup precedes the restart; failure stops recovery.
    with gzip.open(directory/'bridge-db.sql.gz', 'wb') as output:
        with subprocess.Popen(['docker', 'exec', pg, 'pg_dump', '-U', 'bridge_user', '-d', 'bridge_db'],
                              stdout=subprocess.PIPE, stderr=subprocess.PIPE) as dump:
            # stderr is normally empty; the bounded pg_dump timeout below is a
            # guard for a busy or unavailable database.
            try:
                data, error = dump.communicate(timeout=90)
            except subprocess.TimeoutExpired:
                dump.kill()
                dump.communicate()
                raise RuntimeError('bridge database backup timed out')
            if dump.returncode:
                raise RuntimeError('bridge database backup failed: ' + error.decode()[-1000:])
            output.write(data)
    # Recheck after backup, under the project lock. Never race a new generation
    # or restart a service whose dependency is deliberately faulted by chaos.
    latest, _ = snapshot(container, networks)
    if latest['generation'] != before['generation'] or not can_restart(latest, dependencies(project, pg, ports)):
        return 'skipped'
    (directory/'restart-intent.json').write_text(json.dumps({'container': container, 'at': time.time(), 'generation': latest['generation']}))
    command('docker', 'restart', '--time', '20', container, timeout=60)
    deadline = time.monotonic() + 180
    while time.monotonic() < deadline:
        after, logs = snapshot(container, networks)
        (directory/'after.json').write_text(json.dumps(after, indent=2))
        # Same ID proves that recreation did not replace its image or mounts.
        if after['generation'][0] != before['generation'][0]:
            raise RuntimeError('bridge container was replaced during recovery')
        if after['healthy']:
            with gzip.open(directory/'after.log.gz', 'wt') as output:
                output.write(logs)
            # Liveness only; the load's readiness/proof/receipt gates still
            # decide recovery, and its existing GER nudges wake claim readiness.
            return 'iterations-resumed'
        time.sleep(5)
    return 'failed'


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--container')
    parser.add_argument('--networks', type=int, nargs='+')
    parser.add_argument('--watch', action='store_true')
    parser.add_argument('--project')
    parser.add_argument('--evidence', type=Path)
    args = parser.parse_args()
    if not args.watch:
        report, _ = snapshot(args.container, args.networks)
        print(json.dumps(report))
        raise SystemExit(0 if report['healthy'] else 1)
    if not args.project or not re.fullmatch(r'[a-zA-Z0-9][a-zA-Z0-9_.-]*', args.project) or args.evidence is None:
        parser.error('--watch requires --project and --evidence')
    args.evidence.mkdir(parents=True, exist_ok=True)
    cases = [('bridge-service', 'postgres', [0, 1], [8545, 8546]),
             ('bridge-service-l2b', 'postgres-l2b', [0, 2], [8545, 9545])]
    attempts = {service: 0 for service, *_ in cases}
    with open(f'/tmp/{args.project}-bridge-sync-recovery.lock', 'a') as lock:
        fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        while True:
            for service, database, networks, ports in cases:
                container, pg = f'{args.project}-{service}-1', f'{args.project}-{database}-1'
                event = {'time': time.time(), 'service': service}
                try:
                    info = inspect(container)
                    labels = info['Config']['Labels']
                    if labels.get('com.docker.compose.project') != args.project or labels.get('com.docker.compose.service') != service:
                        raise RuntimeError('bridge container ownership mismatch')
                    report, _ = snapshot(container, networks)
                    event.update(report)
                    if attempts[service] < 3 and can_restart(report, dependencies(args.project, pg, ports)):
                        attempts[service] += 1
                        event['attempt'] = attempts[service]
                        event['outcome'] = recover(args.project, container, pg, networks, ports,
                            args.evidence/f'{service}-{attempts[service]}')
                    elif attempts[service] >= 3 and report['stale']:
                        event['outcome'] = 'budget-exhausted'
                except Exception as error:
                    event['error'] = str(error)
                with (args.evidence/'events.jsonl').open('a') as output:
                    output.write(json.dumps(event) + '\n')
                print(json.dumps(event), flush=True)
            time.sleep(15)


if __name__ == '__main__':
    main()
