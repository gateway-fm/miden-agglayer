#!/usr/bin/env python3
"""Regression coverage for the Sep 18 soak: wrong identities and a recovered restart.

All Docker calls are substituted. The full healer cases run on Linux because
its metadata round-trip uses GNU find and flock, just like the SSH runner.
"""
import datetime
import importlib.util
import json
import os
from pathlib import Path
import platform
import re
import shutil
import subprocess
import tempfile
import time
import unittest

SCRIPTS = Path(__file__).resolve().parent
spec = importlib.util.spec_from_file_location("aggkit_evidence", SCRIPTS / "aggkit-log-evidence.py")
parser = importlib.util.module_from_spec(spec)
spec.loader.exec_module(parser)
BASH = shutil.which("bash")
MONITOR = "0x51211c69b95d4ed609e9fe91d9c91080228842e79c541e60dc033d74486ccce0"
SIGNED = "0x2adce5394f0316e843ace49fbe9f6c19c0f64f675a32a89b1f30123e3718d9d1"
CERT = "0x944336df53ee5afd1fc85fd9614c9b4f6fadb3dedb195492d92cf0eeee020107"
GER = "0x7f8954443cc7cf0b49c58ba73328356e8c2b0a06fce99f7961d736849328bdd0"


def line(t, message, **metadata):
    timestamp = datetime.datetime.fromtimestamp(t, datetime.timezone.utc).isoformat().replace("+00:00", "Z")
    return f"{timestamp} INFO {message}\t{json.dumps(metadata)}\n"


def injection(t, monitor=MONITOR, ger=GER, submitted=False):
    kind = "submitted" if submitted else "already exists in monitoring DB"
    return line(t, f"inject GER transaction {kind} with ID: {monitor}. GER: {ger}", module="aggoracle")


def broadcast(t, signed=SIGNED, monitor=MONITOR):
    return line(t, f"signed tx sent to the network: {signed}", monitoredTxId=monitor)


def fixture(now=1000):
    return [broadcast(now - 110)] + [injection(now - 100 + i * 10) for i in range(10)] + [
        line(now - 1, f"recovery: last certificate from AggLayer: Height: 155, CertificateID: {CERT}", module="aggsender")
    ]


class Parsing(unittest.TestCase):
    def test_reproduces_old_certificate_selector_and_resolves_signed_hash(self):
        lines = fixture()
        self.assertEqual(re.findall(r"ID: (0x[a-f0-9]{64})", "".join(lines))[-1], CERT)
        actual = parser.evidence(lines, 1000)
        self.assertEqual(actual[:3], [MONITOR, GER, SIGNED])
        self.assertEqual(actual[-1], "candidate")

    def test_actual_last_attempt_burst_is_not_an_aged_wedge(self):
        lines = [broadcast(973)] + [injection(989 + i) for i in range(11)]
        self.assertEqual(parser.evidence(lines, 1000)[-1], "too-recent")

    def test_certificate_only_is_never_an_injection(self):
        self.assertEqual(parser.evidence(fixture()[-1:], 1000)[-1], "no-injection")

    def test_unrelated_warning_cannot_supply_an_injection_id(self):
        text = line(990, f"already exists in monitoring DB; CertificateID: {CERT}", module="aggoracle")
        self.assertEqual(parser.evidence([text], 1000)[-1], "no-injection")

    def test_many_different_ids_do_not_count_as_one_wedge(self):
        lines = [injection(890+i*10, monitor=f"0x{i:064x}") for i in range(10)]
        self.assertEqual(parser.evidence(lines, 1000)[-1], "insufficient-repeats")

    def test_unmapped_monitor_fails_closed(self):
        result = parser.evidence(fixture()[1:], 1000)
        self.assertEqual(result[2], "-")
        self.assertEqual(result[-1], "no-signed-hash")

    def test_unrelated_broadcast_cannot_supply_signed_hash(self):
        lines = fixture()[1:] + [broadcast(999, monitor=CERT)]
        self.assertEqual(parser.evidence(lines, 1000)[-1], "no-signed-hash")

    def test_all_replacement_hashes_are_probed(self):
        result = parser.evidence(fixture() + [broadcast(999, signed="0x" + "a"*64)], 1000)
        self.assertEqual(set(result[2].split(",")), {SIGNED, "0x" + "a"*64})

    def test_stale_and_future_evidence_cannot_authorize_heal(self):
        self.assertEqual(parser.evidence(fixture(), 1100)[-1], "no-recent-retries")
        self.assertEqual(parser.evidence(fixture(1200), 1000)[-1], "no-injection")

    def test_ambiguous_monitor_fails_closed(self):
        self.assertEqual(parser.evidence(fixture()+[injection(999, ger=CERT)], 1000)[-1], "ambiguous-monitor")

    def test_post_heal_submitted_injection_proves_mapping_without_dedup(self):
        result = parser.evidence([injection(990, submitted=True), broadcast(991)], 1000, latest=True)
        self.assertEqual(result[:3], [MONITOR, GER, SIGNED])
        self.assertEqual(result[-1], "candidate")

    def test_malformed_logs_and_partial_hashes_are_not_evidence(self):
        self.assertEqual(parser.evidence(["garbage", injection(990).replace(MONITOR, MONITOR+"a")], 1000)[-1], "no-injection")


