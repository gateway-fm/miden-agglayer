# PR #186 / issue #185 — live validation report

**Branch** `fix/185-unresolvable-claim-evm-revert`
**Base** `origin/main` @ `5934507` (branch was 0 behind / 2 ahead at start — **no rebase was needed**)
**Stack** node `v0.16.0-rc.5` (`461ac961951c`), stock client rc.5, `zkevm-bridge-service:v0.6.4-RC2-pendingbridges`
**Evidence root** `e2e-results/185-20260907T205206Z/`
**`gh` is UNAUTHENTICATED on this box** — the PR summary could not be posted; paste this file into #186 by hand.

> Note on the task premise: I could not find #176 anywhere in `origin/main`'s history
> (`git log --grep=176`, `git log --oneline | grep 176`). The branch is nonetheless on the
> current `origin/main` tip.

---

## 1. Static checks + baseline round-trip

| check | result |
|---|---|
| `cargo test --lib --bins` | **600 passed, 0 failed, 1 ignored** (599 before my fix added one) |
| `cargo clippy --all-targets -- -D warnings` | **clean** |
| `SKIP_STATIC=1 ./run-all.sh` | **rc=0** — `logs/03-run-all-rebuilt.log` |
| baseline L1→L2 | **PASS** — wallet `0 → 1000` units (`+1000`, expected `+1000`) |
| baseline L2→L1 | **PASS** — certificate settled on L1, `+5000000000000 wei` autoclaimed |

---

## 2. The #185 behaviour, end to end on a live stack

Driven by a new committed harness script, `scripts/e2e-185-unresolvable-claim-revert.sh`
(added in `7c149bd`). It bridges a real L1 deposit to a freshly generated EVM address whose
first byte is non-zero — so it can never satisfy the zero-padding fallback and has no store
mapping — lets the real claimtxman drive `claimAsset`, and asserts on chain state.

Run of record: `logs/04-185-live.log`, evidence in `185-live/`.
Deposit `deposit_cnt=2`, `global_index=18446744073709551618` (`0x10000000000000002`),
destination `0xd1336cc3ad5571621acbe631f08557e6422682b3`,
claim tx `0x99ea4a267efdd39f511711c67c1b5ed2a487914258111acecd749742c076ab22`.

### 2a — revert semantics: **PASS**

```
PASS: (a) receipt REVERTED: status=0x0 logs=0 block=112
PASS: (a) NO ClaimEvent for gi=18446744073709551618 — eth_getLogs=0, synthetic_logs=0
PASS: (a) nonce advanced past the reverted claim (EVM semantics)   [signer 0x6352…, tx nonce 0x1, account nonce 0x2]
PASS: (a) claim_unclaimable_reverted_total advanced: 0 -> 1
      (a) unclaimable_claims rows: 0 -> 1
```

The no-event check is made twice, from both ends a consumer can look: `eth_getLogs` over the
whole chain filtered on the ClaimEvent topic, matching the globalIndex in the first data word,
**and** the proxy's own `synthetic_logs` table. Both zero.

### 2b — `isClaimed(globalIndex)` is the retry-suppression signal: **PASS**

```
PASS: (b) isClaimed(2,0) = TRUE from the unclaimable record, with NO ClaimEvent behind it
```

`eth_call` to the bridge address, selector `0xcc461632`, returns ABI true.

### 2c — the claim submitter stops re-driving: **FAILED FIRST, then PASS after a product fix**

This is the one that did not hold, and the reason is worth reading in full.

**First run, `logs/02-185-live.log` + `185-live-attempt1-prefix-estimategas/`:**

```
(c) monitored_txs status=<none> terminal=0; reverted counter 0 -> 2 -> 10 (total attempts: 10)
FAIL: (c) monitored_txs is still non-terminal AND the revert counter is still advancing
```

Ten reverted receipts and ten consumed nonces in **19 seconds** (21:15:31 → 21:15:50), ended
only by claimtxman's own backstop:

```
claimtxman/monitortxs.go:116  marked as failed because reached the history size limit (10)
```

and **not one** `Checking if the deposit was already claimed` line in the bridge-service log.
The `checkIfClaimed` → `isClaimed` path that the #185 commit message names as its retry
suppression never executed once.

