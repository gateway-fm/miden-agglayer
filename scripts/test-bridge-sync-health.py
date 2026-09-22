#!/usr/bin/env python3
import importlib.util
import json
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch, MagicMock

spec = importlib.util.spec_from_file_location('sync_health', Path(__file__).with_name('bridge-sync-health.py'))
health = importlib.util.module_from_spec(spec)
spec.loader.exec_module(health)


def iteration(network, second, block=1, level='INFO'):
    return f'1970-01-01T00:{second // 60:02d}:{second % 60:02d}Z {level} synchronizer/synchronizer.go:714 NetworkID: {network}, [checkReorg function] Checking Block {block}'


class Health(unittest.TestCase):
    def test_l2_logs_cannot_mask_dead_l1(self):
        result = health.progress([iteration(0, 100), iteration(1, 399)], [0, 1], 0, 400)
        self.assertFalse(result['healthy'])
        self.assertEqual(result['stale'], [0])

    def test_empty_chain_old_block_is_healthy_with_fresh_iterations(self):
        result = health.progress([iteration(0, 399, 1), iteration(1, 399, 2)], [0, 1], 0, 400)
        self.assertTrue(result['healthy'])

    def test_errors_and_other_module_activity_are_not_progress(self):
        lines = [iteration(0, 399, level='ERROR'), iteration(1, 399).replace('synchronizer/synchronizer.go', 'claimtxman/manager.go')]
        self.assertFalse(health.progress(lines, [0, 1], 0, 400)['healthy'])

    def test_old_generation_and_future_timestamps_are_rejected(self):
        result = health.progress([iteration(0, 399), iteration(1, 410)], [0, 1], 400, 405)
        self.assertEqual(result['stale'], [0, 1])

    def test_l2b_requires_its_own_l1_and_network_two(self):
        result = health.progress([iteration(0, 399), iteration(1, 399)], [0, 2], 0, 400)
        self.assertEqual(result['stale'], [2])

    def test_pause_startup_and_broken_dependencies_forbid_restart(self):
        report = {'running': True, 'stale': [0], 'process_age': 200}
        self.assertTrue(health.can_restart(report, True))
        self.assertFalse(health.can_restart(report, False))
        self.assertFalse(health.can_restart(dict(report, running=False), True))
        self.assertFalse(health.can_restart(dict(report, process_age=179), True))

    def test_dependency_probes_refuse_paused_db_or_partitioned_node(self):
        def info(port=None):
            return {'Config': {'Labels': {'com.docker.compose.project': 'test'}},
                    'State': {'Status': 'running', 'Paused': False, 'Restarting': False},
                    'NetworkSettings': {'Networks': {'test-net': {}},
                        'Ports': {f'{port}/tcp': [{'HostPort': str(port)}]} if port else {}}}
        containers = {'test-postgres-1': info(), 'test-anvil-1': info(8545),
                      'test-miden-agglayer-1': info(8546), 'test-miden-node-1': info()}
        with patch.object(health, 'inspect', side_effect=lambda name: containers[name]), \
                patch.object(health, 'command', return_value='1'), \
                patch.object(health, 'rpc_ready', return_value=True):
            self.assertTrue(health.dependencies('test', 'test-postgres-1', [8545, 8546]))
            containers['test-postgres-1']['State']['Paused'] = True
            self.assertFalse(health.dependencies('test', 'test-postgres-1', [8545, 8546]))
            containers['test-postgres-1']['State']['Paused'] = False
            containers['test-miden-node-1']['NetworkSettings']['Networks'] = {'unrelated': {}}
            self.assertFalse(health.dependencies('test', 'test-postgres-1', [8545, 8546]))
            containers['test-miden-node-1']['NetworkSettings']['Networks'] = {'test-net': {}}
            containers['test-miden-agglayer-1']['NetworkSettings']['Ports']['8546/tcp'][0]['HostPort'] = '18546'
            self.assertFalse(health.dependencies('test', 'test-postgres-1', [8545, 8546]))

    def test_recovery_rechecks_generation_after_backup(self):
        self.run_recovery(changed=True)

    def test_recovery_restarts_same_container_and_preserves_database(self):
        self.run_recovery(changed=False)

    def run_recovery(self, changed):
        before = {'running': True, 'stale': [0], 'process_age': 200, 'healthy': False,
                  'generation': ['same-id', 'before', 0, 'running', False, False]}
        newer = dict(before, generation=['same-id', 'someone-restarted', 0, 'running', False, False])
        after = dict(newer, healthy=True, stale=[])
        samples = iter([(before, 'before-log'), (newer if changed else before, 'recheck'), (after, 'after-log')])
        process = MagicMock()
        process.returncode = 0
        process.communicate.return_value = (b'preserved database', b'')
        process.__enter__.return_value = process
        calls = []
        with tempfile.TemporaryDirectory() as directory, \
                patch.object(health, 'snapshot', side_effect=lambda *_: next(samples)), \
                patch.object(health, 'dependencies', return_value=True), \
                patch.object(health.subprocess, 'Popen', return_value=process), \
                patch.object(health, 'command', side_effect=lambda *a, **kw: calls.append(a)):
            evidence = Path(directory)/'attempt'
            result = health.recover('test', 'test-bridge-service-1', 'test-postgres-1', [0, 1], [], evidence)
            self.assertTrue((evidence/'bridge-db.sql.gz').exists())
            if changed:
                self.assertEqual(result, 'skipped')
                self.assertEqual(calls, [])
            else:
                self.assertEqual(result, 'iterations-resumed')
                self.assertEqual(calls, [('docker', 'restart', '--time', '20', 'test-bridge-service-1')])
                self.assertEqual(json.loads((evidence/'after.json').read_text())['generation'][0], 'same-id')

if __name__ == '__main__':
    unittest.main()