class ShellChecks(unittest.TestCase):
    def run_shell(self, body, env=None):
        return subprocess.run([BASH, "-c", 'set -uo pipefail\nsource "$LIB"\nlog() { echo "$*"; }\n'+body],
                              env={**os.environ, "LIB": str(SCRIPTS/"lib-aggkit-recovery.sh"), **(env or {})},
                              text=True, capture_output=True, timeout=10)

    def probe(self, known, lines=None, fail=False):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory)
            (path/"input").write_text("".join(lines or fixture(time.time())))
            r = self.run_shell('''docker() {
  if [[ "$1" == logs ]]; then cat "$DIR/input"; return; fi
  if [[ "$1" == exec ]]; then
    printf '%s\\n' "$@" > "$DIR/query"
    [[ "$FAIL_PROBE" == 0 ]] || return 1
    echo "$KNOWN"; return
  fi
  echo "UNEXPECTED MUTATION $*" >&2; return 99
}
aggkit_probe_wedge aggkit pg "$DIR/snapshot"
''', {"DIR": directory, "KNOWN": known, "FAIL_PROBE": str(int(fail))})
            return r, (path/"query").read_text() if (path/"query").exists() else ""

    def test_known_signed_hash_skips_heal_even_when_monitor_is_unknown(self):
        r, query = self.probe("1")
        self.assertEqual(r.returncode, 2, r.stdout+r.stderr)
        self.assertIn(SIGNED, query)
        self.assertNotIn(MONITOR, query)
        self.assertNotIn(CERT, query)

    def test_real_absent_signed_hash_authorizes_heal(self):
        r, _ = self.probe("0")
        self.assertEqual(r.returncode, 0, r.stdout+r.stderr)

    def test_failed_or_invalid_database_probe_never_authorizes_heal(self):
        for known, fail in [("0", True), ("", False), ("error", False)]:
            with self.subTest(known=known, fail=fail):
                r, _ = self.probe(known, fail=fail)
                self.assertEqual(r.returncode, 1, r.stdout+r.stderr)

    def test_missing_mapping_skips_database_probe(self):
        r, query = self.probe("0", fixture(time.time())[1:])
        self.assertEqual(r.returncode, 2, r.stdout+r.stderr)
        self.assertEqual(query, "")

    def test_hash_list_is_validated_and_quoted(self):
        r = self.run_shell('docker() { printf "%s\\n" "$@"; }; aggkit_known_hashes pg "$HASHES"',
                           {"HASHES": SIGNED+","+CERT})
        self.assertIn(f"IN ('{SIGNED}','{CERT}')", r.stdout)
        r = self.run_shell('docker() { echo UNSAFE; }; aggkit_known_hashes pg "x\x27);DROP TABLE transactions;--"')
        self.assertNotEqual(r.returncode, 0)
        self.assertNotIn("UNSAFE", r.stdout)

    def health(self, scenario):
        return self.run_shell('''CLOCK=1000
date() { echo "$CLOCK"; }
sleep() { CLOCK=$((CLOCK+$1)); }
docker() {
  if [[ "$1" == inspect ]]; then
    case "$SCENARIO" in
      outage) if ((CLOCK<1010)); then echo 'id restarting true 4 start1'; else echo 'id running false 5 start2'; fi ;;
      loop) echo "id running false $((CLOCK/5)) start$CLOCK" ;;
      unavailable) return 1 ;;
      *) echo 'id running false 0 start1' ;;
    esac
  elif [[ "$1" == logs ]]; then
    case "$SCENARIO" in
      outage) if (($3<1010)); then echo 'FATAL old startup'; else echo 'INFO recovered'; fi ;;
      fatal) echo 'FATAL current process' ;;
      empty) : ;;
      logfail) echo 'INFO but incomplete'; return 1 ;;
      *) echo 'INFO processing' ;;
    esac
  else echo "UNEXPECTED MUTATION $*" >&2; return 99
  fi
}
if aggkit_wait_stable aggkit 60 25; then rc=0; else rc=$?; fi
echo "CLOCK=$CLOCK"
exit "$rc"
''', {"SCENARIO": scenario})

    def test_transient_restart_gets_a_fresh_stable_window(self):
        r = self.health("outage")
        self.assertEqual(r.returncode, 0, r.stdout+r.stderr)
        self.assertIn("CLOCK=1035", r.stdout)
        self.assertNotIn("UNEXPECTED MUTATION", r.stderr)

    def test_real_health_failures_still_fail_without_stopping_service(self):
        for scenario in ["loop", "fatal", "empty", "logfail", "unavailable"]:
            with self.subTest(scenario=scenario):
                r = self.health(scenario)
                self.assertEqual(r.returncode, 1, r.stdout+r.stderr)
                self.assertIn("health=timeout", r.stdout)
                self.assertNotIn("UNEXPECTED MUTATION", r.stderr)


