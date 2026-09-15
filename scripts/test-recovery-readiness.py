"""Regression cases for the readiness gate's boot-evidence verdict."""

import importlib.util
import unittest
from pathlib import Path

spec = importlib.util.spec_from_file_location(
    "readiness", Path(__file__).with_name("lib-recovery-readiness.py")
)
readiness = importlib.util.module_from_spec(spec)
spec.loader.exec_module(readiness)


class BootEvidenceTests(unittest.TestCase):
    seed = "2026-09-15T10:07:02.325930Z WARN recovery readiness gated: claims_awaiting_calldata: 1"
    repair = "2026-09-15T10:07:02.637928Z INFO synthesized claim: persisted authoritative full claimAsset calldata, tx_hash: 0x1234, block_number: 114"
    serving = "2026-09-15T10:07:02.747202Z INFO Service started, address: http://0.0.0.0:8546/"

    def evidence(self, *lines, withheld=False):
        return readiness.boot_evidence("\n".join(lines), "0x1234", withheld)

    def test_repair_before_bind(self):
        result = self.evidence(self.seed, self.repair, self.serving)
        self.assertTrue(result["accepted"])
        self.assertTrue(result["repaired_before_http"])

    def test_ansi_fields(self):
        seed = self.seed.replace("claims_awaiting_calldata:", "\x1b[3mclaims_awaiting_calldata\x1b[0m\x1b[2m:\x1b[0m")
        self.assertEqual(self.evidence(seed, self.repair, self.serving)["seeded_backlog"], 1)

    def test_wrong_claim_is_not_proof(self):
        self.assertFalse(self.evidence(self.seed, self.repair.replace("0x1234", "0xabcd"), self.serving)["accepted"])

    def test_repair_after_bind_needs_observed_withhold(self):
        serving = self.serving.replace("02.747202", "02.500000")
        self.assertFalse(self.evidence(self.seed, serving, self.repair)["accepted"])
        self.assertTrue(self.evidence(self.seed, serving, self.repair, withheld=True)["accepted"])

    def test_missing_evidence_fails_closed(self):
        for lines in [(self.seed, self.serving), (self.seed, self.repair), (self.repair, self.serving)]:
            with self.subTest(lines=lines):
                self.assertFalse(self.evidence(*lines)["accepted"])

    def test_no_seed_cannot_pass_on_503_alone(self):
        self.assertFalse(self.evidence(self.serving, withheld=True)["accepted"])
        self.assertFalse(self.evidence(self.seed.replace(": 1", ": 0"), self.repair, self.serving, withheld=True)["accepted"])


if __name__ == "__main__":
    unittest.main()
