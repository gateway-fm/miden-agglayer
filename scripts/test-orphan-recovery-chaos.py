#!/usr/bin/env python3
"""Exercise the real chaos wrapper and deposit flow with local command doubles.

The submission probe becomes true after 30 five-second polling attempts: later
than the normal 120-second gate, but within the documented chaos recovery window.
No Docker stack, RPC connection, or transaction is used by these tests.
"""
import json
import os
from pathlib import Path
import shutil
import signal
import subprocess
import tempfile
import unittest


HERE = Path(__file__).resolve().parent
TIMEOUT = shutil.which("timeout")
MOCK = r'''#!/usr/bin/env python3
import fcntl, json, os, pathlib, sys, tempfile, time
name = pathlib.Path(sys.argv[0]).name
args = sys.argv[1:]
root = pathlib.Path(os.environ["MOCK_ROOT"])
if name == "mktemp":
    # BSD mktemp does not support the Linux suffix template used by the harness.
    fd, path = tempfile.mkstemp(prefix="flow-", suffix=".log", dir=root)
    os.close(fd)
    print(path)
    sys.exit(0)
if name == "timeout":
    (root/"timeout-args.json").write_text(json.dumps(args))
    if os.environ.get("TEST_SHORT_DEADLINE"):
        args = ["1s" if a == "900s" else a for a in args]
    os.execv(os.environ["REAL_TIMEOUT"], ["timeout", *args])
with (root/"lock").open("w") as lock:
    fcntl.flock(lock, fcntl.LOCK_EX)
    path = root/"state.json"
    s = json.loads(path.read_text()) if path.exists() else {}
    def count(key):
        s[key] = s.get(key, 0) + 1
        return s[key]
    if name == "docker":
        if args[0] == "kill": count("faults")
        elif args[0] == "logs":
            if os.environ.get("TEST_HANG_PROBE"):
                time.sleep(30)
            attempt = count("submission_probes")
            if attempt >= 30 or s.get("deposits", 0) > 1:
                print("submitted claim note txn")
                print("claim tx committed to block")
        elif "cat" in args:
            print('bridge = "0xbridge"\nfaucet_eth = "0xfaucet"')
        elif "psql" in args:
            print(os.environ.get("TEST_ORPHANS", "0"))
    elif name == "cast":
        if args[0] == "send":
            count("deposits")
            print("status 1 (success)")
        else: print("100")
    elif name == "curl":
        if any("/metrics" in a for a in args):
            print("orphan_recovery_successes_total 0")
            print("orphan_recovery_redrives_total 0")
            print("orphan_recovery_already_claimed_total 0")
        elif any("eth_blockNumber" in a for a in args):
            print('{"result":"0x100"}')
        else: print('{"deposits":[{"ready_for_claim":true,"amount":"1"}]}')
    elif name == "mock-wallet":
        if args[0] == "balance":
            attempt = count("balances")
            print(500 if attempt % 2 else (2000 if os.environ.get("TEST_DOUBLE_CREDIT") else 1500))
        elif args[0] == "metadata": count("metadata_assertions")
        elif args[0] == "withdrawal": count("withdrawals")
    path.write_text(json.dumps(s))
'''