**Cause.** Only half the on-chain state learned the new fact. `checkIfClaimed` is reachable
only when `ReviewMonitoredTx` sees an `execution reverted` error, and `ReviewMonitoredTx`'s
only failure mode is `eth_estimateGas`. `service_estimate_gas` derived `claimed` from
`applied_state::claim_and_ger_applied` — the Miden-nullifier / ClaimEvent view, which is FALSE
for an unclaimable claim *precisely because #185 stopped emitting the event*. So `eth_call
isClaimed` answered "already claimed" while `eth_estimateGas` answered `0x0`, go ahead, and the
submitter believed the half that told it to retry.

**Fix — `40d6934`.** `eth_estimateGas(claimAsset)` now reads the same durable
`unclaimable_claims` record and reverts `AlreadyClaimed()`, ordered ahead of the
`GlobalExitRootInvalid()` arm ("retry when the GER lands" is exactly the wrong advice for a
claim that can never be applied). New metric `rpc_estimate_gas_unclaimable_total`. New unit
test asserts the revert and that the record wins over an absent GER.

**Re-run after the fix, `logs/04-185-live.log`:**

```
(c) monitored_txs status=confirmed terminal=1; reverted counter 0 -> 1 -> 1 (total attempts: 1)
PASS: (c) submitter STOPPED: monitored_txs status=confirmed (terminal) after 1 attempt(s)
```

and the documented mechanism now actually fires, in the bridge-service's own words:

```
claimtxman/monitortxs.go:375  Checking if the deposit was already claimed
claimtxman/monitortxs.go:393  The deposit was already claimed, so the monitored tx is marked as confirmed
```

proxy `/metrics`: `rpc_estimate_gas_unclaimable_total 1`, `claim_unclaimable_reverted_total 1`.

**One reverted receipt and one nonce, down from ten.**

### 2d — certificates keep settling (#184): **PASS**

```
(d) aggkit 'cutting certificate at block' lines since the claim: 0
PASS: (d) post-claim bridge-out completed
PASS: (d) certificate settlement ADVANCED across the unresolvable claim: height 1 -> 2
PASS: (d) aggkit logged ZERO 'cutting certificate' lines — #184 is gone
```

A full normal bridge-out (`scripts/e2e-l2-to-l1.sh`) landed *after* the unresolvable claim and
its certificate settled on L1 at a strictly greater height. The `#184` signature —
`found claim with unclaim after later unfinalized claim … cutting certificate at block N`
followed by `no bridges or claims found for range` forever — does not appear.

### 2e — restore fidelity (#103): **PASS**

`scripts/e2e-full-db-loss-recovery.sh` on the chain carrying the unresolvable claim
(`logs/05-full-db-loss.log`, rc=0):

```
baseline provenance: live (true fidelity comparison)
note-less (RD-860) claims recorded pre-drop: 1
eth_getLogs: before=9 logs after=9 logs
PASS: eth_getLogs identical across restore EXCEPT transaction_hash on 7 log(s) —
      every other field (block, log_index, address, topics, data, tx_index, removed) byte-identical
PASS: UpdateHashChain content identical … PASS: hash_chain_value identical (order-faithful replay)
PASS: BridgeEvent (count 2) + ClaimEvent (count 1) rows identical
```

This is the #103 proof in its exact shape. The drill *knows* an unresolvable claim is in
history (it snapshots the `unclaimable_claims` global indices before the drop, so it can
classify any lost log as an exempt note-less claim). **Nothing was lost, so the exempt path
never ran: 9 logs before, 9 after, zero exempt, zero unexplained.** Before #185 the same run
would have shown 10 → 9 with one "EXEMPT" drop, which is precisely the gap #103 filed.

The one `ClaimEvent` that survives is the legitimate L1→L2 claim from the baseline round-trip —
it has a real Miden note and replays. The unresolvable claim is simply absent on both sides.

---

## 3. Fixes made on this branch

| commit | kind | what |
|---|---|---|
| `fd0e147` | harness | Root-owned isolated wallet store silently minted a NEW wallet per call — `run-all`'s L1→L2 funded `0xb73e…`, the next L2→L1 minted a different wallet in the same store and died `Wallet has no balance`. Cause: the tool container runs as root, so `wallet-id` could not be written and both that and the `mkdir` failed silently. `_iso_own_store` chowns the store back; an unpersistable wallet id is now a hard stop. **Pre-existing, unrelated to #185.** |
| `40d6934` | **product** | `eth_estimateGas(claimAsset)` never read the `unclaimable_claims` record, so `checkIfClaimed` was unreachable and the submitter re-drove ten times. See 2c. |
| `7c149bd` | harness | `verify-event-completeness` still demanded the phantom ClaimEvent per unclaimable record (`unclaim 0/2 FAIL` — a green product failing a red test). Assertion **inverted**, not deleted: a ClaimEvent carrying an unclaimable record's exact `(block, globalIndex, tx-hash)` is now a PHANTOM and fails the verdict. Also adds `scripts/e2e-185-unresolvable-claim-revert.sh`. |

