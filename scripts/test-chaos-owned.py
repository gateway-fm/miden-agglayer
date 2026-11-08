#!/usr/bin/env python3
"""Local process tests only: no Docker daemon or live stack is accessed."""
import json
import os
import shlex
from pathlib import Path
import subprocess
import sys
import tempfile
import time
import unittest

SUPERVISOR = Path(__file__).with_name('chaos-owned-process.py')


class OwnedProcessTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.directory = Path(self.tmp.name)
        self.processes = []

    def tearDown(self):
        for process, control in self.processes:
            control.touch()
            try:
                process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                process.terminate()
                process.wait(timeout=5)
        self.tmp.cleanup()

    def launch(self, code, *extra):
        control = self.directory / f'stop-{len(self.processes)}'
        process = subprocess.Popen([sys.executable, str(SUPERVISOR), '--stop-file', str(control),
                                    '--grace', '0.05', *extra, '--', sys.executable, '-c', code])
        self.processes.append((process, control))
        return process, control

    def ready(self, marker):
        deadline = time.monotonic() + 5
        while not marker.exists():
            self.assertLess(time.monotonic(), deadline, 'child never reached the tested boundary')
            time.sleep(0.02)

    def descendant_code(self, ready, late, linger):
        descendant = f'import time; from pathlib import Path; time.sleep(0.7); Path({str(late)!r}).write_text("leaked")'
        return (f'import subprocess,sys,time; from pathlib import Path; '
                f'subprocess.Popen([sys.executable,"-c",{descendant!r}]); '
                f'Path({str(ready)!r}).touch(); time.sleep({linger})')

    def test_cancel_stops_actual_descendant_and_preserves_unrelated_process(self):
        ready, late = self.directory / 'ready', self.directory / 'late'
        outsider = subprocess.Popen([sys.executable, '-c', 'import time; time.sleep(10)'], start_new_session=True)
        try:
            process, control = self.launch(self.descendant_code(ready, late, 10))
            self.ready(ready)
            control.touch()
            process.wait(timeout=5)
            self.assertTrue(json.loads(Path(str(control)+'.stopped').read_text())['cleanup_ok'])
            # Repeated cancellation does not address a saved PID or another group.
            control.touch()
            time.sleep(0.9)
            self.assertFalse(late.exists(), 'descendant wrote after cancellation completed')
            self.assertIsNone(outsider.poll())
        finally:
            outsider.terminate()
            outsider.wait(timeout=5)

    def test_naturally_finished_leader_still_cleans_up_descendants(self):
        ready, late = self.directory / 'natural-ready', self.directory / 'natural-late'
        process, control = self.launch(self.descendant_code(ready, late, 0))
        self.ready(ready)
        self.assertEqual(process.wait(timeout=5), 0)
        control.touch()
        time.sleep(0.9)
        self.assertFalse(late.exists())

    def test_cooperative_drain_finishes_staged_operation_before_exit(self):
        ready, restored = [self.directory / name for name in ('staged', 'restored')]
        watchdog = SUPERVISOR.with_name('aggkit-watchdog.sh')
        attempts = self.directory / 'attempts'
        shell = f"""
source {shlex.quote(str(watchdog))}
sleep() {{ command sleep 0.02; }}
watchdog_tick() {{
    echo tick >> {shlex.quote(str(attempts))}
    touch {shlex.quote(str(ready))}
    command sleep 0.5
    touch {shlex.quote(str(restored))}
}}
WATCHDOG_DIR={shlex.quote(str(self.directory / 'watchdog'))}
WATCHDOG_HEALS_FILE={shlex.quote(str(self.directory / 'heals'))}
watchdog_main
"""
        code = f'import os; os.execvp("bash", ["bash", "-c", {shell!r}])'
        process, control = self.launch(code, '--drain-timeout', '2')
        self.ready(ready)
        control.touch()
        self.assertEqual(process.wait(timeout=5), 0)
        self.assertTrue(restored.exists(), 'cancellation interrupted staged state restoration')
        self.assertEqual(attempts.read_text().splitlines(), ['tick'],
                         'watchdog began another heal after cancellation')
        self.assertTrue(json.loads(Path(str(control)+'.stopped').read_text())['cleanup_ok'])

    def test_nested_supervisor_cancels_worker_session_before_parent_stops(self):
        ready, late = self.directory / 'nested-ready', self.directory / 'nested-late'
        inner_control = self.directory / 'inner-stop'
        inner = [sys.executable, str(SUPERVISOR), '--stop-file', str(inner_control),
                 '--grace', '0.05', '--', sys.executable, '-c', self.descendant_code(ready, late, 10)]
        code = f"""
import signal,subprocess
from pathlib import Path
worker = subprocess.Popen({inner!r})
def stop(*args):
    Path({str(inner_control)!r}).touch()
signal.signal(signal.SIGTERM, stop)
worker.wait()
"""
        process, control = self.launch(code, '--grace', '0.5')
        self.ready(ready)
        control.touch()
        process.wait(timeout=5)
        self.assertTrue(Path(str(inner_control)+'.stopped').exists())
        time.sleep(0.9)
        self.assertFalse(late.exists(), 'nested worker escaped parent cancellation')

    def test_drain_timeout_is_failed_cleanup_evidence(self):
        ready = self.directory / 'hung-ready'
        process, control = self.launch(
            f'from pathlib import Path; import time; Path({str(ready)!r}).touch(); time.sleep(10)',
            '--drain-timeout', '0.1')
        self.ready(ready)
        control.touch()
        process.wait(timeout=5)
        self.assertFalse(json.loads(Path(str(control)+'.stopped').read_text())['cleanup_ok'])


if __name__ == '__main__':
    unittest.main()
