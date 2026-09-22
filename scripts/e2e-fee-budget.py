#!/usr/bin/env python3
"""Pre-fund a bounded load stage on the local Anvil E2E fixture.

One durable intent precedes each transfer. An interrupted/failed send is never
repeated automatically: a later invocation must observe the required balance
or fail. Funding happens BEFORE fault injection, never inside its deadline.
"""
import argparse
import fcntl
import hashlib
import json
import math
import os
from pathlib import Path
import re
import subprocess
import time
import tomllib
import urllib.request
import urllib.parse

ID = re.compile(r"0x[0-9a-f]{30}")


def run(*args, timeout=30):
    return subprocess.run(args, check=True, text=True, capture_output=True, timeout=timeout).stdout


def local_url(url):
    parsed = urllib.parse.urlparse(url)
    if parsed.scheme != 'http' or parsed.hostname not in ('localhost', '127.0.0.1', '::1'):
        raise ValueError('fee funding requires a local HTTP test endpoint')
    return url


def rpc(url, method):
    request = urllib.request.Request(local_url(url), data=json.dumps(
        {'jsonrpc': '2.0', 'id': 1, 'method': method, 'params': []}).encode(),
        headers={'Content-Type': 'application/json'})
    with urllib.request.urlopen(request, timeout=10) as response:
        return json.load(response)['result']


def validate_binding(info, url, internal_port):
    endpoint = urllib.parse.urlparse(local_url(url))
    bindings = info['NetworkSettings']['Ports'].get(f'{internal_port}/tcp') or []
    if not any(int(binding['HostPort']) == endpoint.port for binding in bindings):
        raise ValueError('test endpoint does not belong to the selected fixture container')


def validate_chain(url, expected):
    if int(rpc(url, 'eth_chainId'), 16) != expected or 'anvil' not in rpc(url, 'web3_clientVersion').lower():
        raise ValueError('fee funding requires the expected Anvil fixture chain')


def save(path, value):
    temporary = path.with_suffix('.tmp')
    temporary.write_text(json.dumps(value, indent=2) + '\n')
    temporary.replace(path)


def sample_metrics(body, fee_faucet, now):
    balances, timestamps = {}, {}
    maximum = expected_accounts = None
    for line in body.splitlines():
        if line.startswith('bridge_fee_vault_expected_accounts '):
            expected_accounts = float(line.split()[1])
        if line.startswith('bridge_fee_max_per_txn '):
            maximum = float(line.split()[1])
        match = re.fullmatch(r'bridge_fee_vault_(balance_by_id|sample_timestamp_seconds)\{([^}]+)\} ([^ ]+)(?: .*)?', line)
        if not match:
            continue
        labels = dict(re.findall(r'(\w+)="([^"]*)"', match[2]))
        if labels.get('fee_faucet') != fee_faucet:
            continue
        account = labels.get('account_id', '')
        value = float(match[3])
        if not ID.fullmatch(account) or not math.isfinite(value) or value < 0:
            raise ValueError('invalid fee sample')
        target = balances if match[1] == 'balance_by_id' else timestamps
        if account in target:
            raise ValueError('duplicate fee sample identity')
        target[account] = value
    if maximum is None or not math.isfinite(maximum) or maximum < 0 or maximum != int(maximum):
        raise ValueError('missing or invalid fee cap')
    if maximum == 0:
        return 0, {}
    if not balances or balances.keys() != timestamps.keys() or len(balances) != expected_accounts:
        raise ValueError('missing fee balance/timestamp samples')
    if any(not 0 <= now - timestamp <= 120 for timestamp in timestamps.values()):
        raise ValueError('stale fee samples; refusing to fund or certify this stage')
    return int(maximum), {account: int(balance) for account, balance in balances.items()}


def required_balances(accounts, service, cap, cascades, cascade_txns, operations):
    if not 0 <= cascades <= 256 or not 0 <= operations <= 10000 or not 1 <= cascade_txns <= 100000:
        raise ValueError('stage fee budget exceeds fixture bounds')
    # Includes wallet setup, claims, exits, nudges, consume fees and a 512-tx
    # reserve for the stage. Every existing network account gets this runway.
    ordinary = cap * (512 + 8 * operations)
    required = {account: ordinary for account in accounts}
    required[service] = ordinary + cap * cascade_txns * cascades
    return required


