#!/usr/bin/env python3
import importlib.util
import json
from pathlib import Path
import tempfile
import unittest

spec = importlib.util.spec_from_file_location('fee_budget', Path(__file__).with_name('e2e-fee-budget.py'))
fee = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fee)
SERVICE = '0x' + 'a' * 30
FAUCET = '0x' + 'b' * 30


class Budget(unittest.TestCase):
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

if __name__ == '__main__':
    unittest.main()
