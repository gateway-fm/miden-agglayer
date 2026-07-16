# Security-finding regression coverage

This is a current-main index of regression coverage for the Cantina and
subsequent security-review findings. It intentionally avoids test counts, line
numbers, commit hashes and branch status: all four become stale without changing
the behaviour being protected.

The executable source of truth is the named tests in `src/` and the scripts in
`scripts/`. Search for a finding prefix (`cantina`, `finding_`, `ma`, `rd`,
`r`, `c`, `h` or `x`) to locate the exact regression.

## Coverage map

| Risk area | Current implementation and unit coverage | Integration coverage |
|---|---|---|
| Faucet identity and concurrent creation | `claim`, `faucet_ops`, `metadata_recovery`, and both store implementations cover origin `(address, network)` identity, single-flight creation, origin-collision convergence, decimal bounds and metadata recovery | `e2e-cantina10-concurrent-faucet.sh`, `e2e-cantina6-faucet-identity-restore.sh`, `e2e-dynamic-erc20.sh` |
| Claim validation and submission | `claim` and `service_send_raw_txn` cover amount bounds, mainnet proof canonicalisation, GER availability, signer policy, chain ID, nonce ordering and idempotent rebroadcast | `e2e-l1-to-l2.sh`, `e2e-manual-user-claim.sh`, `e2e-security.sh`, and the `e2e-rd940-*.sh` scripts |
| Bridge-out classification | `bridge_out`, `restore`, `unknown_wrapper_detector` and `synthetic_projector` cover bridge-consumed notes, user reclaims, unknown consumers, malformed/erased notes and quarantine | `e2e-l2-to-l1.sh`, `e2e-l2-to-l1-autoclaim.sh`, `e2e-claim-provenance.sh`, `e2e-cantina13-metadata-recovery.sh` |
| Synthetic event correctness | `synthetic_projector`, `log_synthesis`, and store tests cover deterministic projection, idempotency, exact Miden-block provenance, atomic log materialisation and address-aware filtering | `e2e-bridge-loadtest.sh`, `e2e-claim-watcher-synthesis.sh`, `e2e-b2agg-atomic-commit.sh`, `e2e-ger-atomic-commit.sh` |
| Restore and account recovery | `restore`, `account_recovery`, `recovery`, and `miden_client` cover account re-import, listener suspension, claim/GER reconstruction, provenance and fail-closed paths | `e2e-restore.sh`, `e2e-reset-restore-recovery.sh`, `e2e-account-reimport.sh`, `e2e-account-self-heal.sh` |
| Anomaly monitors | The burn-serial, twin-note, expected-mint, forged-mint, mint-target and faucet-ownership modules test their predicates; the stateful burn/twin/expected trackers also cover restart-safe persistence | `e2e-rd913-restart-burn-collision.sh`; broader wiring is exercised by bridge and chaos suites |
| RPC hardening | Service tests cover admin authentication, rate limits, request limits, CORS parsing, log-query bounds, error redaction and pending-receipt semantics | `e2e-security.sh`, `e2e-cantina12-getlogs-returns-all.sh`, `e2e-rpc-tip-consistency.sh`, `e2e-rd940-pending-receipt.sh` |
| Restart, load and fault recovery | Writer, reconciler and store tests cover durable state, cursor persistence, queue pressure and replay safety | `e2e-rd940-restart-inflight.sh`, `e2e-rd940-queue-backpressure.sh`, `e2e-reconciler-cursor-persistence.sh`, `e2e-chaos-soak.sh` |
| Cross-chain flows | Asset identity, GER routing and event synthesis are covered on each native leg | `e2e-l2l2-forward.sh`, `e2e-l2l2-back.sh`, `e2e-l2l2-clash.sh`, `e2e-miden-origin.sh`, `e2e-loadtest-mixed.sh` |

## Known coverage boundaries

- PostgreSQL-specific regressions run only with the `postgres` feature and a
  live `DATABASE_URL`; the default CI job does not provide one.
- Several anomaly detectors have direct predicate and persistence tests but do
  not deliberately manufacture the corresponding adversarial on-chain event in
  E2E. The external completeness and immutability monitors provide independent
  detection during load and chaos runs.
- The full E2E and L2-to-L2 suites are release-certification workflows, not part
  of the default pull-request check.

These are execution boundaries, not claims that an open branch contains a fix.
If behaviour or a script is added or removed, update this index in the same
change.

## How to audit the mapping

```bash
# Finding-labelled Rust regressions
rg -n 'cantina|finding_|ma[0-9]+|rd[0-9]+|r[0-9]+|c[0-9]+|h[0-9]+|x[0-9]+' src

# Integration and release-certification entry points
rg --files scripts | rg 'e2e|release-acceptance|verify-event-completeness'
```

For how these checks are selected in CI, see
`docs/development/code-health-tooling.md`.