class Watchdog(unittest.TestCase):
    def scenario(self, rc, *, repeated=False, unavailable=False):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            healer = root/"aggkit-preserve-heal.sh"
            healer.write_text('#!/usr/bin/env bash\necho "helper diagnostic FORCE=${FORCE:-unset}"\nexit "$MOCK_RC"\n')
            healer.chmod(0o755)
            body = '''set -uo pipefail
source "$WATCHDOG_SCRIPT"
SCRIPT_DIR="$DIR"
WATCHDOG_DIR="$DIR/evidence"; mkdir "$WATCHDOG_DIR"
WATCHDOG_HEALS_FILE="$DIR/ledger"; : > "$WATCHDOG_HEALS_FILE"
AK=aggkit; PG=pg; PROJECT=test; WATCHDOG_MAX_ATTEMPTS=2
attempts=0; budget_reported=0; declare -A seen=()
docker() { [[ "$1" == inspect ]] || return 99; echo '{"Status":"running"}'; }
aggkit_probe_wedge() {
  [[ "$UNAVAILABLE" == 0 ]] || return 1
  WEDGE_MONITOR=monitor; WEDGE_GER=ger
  echo 'trigger log retained' > "$3"
}
for value in a b c; do
  WEDGE_HASHES="$value"
  [[ "$REPEATED" == 0 ]] || WEDGE_HASHES=a
  watchdog_tick || exit 99
done
cat "$WATCHDOG_HEALS_FILE"
'''
            env = {**os.environ, "WATCHDOG_SCRIPT": str(SCRIPTS/"aggkit-watchdog.sh"), "DIR": directory,
                   "MOCK_RC": str(rc), "REPEATED": str(int(repeated)), "UNAVAILABLE": str(int(unavailable))}
            # A caller's FORCE must not bypass the automatic precheck.
            env["FORCE"] = "1"
            r = subprocess.run([BASH, "-c", body], env=env, text=True, capture_output=True, timeout=10)
            logs = [f.read_text() for f in (root/"evidence").glob('attempt-*/heal.log')]
            for attempt in (root/"evidence").glob('attempt-*'):
                self.assertTrue((attempt/"before-state.json").exists())
                self.assertTrue((attempt/"container-state.json").exists())
                self.assertTrue((attempt/"container-generation.txt").exists())
            return r, logs, (root/"ledger").read_text()

    def test_failed_attempts_are_bounded_and_diagnostics_retained(self):
        r, logs, ledger = self.scenario(1)
        self.assertEqual(r.returncode, 0, r.stdout+r.stderr)
        self.assertEqual(len(logs), 2)
        self.assertEqual(ledger.count("WATCHDOG-FAILED:"), 2)
        self.assertEqual(ledger.count("WATCHDOG-BUDGET-EXHAUSTED:"), 1)
        self.assertTrue(all("FORCE=0" in log for log in logs))

    def test_no_repeat_recovery_for_same_signed_hash(self):
        _, logs, ledger = self.scenario(0, repeated=True)
        self.assertEqual(len(logs), 1)
        self.assertEqual(ledger.count("WATCHDOG:"), 1)

    def test_revalidation_skip_is_neither_success_nor_failure(self):
        _, _, ledger = self.scenario(2, repeated=True)
        self.assertIn("WATCHDOG-SKIPPED:", ledger)
        self.assertNotIn("WATCHDOG:", ledger)
        self.assertNotIn("WATCHDOG-FAILED:", ledger)

    def test_unavailable_probe_never_invokes_healer(self):
        _, logs, ledger = self.scenario(0, unavailable=True)
        self.assertEqual(logs, [])
        self.assertEqual(ledger, "")