def ensure_budget(path, identity, required, cap, observe, send, wait_seconds=300, pause=time.sleep):
    if path.exists():
        previous = json.loads(path.read_text())
        if previous.get('status') == 'pending':
            if previous['identity'] != identity:
                raise RuntimeError('unresolved funding intent belongs to a different fixture; inspect it before proceeding')
            balances = observe()
            if any(balances.get(account, -1) < amount for account, amount in previous['required'].items()):
                raise RuntimeError('funding outcome remains ambiguous; refusing to submit again')
            previous['status'] = 'observed'
            save(path, previous)
    balances = observe()
    transfers = {account: amount - balances.get(account, 0) + cap * 16
                 for account, amount in required.items() if balances.get(account, -1) < amount}
    if not transfers:
        return {'status': 'sufficient', 'required': required, 'balances': balances}
    if sum(transfers.values()) > 100_000_000:
        raise ValueError('fee transfer exceeds per-stage test funding ceiling')
    record = {'status': 'pending', 'identity': identity, 'required': required,
              'before': balances, 'transfers': transfers, 'created_at': time.time()}
    save(path, record)  # Includes fsync below before a command can submit.
    with path.open('rb') as durable:
        os.fsync(durable.fileno())
    directory = os.open(path.parent, os.O_RDONLY)
    try:
        os.fsync(directory)
    finally:
        os.close(directory)
    # No retry on timeout/nonzero: the transfer may have been admitted.
    record['command_output'] = send(transfers)
    save(path, record)
    deadline = time.monotonic() + wait_seconds
    while True:
        balances = observe()
        if all(balances.get(account, -1) >= amount for account, amount in required.items()):
            record.update(status='observed', after=balances, observed_at=time.time())
            save(path, record)
            return record
        if time.monotonic() >= deadline:
            raise RuntimeError('fee top-up not observed before deadline; intent preserved, no automatic resend')
        pause(5)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--project', required=True)
    parser.add_argument('--new-faucets', type=int, required=True)
    parser.add_argument('--operations', type=int, required=True)
    parser.add_argument('--l1-rpc', default=os.getenv('L1_RPC', 'http://localhost:8545'))
    parser.add_argument('--proxy-rpc', default=os.getenv('L2_RPC', 'http://localhost:8546'))
    parser.add_argument('--evidence', type=Path)
    args = parser.parse_args()
    if not re.fullmatch(r'[a-zA-Z0-9][a-zA-Z0-9_.-]*', args.project):
        raise ValueError('invalid compose project')
    validate_chain(args.l1_rpc, 271828)
    proxy = f'{args.project}-miden-agglayer-1'
    funder = f'{args.project}-fee-funder-1'
    info = json.loads(run('docker', 'inspect', proxy, funder, f'{args.project}-anvil-1', f'{args.project}-miden-node-1'))
    for container, service in zip(info, ('miden-agglayer', 'fee-funder', 'anvil', 'miden-node')):
        labels = container['Config']['Labels']
        if labels.get('com.docker.compose.project') != args.project or labels.get('com.docker.compose.service') != service:
            raise ValueError('fixture container ownership mismatch')
        if service != 'fee-funder' and (container['State']['Status'] != 'running' or container['State'].get('Paused') or container['State'].get('Restarting')):
            raise ValueError('fixture is not ready for a fee preflight')
    validate_binding(info[0], args.proxy_rpc, 8546)
    validate_binding(info[2], args.l1_rpc, 8545)
    # A zero-fee fixture has no funding manifest and its one-shot funder exits.
    # Confirm the zero cap on this project's proxy before requiring either.
    with urllib.request.urlopen(local_url(args.proxy_rpc) + '/metrics', timeout=10) as response:
        caps = [float(line.split()[1]) for line in response.read().decode().splitlines()
                if line.startswith('bridge_fee_max_per_txn ')]
    if caps == [0.0]:
        result = {'status': 'zero-fee', 'project': args.project}
        if args.evidence:
            save(args.evidence, result)
        print(json.dumps(result))
        return
    funder_state = info[1]['State']
    if funder_state['Status'] != 'running' or funder_state.get('Paused') or funder_state.get('Restarting'):
        raise ValueError('fee-charging fixture funder is unavailable')
    # The tool runs inside this project's fixture network, with the matching
    # genesis operator file mounted from its node_data volume.
    node_volume = next(m['Name'] for m in info[1]['Mounts'] if m['Destination'] == '/data' and m['Type'] == 'volume')
    if not any(m.get('Name') == node_volume for m in info[3]['Mounts']):
        raise ValueError('fee funder and node do not share the same genesis volume')
    volume = json.loads(run('docker', 'volume', 'inspect', node_volume))[0]
    if volume['Labels'].get('com.docker.compose.project') != args.project:
        raise ValueError('genesis volume belongs to a different fixture')
    manifest_text = run('docker', 'exec', proxy, 'cat', '/var/lib/miden-agglayer-service/funding.toml')
    manifest = tomllib.loads(manifest_text)
    for field in ('service', 'ger_manager', 'fee_faucet_id'):
        if not ID.fullmatch(manifest[field]):
            raise ValueError(f'invalid manifest {field}')
    env = dict(item.split('=', 1) for item in info[0]['Config']['Env'] if '=' in item)
    cascade_txns = int(env.get('FEE_TXN_BUDGET_CASCADE', '256'))
    identity = hashlib.sha256((volume['CreatedAt'] + manifest_text).encode()).hexdigest()
    state_dir = Path('/tmp') / f'e2e-fee-budget-{args.project}'
    state_dir.mkdir(mode=0o700, exist_ok=True)
    with (state_dir / 'lock').open('a') as lock:
        fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        def sample():
            with urllib.request.urlopen(local_url(args.proxy_rpc) + '/metrics', timeout=10) as response:
                return sample_metrics(response.read().decode(), manifest['fee_faucet_id'], time.time())
        cap, balances = sample()
        if cap != manifest['max_fee_per_txn']:
            raise ValueError('fee cap disagrees with the fixture manifest')
        if cap == 0:
            print('fee preflight: zero-fee chain')
            return
        if not {manifest['service'], manifest['ger_manager']} <= balances.keys():
            raise ValueError('fee metrics do not cover both manifest accounts')
        required = required_balances(balances, manifest['service'], cap, args.new_faucets, cascade_txns, args.operations)
        def observe():
            current_cap, current = sample()
            if current_cap != cap:
                raise ValueError('fee policy changed during funding')
            return current
        def send(transfers):
            command = ['docker', 'exec', funder, 'bridge-out-tool', '--store-dir',
                f'/tmp/stage-fee-funder-{time.time_ns()}', '--node-url', 'http://miden-node:57291',
                '--fund-fee-asset', '--faucet-operator-mac', '/data/accounts/faucet_operator.mac',
                '--fee-faucet-id', manifest['fee_faucet_id']]
            for account, amount in transfers.items():
                command.extend(['--fund', f'{account}={amount}'])
            return run(*command, timeout=600)
        result = ensure_budget(state_dir / 'last-intent.json', identity, required, cap, observe, send)
        if args.evidence:
            save(args.evidence, result)
        print(json.dumps(result))


if __name__ == '__main__':
    main()
