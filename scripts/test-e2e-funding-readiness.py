#!/usr/bin/env python3
import importlib.util
from pathlib import Path
import unittest
import urllib.error

spec = importlib.util.spec_from_file_location('funding', Path(__file__).with_name('e2e-funding-readiness.py'))
funding = importlib.util.module_from_spec(spec)
spec.loader.exec_module(funding)
TX = '0x' + 'a' * 64
DEST = '0x' + 'b' * 40


def deposit(**changes):
    return dict(tx_hash=TX, network_id=0, dest_net=1, dest_addr=DEST,
                amount='100', ready_for_claim=False, **changes)


class FundingReadiness(unittest.TestCase):
    def test_old_ready_deposit_cannot_satisfy_current_funding(self):
        old = deposit(); old.update(tx_hash='0x'+'c'*64, ready_for_claim=True)
        result = funding.funding_status([old], {TX}, DEST)
        self.assertEqual((result['indexed'], result['ready']), (0, 0))

    def test_wrong_network_destination_amount_or_duplicate_is_rejected(self):
        for changes in [{'network_id': 2}, {'dest_net': 2}, {'dest_addr': '0x'+'c'*40},
                        {'amount': '0'}, {'ready_for_claim': 'true'}]:
            row = deposit(); row.update(changes)
            with self.assertRaises(ValueError):
                funding.funding_status([row], {TX}, DEST)
        with self.assertRaises(ValueError):
            funding.funding_status([deposit(), deposit()], {TX}, DEST)

    def run_wait(self, sample, nudge, timeout=200):
        now = [0]
        records = []
        def pause(seconds): now[0] += seconds
        return funding.wait_ready(sample, nudge, records.append, timeout,
                                  lambda: now[0], pause), records, now[0]

    def test_ready_funding_needs_no_nudge(self):
        row = deposit(); row['ready_for_claim'] = True
        result, records, elapsed = self.run_wait(
            lambda: funding.funding_status([row], {TX}, DEST),
            lambda _: self.fail('unnecessary nudge'))
        self.assertEqual(result['nudge_attempts'], 0)
        self.assertEqual(elapsed, 0)

    def test_missed_notification_nudges_after_grace_then_rechecks(self):
        row = deposit(); attempts = []
        def nudge(attempt):
            attempts.append(attempt); row['ready_for_claim'] = True
        result, records, elapsed = self.run_wait(
            lambda: funding.funding_status([row], {TX}, DEST), nudge)
        self.assertEqual(attempts, [1])
        self.assertEqual(result['status'], 'funding-ready')
        self.assertGreaterEqual(elapsed, 60)
        self.assertEqual([r['event'] for r in records if r['event'] != 'sample'],
                         ['nudge-intent', 'nudge-submitted'])

    def test_missing_indexed_deposit_times_out_without_sending(self):
        with self.assertRaisesRegex(RuntimeError, 'deadline'):
            self.run_wait(lambda: funding.funding_status([], {TX}, DEST),
                          lambda _: self.fail('cannot nudge unindexed deposits'))

    def test_unreadable_evidence_cannot_authorize_nudge(self):
        def sample(): raise ValueError('malformed API')
        with self.assertRaises(ValueError):
            self.run_wait(sample, lambda _: self.fail('unexpected send'))

    def test_ambiguous_nudge_is_not_retried(self):
        attempts = []
        def nudge(attempt):
            attempts.append(attempt); raise TimeoutError('unknown send outcome')
        with self.assertRaises(TimeoutError):
            self.run_wait(lambda: funding.funding_status([deposit()], {TX}, DEST), nudge)
        self.assertEqual(attempts, [1])

    def test_temporary_api_outage_retries_without_authorizing_a_send(self):
        row = deposit(); row['ready_for_claim'] = True
        samples = [urllib.error.URLError('connection refused'), funding.Unavailable('paused'), row]
        def sample():
            value = samples.pop(0)
            if isinstance(value, Exception): raise value
            return funding.funding_status([value], {TX}, DEST)
        result, records, elapsed = self.run_wait(sample, lambda _: self.fail('unexpected send'))
        self.assertEqual(result['status'], 'funding-ready')
        self.assertEqual(elapsed, 10)
        self.assertEqual(sum(r['event'] == 'sample-unavailable' for r in records), 2)

    def test_unavailable_dependency_skips_without_consuming_send_budget(self):
        row = deposit(); candidates = []
        def nudge(attempt):
            candidates.append(attempt)
            if len(candidates) == 1: return False
            row['ready_for_claim'] = True
        result, records, _ = self.run_wait(lambda: funding.funding_status([row], {TX}, DEST), nudge)
        self.assertEqual(candidates, [1, 1])
        self.assertEqual(result['nudge_attempts'], 1)
        self.assertEqual(sum(r['event'] == 'nudge-skipped' for r in records), 1)

    def test_persistent_outage_still_expires_at_original_deadline(self):
        def sample(): raise funding.Unavailable('paused')
        with self.assertRaisesRegex(RuntimeError, 'deadline'):
            self.run_wait(sample, lambda _: self.fail('unexpected send'), timeout=20)

    def test_nudge_budget_does_not_turn_unready_into_success(self):
        attempts = []
        with self.assertRaisesRegex(RuntimeError, 'deadline'):
            self.run_wait(lambda: funding.funding_status([deposit()], {TX}, DEST),
                          attempts.append, timeout=1000)
        self.assertEqual(attempts, list(range(1, 7)))


if __name__ == '__main__':
    unittest.main()