Verifier before/after on the live stack:

```
before (logs/07-…-BEFORE-fix.log):  CLAIM->ClaimEvent  2  2  2  0  0  0   0/2   -  0  FAIL   VERDICT: FAIL
after  (logs/08-…-AFTER-fix.log):   CLAIM->ClaimEvent  2  2  2  0  0  0  0p/0u  -  0  PASS   VERDICT: PASS
```

`scripts/test-verify-completeness-substitution.sh` (the verifier's own regression test) stays green.

---

## 4. Regression matrix

The regression question is narrow: **does any scenario that is green on `main`
go red with #185?** It is answered by one clean iteration of the committed
growing-chain battery, plus a same-shaped run on the base commit to say what
"green on main" actually means.

| | |
|---|---|
| branch run | `BATTERY_TAG=185 ITERATIONS=1 KEEP_CHAIN=1 ./scripts/e2e-battery.sh` at `eba7339` |
| results | `e2e-results/185-20260908T052005Z/` (`results.tsv`, `MATRIX.md`, `chain-growth.tsv`, `logs/`) |
| on-main baseline | `e2e-results/167-20260907T174108Z/` — battery started 17:41Z on the base commit; the #185 commits are authored 20:48Z, so that run contains none of them |
| gates before the run | `cargo test --lib --bins` 600+27+5 pass / 0 fail; `cargo clippy --all-targets -- -D warnings` clean; full `make lint` (fmt + taplo + typos + clippy) clean |
| ownership | this session was the **sole** owner of the `miden-agglayer` compose project for the whole run — the two-batteries-one-project contamination described in §5 does not apply here |

| # | target | on main | #185 branch | secs | regression? |
|---|---|---|---|---|---|
| 1 | `fixture-l2-to-l1-livebaseline` | PASS | PASS | 399 |  no |
| 2 | `full-db-loss-recovery-livebaseline` | PASS | PASS | 371 |  no |
| 3 | `test-e2e` | PASS | PASS | 3685 |  no |
| 4 | `e2e-l1-to-l2` | PASS | PASS | 122 |  no |
| 5 | `e2e-claim-provenance` | PASS | PASS | 756 |  no |
| 6 | `e2e-claim-watcher` | PASS | PASS | 142 |  no |
| 7 | `e2e-claim-watcher-synthesis` | PASS | PASS | 153 |  no |
| 8 | `e2e-l2-to-l1` | PASS | PASS | 181 |  no |
| 9 | `e2e-l2-to-l1-autoclaim` | PASS | PASS | 195 |  no |
| 10 | `e2e-restore` | PASS | PASS | 260 |  no |
| 11 | `e2e-cantina6-faucet-identity-restore` | PASS | PASS | 427 |  no |
| 12 | `e2e-cantina10` | PASS | PASS | 290 |  no |
| 13 | `e2e-cantina12-getlogs-returns-all` | PASS | PASS | 33 |  no |
| 14 | `e2e-cantina13` | PASS | PASS | 623 |  no |
| 15 | `e2e-ger-decomposition` | PASS | PASS | 31 |  no |
| 16 | `e2e-security` | PASS | PASS | 34 |  no |
| 17 | `e2e-fuzz` | PASS | PASS | 271 |  no |
| 18 | `e2e-reconciler-private-note` | FAIL | **FAIL** | 329 |  **no** — red on main too |
| 19 | `e2e-reconciler-cursor-persistence` | PASS | PASS | 46 |  no |
| 20 | `e2e-rd913-restart-burn-collision` | FAIL | **FAIL** | 164 |  **no** — red on main too |
| 21 | `e2e-rd940` | PASS | PASS | 145 |  no |
| 22 | `e2e-l2l2-up` | PASS | PASS | 75 |  no |
| 23 | `e2e-l2l2` | PASS | PASS | 516 |  no |
| 24 | `e2e-recovery-readiness` | PASS | PASS | 484 |  no |
| 25 | `fixture-l2-to-l1` | PASS | PASS | 197 |  no |
| 26 | `full-db-loss-recovery` | FAIL | PASS | 385 |  no |
| 27 | `loadtest-N30` | FAIL | PASS | 2235 |  no |
| 28 | `verify-event-completeness` | PASS | PASS | 22 |  no |
| 29 | `chaos-soak` | — | PASS | 4707 |  no |
| 30 | `full-db-loss-recovery-postchaos` | — | **FAIL** | 656 |  no baseline |

**27 PASS / 3 FAIL of 30**

`— ` = the on-main battery was stopped after `verify-event-completeness`, so
those two targets have no baseline to compare against.

**Zero regressions.** No target that is green on main is red here. Two targets
went the other way — red on main, green with #185 — and one of those is the
point of the PR.

### The two improvements

**`full-db-loss-recovery`: RED on main -> GREEN with #185.** Main failed the
drill with exactly the symptom #185 exists to remove:

    main:  FAIL: #69/#136 ClaimEvent (all fields except tx_hash) rows differ
                 (digest e4a18746f9d029961ff8e13fb91eb812 -> 1b052cb9d77535b07e4eca89cd473a16)
    #185:  PASS: BridgeEvent (count 26) + ClaimEvent (count 52) rows identical

A phantom ClaimEvent — emitted for a claim that never resolved to a note —
cannot be reconstructed from the chain after a full DB loss, so the pre- and
post-restore digests disagree. #185 stops emitting it, and the digests match.
Written up in `logs/FINDING-fulldbloss-red-to-green.md`. Caveat stated there:
the main-side battery was partly contaminated (section 5), so this is strong
corroboration of section 2e rather than a controlled A/B.

**`loadtest-N30`: RED on main -> GREEN with #185**, `sub 16/16 + 14/14, fail 0,
claimed 30/30`. Counted as "no regression" only, NOT as a second improvement:
main's failure was a direct casualty of the competing-run contamination in
section 5, so that red says nothing about main's real behaviour.

### The three reds, each diagnosed with evidence

| target | root cause | verdict | evidence |
|---|---|---|---|
| `e2e-reconciler-private-note` | writer-queue backlog (16 inflight, 2 pending-unlinked at 7s); the L1->L2 funding claim missed its 240s budget. Cursors level, nothing parked or stranded, ZERO `nonce too low`. | **red on main too** — identical failure, same phase, same message, 264s vs 265s in-phase | `logs/FINDING-reconciler-private-note.md` |
| `e2e-rd913-restart-burn-collision` | `second INSERT of a known serial returned 1 rows; ON CONFLICT semantics broken` | **red on main too** — same step, same assertion, different serial | `logs/FINDING-rd913-burn-collision.md` |
| `full-db-loss-recovery-postchaos` | one claim published during the chaos storm never had its note consumed, so its receipt stayed `pending` and quiesce correctly refused to fingerprint a moving pipeline (#156 orphan family) | **not #185** — three independent proofs below | `logs/FINDING-postchaos-quiesce-orphan.md`, `logs/quiesce-timeout-20260908T101942Z.txt` |

For every one of the three, the #185 path was proven NOT to have been entered.

### Why none of the three can be #185 — the shared proof

**`unclaimable_claims` holds 0 rows for the entire battery**, and no
`claim_unclaimable_reverted_total` samples were recorded. #185 writes that
record BEFORE it reverts, so any half-completed #185 path would leave a record
behind. There are none: across 30 targets, ~5900 blocks, 59 bridges and 124
claims, the changed branch was never reached. The battery therefore measures
exactly what it is supposed to measure — that the UNCHANGED behaviour is intact.

The post-chaos red is the only one that even looks like it could be #185
(a claimAsset that never terminated), so it was traced line by line:

    09:55:18  WARN address_mapper C5: resolved EVM address via zero-padding fallback
              (no store mapping; account existence on Miden NOT verified)
    09:56:12  claim published; receipt pending until the projector finalises it on consumption
    09:56:12  writer_worker: job committed, block 5444, elapsed 54.19s

1. The destination WAS resolved (via the C5 fallback), so the unresolvable
   branch #185 rewrote never fired — by design, and identically to main.
2. `unclaimable_claims` is empty, so the path left no trace because it was
   never taken.
3. The #185 branch is terminal in BOTH versions — `record_local_immediate_success`
   before, `commit_reverted_receipt_and_advance_nonce` after. Both write a
   receipt and advance the nonce; neither can leave a txn `pending`, which is
   the state that blocks quiesce.

The positive control: `full-db-loss-recovery` (row 26) — same drill, same chain,
same branch, before the storm — PASSED in 385s. The drill works with #185; an
orphaned claim from a 300s chaos storm is what defeats the post-chaos run. The
pending row was LEFT IN PLACE; deleting it would manufacture a green matrix.

### Chain growth (the growing-chain claim, recorded not assumed)

| when | mark | synthetic tip | UHC | Bridge | Claim |
|---|---|---|---|---|---|
| 05:27:11Z | live-baseline drill: before | 90 | 3 | 1 | 1 |
| 05:33:23Z | live-baseline drill: after | 213 | 5 | 1 | 2 |
| 08:06:18Z | before `full-db-loss-recovery` | 3263 | 76 | 26 | 52 |
| 08:12:43Z | after `full-db-loss-recovery` | 3392 | 78 | 26 | 53 |
| 10:08:47Z | before `full-db-loss-recovery-postchaos` | 5713 | 145 | 59 | 124 |
| 10:19:44Z | after `full-db-loss-recovery-postchaos` | 5931 | 145 | 59 | 124 |
| **10:19:44Z** | **iteration 1 END** | **5931** | **145** | **59** | **124** |

One genesis, one `node_data` volume, one anvil L1 for all 30 targets. **Chain
height at iteration end: 5931 blocks**, carrying 145 GER updates, 59 bridge-outs
and 124 claims. Total wall time 04:59:39 (05:20:05Z -> 10:19:44Z).

### The claim-touching targets, called out

These exercise the path #185 changed and were watched specifically:

| target | result | the assertion that matters |
|---|---|---|
| `e2e-l1-to-l2` | PASS 122s | the primary claim path, faster than main (126s) |
| `e2e-claim-watcher` / `-synthesis` | PASS 142s / 153s | claim watching + ClaimEvent synthesis |
| `e2e-claim-provenance` | PASS 756s | claim calldata/attribution |
| `e2e-cantina10` / `12` / `13` / `6` | PASS | the cantina claim + getlogs + restore tests |
| `e2e-recovery-readiness` | PASS 484s | `No foreign/spurious ClaimEvent across recovery (count stable at 51)`; full calldata recovered byte-for-byte; cert settled Height 72 -> 73 on L1 |
| `verify-event-completeness` | PASS 22s | `CLAIM->ClaimEvent 79 79 79 0 0 0 0p/0u - 0 PASS` |
| `e2e-restore` / `full-db-loss-recovery*` | PASS (3 of 4) | restore stays byte-identical (below) |
| `test-e2e` (claim-race scenario) | PASS | `race: SPONSOR won; user's tx accept-and-reverted (status-0x0, no ClaimEvent)` + `exactly ONE ClaimEvent for the raced gi` |

**`verify-event-completeness` (fixed in `7c149bd`) stays green**, and the
inverted assertion is demonstrably live rather than vacuous:

```
TYPE                    notes   logs  exact  late  missing  defer  unclaim forbid  extra  verdict
B2AGG->BridgeEvent         40     40     40     0        0      0        -      0      0  PASS
CLAIM->ClaimEvent          79     79     79     0        0      0    0p/0u      -      0  PASS
GER->UpdateHashChain      100    100    100     0        0      0        -      -      0  PASS
VERDICT: PASS
```

79 of 79 claims matched at their exact consumption block, **0 phantom / 0
unclaimed-missing**, 0 late, 0 extra — on a chain carrying 30 loadtest bridges.

**The restore drills stay byte-identical.** The live-baseline drill (the only
fully-live fidelity comparison in the battery, bought once immediately after
the genesis wipe) reports:

```
baseline provenance: live (true fidelity comparison)
PASS: eth_getLogs identical across restore EXCEPT transaction_hash on 5 log(s)
      — every other field (block, log_index, address, topics, data, tx_index, removed) byte-identical
PASS: UpdateHashChain content identical (count 4, digest 21daf3567c15)
PASS: injected-GER set identical (count 4, digest fd7e0b55666b)
PASS: hash_chain_value identical (order-faithful replay)
PASS: BridgeEvent (count 1) + ClaimEvent (count 1) rows identical
PASS: post-restore claim SETTLED ... no unclaimable_claims row
      (a real claim, not a note-less short-circuit)
```

and the larger mixed-baseline drill repeats it at scale (51 logs, 77 GERs,
26 BridgeEvents, 52 ClaimEvents, all identical). The drill itself distinguishes
an #185 record from a real claim, and correctly found none.

### Chaos

`chaos-soak` PASSED in 4707s (78 min) with no on-main baseline to compare to.
Under a 300s seeder + 300s garbo storm the mixed loadtest returned
`sub 15/15 + 15/15, fail 0, claimed 30/30`, the post-chaos operation reported
`MIXED LOADTEST PASS — all 4 directions landed + clash distinct` with proxy
store-locks = 0, and `post-storm service state before any harness repair: all
running`.

---

## 5. Environment note — a competing run was destroying an EARLIER run's evidence

**This applies to the 2026-09-07 runs only.** The section-4 battery above
(`185-20260908T052005Z`) was the sole owner of the `miden-agglayer` compose
project from start to finish, verified before launch and never contended.
The note is kept because it is why the on-main baseline
(`167-20260907T174108Z`) is corroborating evidence rather than a clean control.

A previous session's `e2e-battery.sh` (PID 14509, started 17:41Z, results in
`e2e-results/167-20260907T174108Z`) was still running against the **same** docker-compose
project. This session's instructed `SKIP_STATIC=1 ./run-all.sh` wipes genesis and the
`node_data` volume, so it destroyed that battery's chain at ~21:02Z — its `loadtest-N30`
failed as a direct result — and that battery's chaos-soak then drove concurrent L1 traffic
from the same funded key `scripts/e2e-l2-to-l1.sh` uses as its L1 destination, producing a
nonsensical `L1 balance change mismatch: got -4547008009104016 wei` (a *negative* delta = that
account paid gas for someone else's `bridgeAsset`).

Both runs were producing garbage. I stopped the stale battery and left its results directory
untouched. Recorded in `logs/00-stale-battery-conflict.txt`. Two batteries cannot share one
compose project; only one owner at a time.

---

## 6. Open items

- **`gh` is unauthenticated** — the PR #186 summary was not posted from this
  session. Paste this file, or run
  `gh pr comment 186 --body-file REPORT.md` once authenticated.
- **The first unresolvable claim per deposit still costs one reverted receipt and one nonce**,
  by design: attempt 1's `eth_estimateGas` runs *before* the `unclaimable_claims` record exists,
  so it cannot know. That is the floor, and it matches EVM (the first attempt is what reverts).
- **Operator rescue (tier 2) must delete the `unclaimable_claims` row** to re-open a global
  index. Since #185 that was already true of `eth_call isClaimed`; `40d6934` adds
  `eth_estimateGas` to the same contract. Not a new constraint, but now two call sites.

### Raised by the regression run (all pre-existing, none blocking #186)

- **`e2e-rd913-restart-burn-collision` fails on base.** `INSERT ... ON CONFLICT
  DO NOTHING` on the burn-serial table returns 1 row for a serial already seen.
  Reproduced identically on the pre-#185 battery. Worth its own issue; not fixed
  here because it would mix an unrelated product change into a claim-semantics PR.
  Evidence: `logs/FINDING-rd913-burn-collision.md`.
- **`e2e-reconciler-private-note` fails on base** under writer-queue backlog on a
  growing chain: the L1->L2 funding claim misses its 240s budget. Same failure on
  main. Evidence: `logs/FINDING-reconciler-private-note.md`.
- **#156 orphan recovery is still the growing-chain ceiling.** One claim published
  inside the chaos storm never had its note consumed; its receipt stays `pending`
  forever, which permanently blocks quiesce and therefore every later
  fingerprinting drill on that chain. Evidence:
  `logs/FINDING-postchaos-quiesce-orphan.md`.
- **Follow-up that would EXTEND #185's coverage (not a defect in it).** The C5
  zero-padding fallback in `address_mapper` resolves a destination without
  verifying the account exists on Miden. That is exactly why the orphaned claim
  above bypassed #185's branch and became a stuck `pending` instead of a clean
  accept-and-revert. Verifying existence at resolution time would route this
  class into #185's revert path and remove the orphan. Worth filing against #156
  or as a follow-up to #185.
- **The battery driver's `nonce_gate` never fired** in this run (no parked txns at
  any target boundary), so the #15 gap-adoption wedge that dominated the g1-g4
  iterations did not recur.
