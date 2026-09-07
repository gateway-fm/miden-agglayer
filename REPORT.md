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

*(pending — full growing-chain battery, `ITERATIONS=2`, running; see section below)*

---

## 5. Environment note — a competing run was destroying this one's evidence

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

- **`gh` is unauthenticated** — the PR #186 summary was not posted. Paste this file.
- **The first unresolvable claim per deposit still costs one reverted receipt and one nonce**,
  by design: attempt 1's `eth_estimateGas` runs *before* the `unclaimable_claims` record exists,
  so it cannot know. That is the floor, and it matches EVM (the first attempt is what reverts).
- **Operator rescue (tier 2) must delete the `unclaimable_claims` row** to re-open a global
  index. Since #185 that was already true of `eth_call isClaimed`; `40d6934` adds
  `eth_estimateGas` to the same contract. Not a new constraint, but now two call sites.
