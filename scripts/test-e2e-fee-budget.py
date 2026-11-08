#!/usr/bin/env python3
import importlib.util
import json
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location('fee_budget', Path(__file__).with_name('e2e-fee-budget.py'))
fee = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fee)
SERVICE = '0x' + 'a' * 30
FAUCET = '0x' + 'b' * 30
GER = '0x' + 'c' * 30
NATIVE = '0x' + 'd' * 30
UNKNOWN = '0x' + 'e' * 30
MANIFEST = {'service': SERVICE, 'ger_manager': GER}
EXCLUDED = {NATIVE: {'reason': 'operator-owned native faucet', 'origin_network': 1}}


class Budget(unittest.TestCase):
    def pending(self, path, transfers, required=None, identity='chain'):
        record = {'status': 'pending', 'identity': identity,
                  'required': required or {account: 10 for account in transfers},
                  'transfers': transfers, 'command_output': 'submission evidence'}
        fee.save(path, record)
        return path.read_bytes()

    def test_ordinary_runway_does_not_cover_a_faucet(self):
        required = fee.required_balances([SERVICE], SERVICE, 210, 1, 1024, 0)
        self.assertEqual(184485 // 210, 878)
        self.assertGreater(required[SERVICE], 215040)
        self.assertLess(184485, required[SERVICE])

    def test_low_existing_network_account_is_included(self):
        required = fee.required_balances([SERVICE, FAUCET], SERVICE, 210, 9, 1024, 30)
        self.assertEqual(required[SERVICE] - required[FAUCET], 9 * 215040)
        self.assertGreater(required[FAUCET], 30 * 210)

    def test_sufficient_budget_sends_nothing(self):
        with tempfile.TemporaryDirectory() as directory:
            result = fee.ensure_budget(Path(directory)/'intent.json', 'chain', {SERVICE: 10}, 1,
                lambda: {SERVICE: 10}, lambda _: self.fail('unexpected transfer'))
            self.assertEqual(result['status'], 'sufficient')

    def test_intent_precedes_send_and_waits_for_balance(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory)/'intent.json'
            samples = iter([{SERVICE: 0}, {SERVICE: 1}, {SERVICE: 10}])
            sends = []
            def send(transfers):
                self.assertEqual(json.loads(path.read_text())['status'], 'pending')
                sends.append(transfers)
                return 'submitted'
            result = fee.ensure_budget(path, 'chain', {SERVICE: 10}, 1,
                lambda: next(samples), send, pause=lambda _: None)
            self.assertEqual(result['status'], 'observed')
            self.assertEqual(sends, [{SERVICE: 26}])

    def test_ambiguous_send_never_repeats(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory)/'intent.json'
            def send(_):
                raise TimeoutError('admission unknown')
            with self.assertRaises(TimeoutError):
                fee.ensure_budget(path, 'chain', {SERVICE: 10}, 1, lambda: {SERVICE: 0}, send)
            with self.assertRaisesRegex(RuntimeError, 'ambiguous'):
                fee.ensure_budget(path, 'chain', {SERVICE: 10}, 1,
                    lambda: {SERVICE: 0}, lambda _: self.fail('resent ambiguous transfer'))
            result = fee.ensure_budget(path, 'chain', {SERVICE: 10}, 1,
                lambda: {SERVICE: 10}, lambda _: self.fail('resent landed transfer'))
            self.assertEqual(result['status'], 'sufficient')

    def test_failed_balance_deadline_preserves_intent(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory)/'intent.json'
            with self.assertRaisesRegex(RuntimeError, 'not observed'):
                fee.ensure_budget(path, 'chain', {SERVICE: 10}, 1,
                    lambda: {SERVICE: 0}, lambda _: 'sent', wait_seconds=0)
            self.assertEqual(json.loads(path.read_text())['status'], 'pending')

    def test_stale_or_wrong_asset_metrics_cannot_authorize_stage(self):
        labels = f'account_id="{SERVICE}",fee_faucet="{FAUCET}"'
        body = f'bridge_fee_vault_expected_accounts 1\nbridge_fee_max_per_txn 210\nbridge_fee_vault_balance_by_id{{{labels}}} 184485\nbridge_fee_vault_sample_timestamp_seconds{{{labels}}} 1000\n'
        self.assertEqual(fee.sample_metrics(body, FAUCET, 1010), (210, {SERVICE: 184485}))
        for now, faucet in [(1121, FAUCET), (999, FAUCET), (1010, SERVICE)]:
            with self.assertRaises(ValueError):
                fee.sample_metrics(body, faucet, now)

    def test_wrong_container_port_is_rejected(self):
        info = {'NetworkSettings': {'Ports': {'8546/tcp': [{'HostPort': '18546'}]}}}
        fee.validate_binding(info, 'http://localhost:18546', 8546)
        with self.assertRaises(ValueError):
            fee.validate_binding(info, 'http://localhost:8546', 8546)

    def test_missing_network_account_sample_cannot_authorize_stage(self):
        labels = f'account_id="{SERVICE}",fee_faucet="{FAUCET}"'
        body = f'bridge_fee_vault_expected_accounts 3\nbridge_fee_max_per_txn 210\nbridge_fee_vault_balance_by_id{{{labels}}} 1000\nbridge_fee_vault_sample_timestamp_seconds{{{labels}}} 1000\n'
        with self.assertRaises(ValueError):
            fee.sample_metrics(body, FAUCET, 1010)

    def test_nonlocal_funder_is_rejected(self):
        for url in ['https://mainnet.example', 'http://192.168.0.1:8545', 'file:///tmp/test']:
            with self.assertRaises(ValueError):
                fee.local_url(url)

    def test_wrong_chain_or_nonanvil_is_rejected(self):
        original = fee.rpc
        try:
            for chain, client in [(1, 'anvil'), (271828, 'geth')]:
                fee.rpc = lambda _, method: hex(chain) if method == 'eth_chainId' else client
                with self.assertRaises(ValueError):
                    fee.validate_chain('http://localhost:8545', 271828)
        finally:
            fee.rpc = original

    def test_native_registration_excludes_only_externally_owned_accounts(self):
        balances = {SERVICE: 100, GER: 100, FAUCET: 5, NATIVE: 1, UNKNOWN: 2}
        rows = [{'faucet_id': FAUCET, 'origin_network': 2},
                {'faucet_id': NATIVE, 'origin_network': 1}]
        managed, excluded = fee.funding_scope(balances, rows, 1, MANIFEST)
        self.assertEqual(set(managed), {SERVICE, GER, FAUCET, UNKNOWN})
        self.assertEqual(excluded, EXCLUDED)
        required = fee.required_balances(managed, SERVICE, 210, 9, 1024, 30)
        self.assertEqual(required[FAUCET], 157920)
        self.assertEqual(required[UNKNOWN], 157920)
        self.assertEqual(required[SERVICE], 2093280)
        self.assertNotIn(NATIVE, required)

    def test_registry_scope_fails_closed_on_missing_or_conflicting_evidence(self):
        balances = {SERVICE: 100, GER: 100, FAUCET: 100}
        good = {'faucet_id': FAUCET, 'origin_network': 2}
        bad_rows = [None, {}, [None], [{}], [good, good],
                    [{'faucet_id': FAUCET, 'origin_network': True}],
                    [{'faucet_id': FAUCET, 'origin_network': '1'}],
                    [{'faucet_id': FAUCET, 'origin_network': -1}],
                    [{'faucet_id': FAUCET, 'origin_network': 2**32}],
                    [{'faucet_id': 'bad-id', 'origin_network': 1}],
                    [{'faucet_id': NATIVE, 'origin_network': 1}],
                    [{'faucet_id': SERVICE, 'origin_network': 1}],
                    [{'faucet_id': GER, 'origin_network': 2}]]
        for rows in bad_rows:
            with self.subTest(rows=rows), self.assertRaises(ValueError):
                fee.funding_scope(balances, rows, 1, MANIFEST)
        for network in [None, True, '1', -1, 2**32]:
            with self.subTest(network=network), self.assertRaises(ValueError):
                fee.funding_scope(balances, [good], network, MANIFEST)
        with self.assertRaisesRegex(ValueError, 'manifest'):
            fee.funding_scope({SERVICE: 100}, [], 1, MANIFEST)

    def test_scope_reads_actual_network_and_authenticated_registry(self):
        balances = {SERVICE: 100, GER: 100, NATIVE: 100}
        with patch.object(fee, 'rpc', side_effect=[f'0x{1:064x}',
                          [{'faucet_id': NATIVE, 'origin_network': 1}]]) as rpc:
            managed, excluded, network = fee.read_funding_scope(
                'http://localhost:8546', {'ADMIN_API_KEY': 'fixture-key'}, balances, MANIFEST)
        self.assertEqual(network, 1)
        self.assertEqual(excluded, EXCLUDED)
        self.assertEqual(set(managed), {SERVICE, GER})
        self.assertEqual(rpc.call_args_list[0].args[1], 'eth_call')
        self.assertEqual(rpc.call_args_list[1].kwargs, {'token': 'fixture-key'})
        for encoded in [None, '0x1', '0x' + 'g' * 64, f'0x{2**32:064x}']:
            with self.subTest(encoded=encoded), patch.object(fee, 'rpc', return_value=encoded):
                with self.assertRaises(ValueError):
                    fee.read_funding_scope('http://localhost:8546',
                                           {'ADMIN_API_KEY': 'fixture-key'}, balances, MANIFEST)
        with patch.object(fee, 'rpc', return_value=f'0x{1:064x}'):
            with self.assertRaisesRegex(ValueError, 'admin key'):
                fee.read_funding_scope('http://localhost:8546', {}, balances, MANIFEST)

    def test_each_observation_rechecks_account_set_and_ownership(self):
        balances = {SERVICE: 100, GER: 100, FAUCET: 100, NATIVE: 100}
        rows = [{'faucet_id': FAUCET, 'origin_network': 2},
                {'faucet_id': NATIVE, 'origin_network': 1}]
        def scope(current, registry=rows, network=1):
            managed, excluded = fee.funding_scope(current, registry, network, MANIFEST)
            return managed, excluded, network
        expected = scope(balances)
        self.assertEqual(fee.checked_observation(lambda: (210, balances), scope,
                         210, balances.keys(), expected), balances)
        for current, current_scope in [
                ({**balances, UNKNOWN: 100}, scope),
                ({key: value for key, value in balances.items() if key != FAUCET}, scope),
                (balances, lambda current: scope(current, [rows[0]], 1)),
                (balances, lambda current: scope(current, [rows[0],
                    {'faucet_id': NATIVE, 'origin_network': 2}], 1)),
                (balances, lambda current: scope(current, rows, 3))]:
            with self.subTest(current=current, scope=current_scope), self.assertRaises(ValueError):
                fee.checked_observation(lambda: (210, current), current_scope,
                                        210, balances.keys(), expected)

    def test_native_pending_allows_sufficient_stage_without_losing_original(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory)/'intent.json'
            original = self.pending(path, {NATIVE: 26}, {NATIVE: 10, SERVICE: 100})
            # SERVICE was not transferred. Its spending is a new budget issue,
            # never an ambiguous outcome of the old native-only send.
            for _ in range(2):
                result = fee.ensure_budget(path, 'chain', {SERVICE: 10}, 1,
                    lambda: {SERVICE: 10, NATIVE: 0},
                    lambda _: self.fail('resent native transfer'), excluded=EXCLUDED)
                self.assertEqual(result['status'], 'sufficient')
                self.assertTrue(result['pending_outside_scope'])
                self.assertEqual(path.read_bytes(), original)
                archives = list(path.with_name('intent-pending').glob('*.json'))
                self.assertEqual(len(archives), 1)
                self.assertEqual(archives[0].read_bytes(), original)
                self.assertEqual(json.loads(archives[0].read_text())['status'], 'pending')

    def test_partial_observation_survives_sufficient_then_later_managed_spending(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory)/'intent.json'
            original = self.pending(path, {SERVICE: 26, NATIVE: 26})
            fee.ensure_budget(path, 'chain', {SERVICE: 10}, 1,
                lambda: {SERVICE: 10, NATIVE: 0},
                lambda _: self.fail('unexpected send'), excluded=EXCLUDED)
            self.assertEqual(path.read_bytes(), original)
            samples = iter([{SERVICE: 5, NATIVE: 0}, {SERVICE: 20, NATIVE: 0}])
            sends = []
            result = fee.ensure_budget(path, 'chain', {SERVICE: 20}, 1,
                lambda: next(samples), lambda transfers: sends.append(transfers),
                excluded=EXCLUDED)
            self.assertEqual(result['status'], 'observed')
            self.assertEqual(sends, [{SERVICE: 31}])
            self.assertEqual(len(list(path.with_name('intent-pending').glob('*.json'))), 1)
            # A native exclusion is never an acknowledgement. If it stops
            # being authoritative, that archived unobserved target blocks.
            with self.assertRaisesRegex(RuntimeError, 'ambiguous'):
                fee.ensure_budget(path, 'chain', {SERVICE: 20, NATIVE: 10}, 1,
                    lambda: {SERVICE: 20, NATIVE: 0},
                    lambda _: self.fail('resent archived native transfer'))

    def test_managed_or_unknown_pending_still_blocks_before_new_send(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory)/'intent.json'
            for account in [SERVICE, UNKNOWN]:
                self.pending(path, {account: 26, NATIVE: 26})
                with self.subTest(account=account), self.assertRaisesRegex(RuntimeError, 'ambiguous'):
                    fee.ensure_budget(path, 'chain', {SERVICE: 10}, 1,
                        lambda: {SERVICE: 0, NATIVE: 0, UNKNOWN: 0},
                        lambda _: self.fail('resent ambiguous transfer'), excluded=EXCLUDED)

    def test_wrong_fixture_and_corrupt_archives_cannot_authorize_transfer(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory)/'intent.json'
            self.pending(path, {NATIVE: 26})
            archived = fee.archive_pending(path, {NATIVE: 0})
            path.unlink()  # Simulate a later current intent already retired.
            with self.assertRaisesRegex(RuntimeError, 'different fixture'):
                fee.ensure_budget(path, 'other-chain', {SERVICE: 10}, 1,
                    lambda: {SERVICE: 0, NATIVE: 0},
                    lambda _: self.fail('resent on wrong chain'), excluded=EXCLUDED)
            archived.write_text(archived.read_text() + '\n')
            with self.assertRaisesRegex(RuntimeError, 'content mismatch'):
                fee.ensure_budget(path, 'chain', {SERVICE: 10}, 1,
                    lambda: {SERVICE: 0, NATIVE: 0},
                    lambda _: self.fail('sent with corrupted archive'), excluded=EXCLUDED)

    def test_acknowledgement_persistence_failure_prevents_send_and_is_retryable(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory)/'intent.json'
            original = self.pending(path, {SERVICE: 26, NATIVE: 26})
            real_save = fee.save
            def fail_ack(destination, value):
                if destination.suffix == '.observed':
                    raise OSError('ack fsync failed')
                real_save(destination, value)
            with patch.object(fee, 'save', side_effect=fail_ack):
                with self.assertRaisesRegex(OSError, 'ack fsync'):
                    fee.ensure_budget(path, 'chain', {SERVICE: 20}, 1,
                        lambda: {SERVICE: 10, NATIVE: 0},
                        lambda _: self.fail('sent before durable acknowledgement'), excluded=EXCLUDED)
            self.assertEqual(path.read_bytes(), original)
            samples = iter([{SERVICE: 10, NATIVE: 0}, {SERVICE: 20, NATIVE: 0}])
            sends = []
            fee.ensure_budget(path, 'chain', {SERVICE: 20}, 1,
                lambda: next(samples), lambda transfers: sends.append(transfers), excluded=EXCLUDED)
            self.assertEqual(sends, [{SERVICE: 26}])

    def test_missing_referenced_archive_cannot_drop_an_old_pending_transfer(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory)/'intent.json'
            self.pending(path, {NATIVE: 26})
            samples = iter([{SERVICE: 0, NATIVE: 0}, {SERVICE: 10, NATIVE: 0}])
            fee.ensure_budget(path, 'chain', {SERVICE: 10}, 1,
                lambda: next(samples), lambda _: 'sent managed', excluded=EXCLUDED)
            self.assertEqual(json.loads(path.read_text())['status'], 'observed')
            archive = path.with_name('intent-pending')
            next(archive.glob('*.json')).unlink()
            with self.assertRaisesRegex(RuntimeError, 'missing referenced'):
                fee.ensure_budget(path, 'chain', {SERVICE: 20}, 1,
                    lambda: {SERVICE: 10, NATIVE: 0},
                    lambda _: self.fail('sent despite missing historical fence'), excluded=EXCLUDED)
            archive.rmdir()
            archive.write_text('corrupt archive directory')
            with self.assertRaisesRegex(RuntimeError, 'archive directory'):
                fee.ensure_budget(path, 'chain', {SERVICE: 20}, 1,
                    lambda: {SERVICE: 10, NATIVE: 0},
                    lambda _: self.fail('sent despite corrupt archive directory'), excluded=EXCLUDED)

    def test_archive_directory_sync_failure_preserves_current_and_never_sends(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory)/'intent.json'
            original = self.pending(path, {NATIVE: 26})
            real_sync = fee.sync_directory
            def fail_archive(destination):
                if destination.name == 'intent-pending':
                    raise OSError('archive fsync failed')
                real_sync(destination)
            with patch.object(fee, 'sync_directory', side_effect=fail_archive):
                with self.assertRaisesRegex(OSError, 'archive fsync'):
                    fee.ensure_budget(path, 'chain', {SERVICE: 10}, 1,
                        lambda: {SERVICE: 0, NATIVE: 0},
                        lambda _: self.fail('sent before durable archive'), excluded=EXCLUDED)
            self.assertEqual(path.read_bytes(), original)
            samples = iter([{SERVICE: 0, NATIVE: 0}, {SERVICE: 10, NATIVE: 0}])
            sends = []
            fee.ensure_budget(path, 'chain', {SERVICE: 10}, 1,
                lambda: next(samples), lambda transfers: sends.append(transfers), excluded=EXCLUDED)
            self.assertEqual(sends, [{SERVICE: 26}])

    def test_bad_acknowledgements_and_invalid_pending_status_fail_closed(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory)/'intent.json'
            self.pending(path, {SERVICE: 26, NATIVE: 26})
            archived = fee.archive_pending(path, {SERVICE: 10, NATIVE: 0})
            acknowledgement = archived.with_suffix('.observed')
            valid = json.loads(acknowledgement.read_text())
            for change in [{'identity': 'other'}, {'intent_sha256': 'wrong'},
                           {'balances': {NATIVE: 9}}, {'balances': {UNKNOWN: 10}},
                           {'balances': {NATIVE: float('nan')}}]:
                fee.save(acknowledgement, {**valid, **change})
                with self.subTest(change=change), self.assertRaisesRegex(RuntimeError, 'observations'):
                    fee.ensure_budget(path, 'chain', {SERVICE: 20}, 1,
                        lambda: {SERVICE: 10, NATIVE: 0},
                        lambda _: self.fail('sent with bad acknowledgement'), excluded=EXCLUDED)
            acknowledgement.unlink()
            archived.unlink()
            record = json.loads(path.read_text())
            record['status'] = 'unknown'
            fee.save(path, record)
            with self.assertRaisesRegex(RuntimeError, 'status'):
                fee.ensure_budget(path, 'chain', {SERVICE: 20}, 1,
                    lambda: {SERVICE: 10, NATIVE: 0},
                    lambda _: self.fail('sent with invalid status'), excluded=EXCLUDED)

if __name__ == '__main__':
    unittest.main()
