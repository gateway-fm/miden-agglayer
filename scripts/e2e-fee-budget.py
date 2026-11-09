#!/usr/bin/env python3
"""Pre-fund a bounded load stage on the local Anvil E2E fixture.

One durable intent precedes each transfer. An interrupted/failed send is never
repeated automatically. Operator-owned native faucets are outside this proxy's
funding scope; unresolved older transfers to them remain recorded and fenced.
Funding happens BEFORE fault injection, never inside its deadline.
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


def rpc(url, method, params=None, token=None):
    headers = {'Content-Type': 'application/json'}
    if token:
        headers['Authorization'] = f'Bearer {token}'
    request = urllib.request.Request(local_url(url), data=json.dumps(
        {'jsonrpc': '2.0', 'id': 1, 'method': method, 'params': params or []}).encode(),
        headers=headers)
    with urllib.request.urlopen(request, timeout=10) as response:
        result = json.load(response)
    if 'error' in result or 'result' not in result:
        raise RuntimeError(f'{method} did not return an authoritative result')
    return result['result']


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
    with temporary.open('w') as output:
        output.write(json.dumps(value, indent=2) + '\n')
        output.flush()
        os.fsync(output.fileno())
    temporary.replace(path)
    sync_directory(path.parent)


def sync_directory(path):
    directory = os.open(path, os.O_RDONLY)
    try:
        os.fsync(directory)
    finally:
        os.close(directory)


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


def funding_scope(accounts, faucets, network, manifest):
    """Mirror OwnershipDuty: only local-origin faucets are externally owned.

    FeeVaultMonitor deliberately observes native faucets too. Observation does
    not grant their signing keys to the proxy. Unknown accounts remain in scope;
    only authoritative registry metadata can exclude an account.
    """
    if type(network) is not int or not 0 <= network <= 0xffffffff:
        raise ValueError('invalid proxy network ID')
    if not isinstance(faucets, list):
        raise ValueError('invalid faucet registry response')
    protected = {manifest['service'], manifest['ger_manager']}
    if not protected <= accounts.keys():
        raise ValueError('fee metrics do not cover both manifest accounts')
    registered, excluded = set(), {}
    for row in faucets:
        if not isinstance(row, dict):
            raise ValueError('invalid faucet registry row')
        account, origin = row.get('faucet_id'), row.get('origin_network')
        if (not isinstance(account, str) or not ID.fullmatch(account)
                or type(origin) is not int or not 0 <= origin <= 0xffffffff):
            raise ValueError('invalid faucet registry identity or origin network')
        if account in registered:
            raise ValueError('duplicate faucet registry identity')
        registered.add(account)
        if account in protected:
            raise ValueError('faucet registry contradicts manifest account ownership')
        if origin == network:
            excluded[account] = {'reason': 'operator-owned native faucet',
                                 'origin_network': origin}
    if not registered <= accounts.keys():
        raise ValueError('fee metrics do not cover the complete faucet registry')
    return sorted(accounts.keys() - excluded.keys()), excluded


def read_funding_scope(url, env, accounts, manifest):
    # networkID(), unlike eth_chainId, returns the AggLayer network identity
    # used by admin_registerNativeFaucet and the ownership monitor.
    encoded = rpc(url, 'eth_call', [{'data': '0xbab161bf'}, 'latest'])
    if not isinstance(encoded, str) or not re.fullmatch(r'0x[0-9a-fA-F]{64}', encoded):
        raise ValueError('invalid proxy network ID response')
    token = env.get('ADMIN_API_KEY')
    if not token:
        raise ValueError('proxy admin key unavailable for authoritative faucet scope')
    faucets = rpc(url, 'admin_listFaucets', token=token)
    network = int(encoded, 16)
    managed, excluded = funding_scope(accounts, faucets, network, manifest)
    return managed, excluded, network


def checked_observation(sample, scope, expected_cap, expected_accounts, expected_scope):
    cap, balances = sample()
    if cap != expected_cap:
        raise ValueError('fee policy changed during funding')
    if scope(balances) != expected_scope or balances.keys() != expected_accounts:
        raise ValueError('fee account ownership or coverage changed during funding')
    return balances


def required_balances(accounts, service, cap, cascades, cascade_txns, operations):
    if not 0 <= cascades <= 256 or not 0 <= operations <= 10000 or not 1 <= cascade_txns <= 100000:
        raise ValueError('stage fee budget exceeds fixture bounds')
    # Includes wallet setup, claims, exits, nudges, consume fees and a 512-tx
    # reserve for the stage. Every existing network account gets this runway.
    ordinary = cap * (512 + 8 * operations)
    required = {account: ordinary for account in accounts}
    required[service] = ordinary + cap * cascade_txns * cascades
    return required


def pending_intents(path, identity, balances, excluded):
    """Keep unresolved transfers fenced even after their scope is corrected."""
    archive = path.with_name(path.stem + '-pending')
    if archive.exists() and not archive.is_dir():
        raise RuntimeError('invalid funding intent archive directory')
    paths = sorted(archive.glob('*.json')) if archive.exists() else []
    if path.exists():
        paths.append(path)
    pending = []
    for previous_path in paths:
        previous = json.loads(previous_path.read_text())
        # A later current intent carries references to unresolved older sends.
        # Missing evidence must not silently remove their ambiguity fences.
        for reference in previous.get('pending_outside_scope', []):
            referenced = Path(reference['path'])
            if referenced.parent != archive or not referenced.is_file():
                raise RuntimeError('missing referenced funding intent archive')
        if previous.get('status') == 'observed' and previous_path == path:
            continue
        if previous.get('status') != 'pending':
            raise RuntimeError('invalid funding intent status; refusing to submit')
        if previous['identity'] != identity:
            raise RuntimeError('unresolved funding intent belongs to a different fixture; inspect it before proceeding')
        targets = transfer_requirements(previous)
        archived_path = (archive / (hashlib.sha256(previous_path.read_bytes()).hexdigest() + '.json')
                         if previous_path == path else previous_path)
        acknowledged = (archive_observations(archived_path, previous, balances)
                        if archived_path.exists() else {})
        unmet = {account: amount for account, amount in targets.items()
                 if account not in acknowledged
                 if balances.get(account, -1) < amount}
        if unmet.keys() - excluded.keys():
            raise RuntimeError('funding outcome remains ambiguous; refusing to submit again')
        if unmet:
            pending.append({'path': str(previous_path), 'unobserved_required': unmet})
        elif previous_path == path:
            previous.update(status='observed', after=balances, observed_at=time.time())
            save(path, previous)
    return pending


def transfer_requirements(record):
    # A previous budget includes accounts that needed no transfer. Their later
    # ordinary spending cannot make the outcome of that send ambiguous.
    required, transfers = record.get('required'), record.get('transfers')
    if not isinstance(required, dict) or not isinstance(transfers, dict) or not transfers:
        raise RuntimeError('invalid funding intent targets')
    if not transfers.keys() <= required.keys():
        raise RuntimeError('funding intent transfer lacks its observation threshold')
    for account, amount in transfers.items():
        threshold = required[account]
        if (not ID.fullmatch(account) or type(amount) is not int or amount <= 0
                or type(threshold) is not int or threshold <= 0):
            raise RuntimeError('invalid funding intent transfer or threshold')
    return {account: required[account] for account in transfers}


def archive_observations(path, record, balances):
    """Acknowledge individual observations without altering original evidence.

    A mixed managed/native send can be partially observed. Once a transferred
    managed target was observed, its later spending must not resurrect its old
    ambiguity fence. The unobserved native targets keep their fences.
    """
    digest = hashlib.sha256(path.read_bytes()).hexdigest()
    if path.name != digest + '.json':
        raise RuntimeError('funding intent archive content mismatch')
    targets = transfer_requirements(record)
    acknowledgement = path.with_suffix('.observed')
    observed = {}
    if acknowledgement.exists():
        saved = json.loads(acknowledgement.read_text())
        observed = saved['balances']
        if (saved['intent_sha256'] != digest or saved['identity'] != record['identity']
                or not isinstance(observed, dict)
                or not observed.keys() <= targets.keys()
                or any(type(value) is not int or value < targets[account]
                       for account, value in observed.items())):
            raise RuntimeError('invalid archived funding observations')
    newly_observed = {account: balances[account] for account, amount in targets.items()
                      if account not in observed and balances.get(account, -1) >= amount}
    if newly_observed:
        observed.update(newly_observed)
        save(acknowledgement, {'intent_sha256': digest, 'identity': record['identity'],
                               'balances': observed})
    return observed


def archive_pending(path, balances):
    """Copy exact original bytes durably before replacing the current intent."""
    payload = path.read_bytes()
    archive = path.with_name(path.stem + '-pending')
    archive.mkdir(mode=0o700, exist_ok=True)
    sync_directory(archive.parent)
    destination = archive / (hashlib.sha256(payload).hexdigest() + '.json')
    if destination.exists():
        if destination.read_bytes() != payload:
            raise RuntimeError('funding intent archive identity collision')
    else:
        temporary = destination.with_suffix('.tmp')
        with temporary.open('wb') as output:
            output.write(payload)
            output.flush()
            os.fsync(output.fileno())
        temporary.replace(destination)
    sync_directory(archive)
    archive_observations(destination, json.loads(payload), balances)
    return destination


def ensure_budget(path, identity, required, cap, observe, send, wait_seconds=300,
                  pause=time.sleep, excluded=None):
    excluded = excluded or {}
    if required.keys() & excluded.keys():
        raise ValueError('funding requirements overlap excluded native accounts')
    balances = observe()
    pending = pending_intents(path, identity, balances, excluded)
    transfers = {account: amount - balances.get(account, 0) + cap * 16
                 for account, amount in required.items() if balances.get(account, -1) < amount}
    # Preserve partial observations even when this invocation needs no send.
    # Otherwise later spending could resurrect an already resolved target.
    for prior in pending:
        if prior['path'] == str(path):
            prior['path'] = str(archive_pending(path, balances))
    if not transfers:
        return {'status': 'sufficient', 'required': required, 'balances': balances,
                'excluded_native': excluded, 'pending_outside_scope': pending}
    if sum(transfers.values()) > 100_000_000:
        raise ValueError('fee transfer exceeds per-stage test funding ceiling')
    record = {'status': 'pending', 'identity': identity, 'required': required,
              'before': balances, 'transfers': transfers, 'created_at': time.time(),
              'excluded_native': excluded, 'pending_outside_scope': pending}
    save(path, record)
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
        managed, excluded, network = read_funding_scope(args.proxy_rpc, env, balances, manifest)
        required = required_balances(managed, manifest['service'], cap, args.new_faucets, cascade_txns, args.operations)
        def observe():
            return checked_observation(
                sample, lambda current: read_funding_scope(args.proxy_rpc, env, current, manifest),
                cap, balances.keys(), (managed, excluded, network))
        def send(transfers):
            command = ['docker', 'exec', funder, 'bridge-out-tool', '--store-dir',
                f'/tmp/stage-fee-funder-{time.time_ns()}', '--node-url', 'http://miden-node:57291',
                '--fund-fee-asset', '--faucet-operator-mac', '/data/accounts/faucet_operator.mac',
                '--fee-faucet-id', manifest['fee_faucet_id']]
            for account, amount in transfers.items():
                command.extend(['--fund', f'{account}={amount}'])
            return run(*command, timeout=600)
        result = ensure_budget(state_dir / 'last-intent.json', identity, required, cap,
                               observe, send, excluded=excluded)
        result['proxy_network'] = network
        if args.evidence:
            save(args.evidence, result)
        print(json.dumps(result))


if __name__ == '__main__':
    main()