@unittest.skipUnless(platform.system() == "Linux", "full healer metadata checks require GNU find/flock")
class FullHealer(unittest.TestCase):
    def run_healer(self, scenario, deferred=False):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root/"bin").mkdir(); (root/"container").mkdir(); (root/"stages").mkdir()
            for name in ["aggsender.sqlite", "bridgel2sync.sqlite", "ethtxmanager-aggoracle.sqlite"]:
                (root/"container"/name).write_text(name)
            (root/"clock").write_text("1000")
            for name in ["docker", "date", "sleep"]:
                dest = root/"bin"/name
                dest.write_text(MOCK)
                dest.chmod(0o755)
            env = {**os.environ, "PATH": f"{root}/bin:"+os.environ["PATH"], "MOCK_ROOT": str(root),
                   "SCENARIO": scenario, "FORCE": "1", "PROJECT": f"test-heal-{os.getpid()}",
                   "TMPDIR": str(root/"stages"), "HEAL_CONFIRM_TIMEOUT": "5", "HEAL_HEALTH_TIMEOUT": "60",
                   "HEAL_DIAGNOSTIC_DIR": str(root/"diagnostics"), "HEAL_ALLOW_DEFERRED_PROOF": str(int(deferred)),
                   "MOCK_MONITOR": MONITOR, "MOCK_SIGNED": SIGNED, "MOCK_GER": GER}
            r = subprocess.run([BASH, str(SCRIPTS/"aggkit-preserve-heal.sh"), "aggkit"], env=env,
                               text=True, capture_output=True, timeout=20)
            actions = (root/"actions").read_text()
            diagnostics = (root/"diagnostics/container.log.gz").exists()
            return r, actions, diagnostics

    def test_restored_service_survives_restart_and_confirms_real_signed_hash(self):
        r, actions, diagnostics = self.run_healer("recovered")
        self.assertEqual(r.returncode, 0, r.stdout+r.stderr)
        self.assertEqual(actions.count('"stop"'), 1, actions) # only the initial snapshot stop
        self.assertIn("positive exact outcome", r.stdout)
        queries = "\n".join(l for l in actions.splitlines() if '"exec"' in l)
        self.assertIn(SIGNED, queries)
        self.assertNotIn(MONITOR, queries)
        self.assertTrue(diagnostics)

    def test_no_positive_admission_fails_and_keeps_service_running(self):
        r, actions, diagnostics = self.run_healer("unadmitted")
        self.assertEqual(r.returncode, 1, r.stdout+r.stderr)
        self.assertEqual(actions.count('"stop"'), 1, actions)
        self.assertTrue(diagnostics)

    def test_full_db_loss_quiet_stack_keeps_distinct_deferred_exit(self):
        r, actions, _ = self.run_healer("quiet", deferred=True)
        self.assertEqual(r.returncode, 3, r.stdout+r.stderr)
        self.assertIn("UNPROVEN", r.stdout)
        self.assertEqual(actions.count('"stop"'), 1)

    def test_permanent_crash_loop_cannot_pass_even_with_deferred_proof(self):
        r, actions, diagnostics = self.run_healer("loop", deferred=True)
        self.assertEqual(r.returncode, 1, r.stdout+r.stderr)
        self.assertIn("health=timeout", r.stdout)
        self.assertEqual(actions.count('"stop"'), 1)
        self.assertTrue(diagnostics)

    def test_corrupt_restore_cannot_start_service(self):
        r, actions, diagnostics = self.run_healer("corrupt")
        self.assertEqual(r.returncode, 1, r.stdout+r.stderr)
        self.assertIn("restored state differs", r.stdout)
        self.assertNotIn('"start"', actions)
        self.assertTrue(diagnostics)


