#!/usr/bin/env python3
"""Run one chaos command in an owned session, including its descendants.

Cancellation uses a unique stop file, never a numeric PID retained after exit.
waitid(WNOWAIT) keeps the session leader unreaped until group cleanup finishes,
so its process-group ID cannot be reused for an unrelated process meanwhile.
"""
import argparse
import json
import os
from pathlib import Path
import signal
import subprocess
import time


def signal_owned_group(pgid, sig):
    try:
        os.killpg(pgid, sig)
    except ProcessLookupError:
        pass
    except PermissionError:
        # Darwin reports EPERM, not ESRCH, for an all-zombie process group.
        # Do not hide a real denial: any live member makes cleanup fail closed.
        rows = subprocess.check_output(['ps', '-axo', 'pgid=,stat='], text=True)
        if any(fields[0] == str(pgid) and not fields[1].startswith('Z')
               for row in rows.splitlines() if len(fields := row.split()) == 2):
            raise


def run(args):
    stopping = False

    def stop(_signum, _frame):
        nonlocal stopping
        stopping = True

    signal.signal(signal.SIGTERM, stop)
    signal.signal(signal.SIGINT, stop)
    control = Path(args.stop_file)
    child = None
    status = 0
    drain_timed_out = False
    drain_deadline = None
    try:
        if not stopping and not control.exists():
            environment = dict(os.environ, CHAOS_STOP_FILE=str(control))
            child = subprocess.Popen(args.command, start_new_session=True, env=environment)
            while True:
                result = os.waitid(os.P_PID, child.pid, os.WEXITED | os.WNOHANG | os.WNOWAIT)
                if result is not None:
                    break
                if stopping or control.exists():
                    control.touch(exist_ok=True)
                    if not args.drain_timeout:
                        break
                    if drain_deadline is None:
                        drain_deadline = time.monotonic() + args.drain_timeout
                    if time.monotonic() >= drain_deadline:
                        drain_timed_out = True
                        break
                time.sleep(0.1)
    finally:
        if child is not None:
            # The unreaped leader pins this group identity throughout cleanup.
            for sig, delay in [(signal.SIGTERM, args.grace), (signal.SIGKILL, 0)]:
                signal_owned_group(child.pid, sig)
                if delay:
                    time.sleep(delay)
            status = child.wait()
        Path(str(control) + '.stopped').write_text(json.dumps({'status': status, 'cleanup_ok': not drain_timed_out}) + '\n')
    return status if status >= 0 else 128 - status


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--stop-file', required=True)
    parser.add_argument('--grace', type=float, default=2)
    parser.add_argument('--drain-timeout', type=float, default=0,
                        help='let a cooperative command finish its current bounded operation')
    parser.add_argument('command', nargs=argparse.REMAINDER)
    args = parser.parse_args()
    if args.command[:1] == ['--']:
        args.command.pop(0)
    if not args.command or args.grace < 0 or args.drain_timeout < 0:
        parser.error('a command and nonnegative grace are required')
    raise SystemExit(run(args))


if __name__ == '__main__':
    main()