@unittest.skipUnless(TIMEOUT, "GNU timeout is required by the chaos harness")
class OrphanRecoveryChaosTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        self.scripts = self.root / "scripts"
        self.scripts.mkdir()
        (self.root / "fixtures").mkdir()
        (self.root / "fixtures/.env").write_text("BRIDGE_ADDRESS=0xbridge\n")
        for name in ["e2e-orphan-recovery-chaos.sh", "e2e-l1-to-l2.sh"]:
            shutil.copyfile(HERE / name, self.scripts / name)
        (self.scripts / "lib-isolated-wallet.sh").write_text('''
provision_isolated_wallet() { WALLET_ID=0xwallet; DEST_ADDR=0xwallet; }
iso_wallet_balance() { mock-wallet balance; }
iso_wallet_faucets() { echo '{}'; }
assert_received_faucet() { mock-wallet metadata; }
''')
        (self.scripts / "e2e-l2-to-l1.sh").write_text("mock-wallet withdrawal\n")
        bindir = self.root / "bin"
        bindir.mkdir()
        for name in ["docker", "cast", "curl", "sleep", "mock-wallet", "timeout", "mktemp"]:
            path = bindir / name
            path.write_text(MOCK)
            path.chmod(0o755)
        self.env = {**os.environ, "PATH": str(bindir) + os.pathsep + os.environ["PATH"],
                    "MOCK_ROOT": str(self.root), "REAL_TIMEOUT": TIMEOUT,
                    "COMPOSE_PROJECT_NAME": "unit-chaos"}
        for key in ["CLAIM_SUBMIT_TIMEOUT", "CLAIM_COMMIT_TIMEOUT", "BALANCE_ATTEMPTS",
                    "RECV_POLL_TRIES", "RECV_POLL_INTERVAL"]:
            self.env.pop(key, None)

    def run_flow(self, script="e2e-orphan-recovery-chaos.sh", **env):
        process = subprocess.Popen(["bash", str(self.scripts / script)],
                                   env={**self.env, **env}, stdout=subprocess.PIPE,
                                   stderr=subprocess.STDOUT, text=True, start_new_session=True)
        try:
            output, _ = process.communicate(timeout=20)
            return process.returncode, output
        finally:
            # A failed regression must not leave its mock subprocesses behind.
            try:
                os.killpg(process.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            process.wait()

    def state(self):
        return json.loads((self.root / "state.json").read_text())

    def test_chaos_recovery_outlasts_normal_submission_gate(self):
        rc, output = self.run_flow()
        self.assertEqual(rc, 0, output)
        state = self.state()
        self.assertEqual(state["faults"], 6)
        self.assertEqual(state["deposits"], 3)  # chaos + two fresh liveness deposits
        self.assertEqual(state["withdrawals"], 2)
        self.assertEqual(state["metadata_assertions"], 3)
        self.assertIn("deposit CLAIMED EXACTLY ONCE", output)

    def test_ordinary_flow_keeps_120_second_submission_gate(self):
        rc, output = self.run_flow("e2e-l1-to-l2.sh")
        self.assertNotEqual(rc, 0, output)
        self.assertIn("Timed out: claim tx submitted", output)
        self.assertEqual(self.state()["submission_probes"], 24)

    def test_recovery_does_not_accept_double_credit(self):
        rc, output = self.run_flow(TEST_DOUBLE_CREDIT="1")
        self.assertNotEqual(rc, 0, output)
        self.assertIn("Balance change mismatch", output)
        self.assertNotIn("withdrawals", self.state())

    def test_pending_orphan_still_fails_before_liveness(self):
        rc, output = self.run_flow(TEST_ORPHANS="1")
        self.assertNotEqual(rc, 0, output)
        self.assertIn("remain unrecovered after the chaos", output)
        self.assertNotIn("withdrawals", self.state())

    def test_one_deadline_bounds_a_blocked_probe(self):
        rc, output = self.run_flow(TEST_SHORT_DEADLINE="1", TEST_HANG_PROBE="1")
        self.assertNotEqual(rc, 0, output)
        self.assertIn("l1-to-l2 rc=124", output)
        self.assertNotIn("withdrawals", self.state())
        args = json.loads((self.root / "timeout-args.json").read_text())
        self.assertIn("900s", args)  # production deadline; shortened only by the double
        self.assertIn("--kill-after=10s", args)

    def test_success_during_timeout_cleanup_is_still_failure(self):
        (self.scripts / "e2e-l1-to-l2.sh").write_text(
            "trap 'exit 0' TERM\nwhile :; do /bin/sleep 30; done\n")
        rc, output = self.run_flow(TEST_SHORT_DEADLINE="1")
        self.assertNotEqual(rc, 0, output)
        self.assertIn("l1-to-l2 rc=124", output)
        self.assertNotIn("withdrawals", self.state())


if __name__ == "__main__":
    unittest.main()