MOCK = r'''#!/usr/bin/env python3
import datetime,json,os,shutil,sys
from pathlib import Path
root=Path(os.environ['MOCK_ROOT']); name=Path(sys.argv[0]).name; args=sys.argv[1:]
clock=int((root/'clock').read_text()); scenario=os.environ['SCENARIO']
if name=='date':
 if args==['+%s']: print(clock)
 else: print('MOCK-TIME')
 sys.exit(0)
if name=='sleep': (root/'clock').write_text(str(clock+int(args[0]))); sys.exit(0)
with (root/'actions').open('a') as f: f.write(json.dumps(args)+'\n')
if args[0]=='ps': sys.exit(0)
if args[0]=='inspect':
 fmt=args[args.index('-f')+1] if '-f' in args else ''
 if 'Labels' in fmt: print(os.environ['PROJECT'])
 elif 'json .State' in fmt: print(json.dumps({'Status':'running'}))
 elif '.Id' in fmt:
  count=(clock-1000)//5 if scenario=='loop' else (5 if clock>=1010 else 0)
  start='start2' if clock>=1010 else 'start1'
  print(f'id running false {count} {start}')
 elif 'State.Status' in fmt: print('exited')
 else: print('{}')
elif args[0]=='cp':
 src,dst=args[-2:]
 if ':' in src: shutil.copytree(root/'container',dst,dirs_exist_ok=True)
 else:
  shutil.copytree(src,root/'container',dirs_exist_ok=True)
  if scenario=='corrupt': (root/'container/aggsender.sqlite').write_text('CORRUPTED')
elif args[0]=='compose':
 shutil.rmtree(root/'container'); (root/'container').mkdir()
elif args[0]=='exec': print('0' if scenario=='unadmitted' else '1')
elif args[0]=='logs':
 since=args[args.index('--since')+1] if '--since' in args else '0'
 def emit(t,msg,**data):
  stamp=datetime.datetime.fromtimestamp(t,datetime.timezone.utc).isoformat().replace('+00:00','Z')
  print(stamp+' INFO '+msg+'\t'+json.dumps(data))
 if since.isdigit() and int(since)<1010: emit(1005,'FATAL proxy unavailable')
 emit(clock,'processing',module='aggsender')
 if scenario!='quiet':
  emit(clock-1,'inject GER transaction submitted with ID: '+os.environ['MOCK_MONITOR']+'. GER: '+os.environ['MOCK_GER'],module='aggoracle')
  emit(clock,'signed tx sent to the network: '+os.environ['MOCK_SIGNED'],monitoredTxId=os.environ['MOCK_MONITOR'])
elif args[0] not in ['stop','start']: sys.exit('unexpected docker call '+str(args))
'''


if __name__ == "__main__":
    unittest.main(verbosity=2)
