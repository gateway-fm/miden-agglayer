# #146 future-nonce queue (stacked on rc.1) — review status

Branch: `feat/0.16-146-future-nonce-stacked` (upstream draft PR #155 replayed onto
`chore/node-0.16.0-rc1`, then fixed). **NOT ready to merge.**

## Why this file exists

Five rounds of adversarial review (gpt-5.6-sol, ultra) did not converge:

| Round | Blockers | Where they came from |
|-------|----------|----------------------|
| 1 | 10 | PR #155 as inherited |
| 2 | 9 → 4 open | 5 closed; 2 of my fixes incomplete |
| 3 | 4 | mostly my round-2 fixes |
| 4 | 5 | **all five from my round-3 fixes** |
| 5 | 4 | **all four from my round-4 fixes** |

Rounds 4 and 5 each introduced as many defects as they closed. That is the signal
to stop, not to keep going: the change is being driven by review pressure rather
than by a settled design, and each new guard interacts with the previous one.

## What IS solid and should be kept

* Parking itself works, live. A future-nonce `claimAsset` (nonce 1, expected 0) is
  accepted and lands in `queued_txns` on a real proxy; `e2e-future-nonce-mempool.sh`
  passes 8/8 (park, null receipt, geth pending shape, no nonce jump, idempotent
  rebroadcast, conflict refused, gap-fill, in-order promotion).
* TTL never deletes an acknowledged tx (`count_stale_queued_txns` is observability
  only). Memory is bounded by the per-signer/global caps, which reject at
  SUBMISSION where the caller still holds the tx.
* First-parked-hash wins, including once the nonce becomes executable.
* A stale row below the frontier no longer masks its successors.
* Promotion confirms a durable transaction row before deleting the queue copy.
* Startup replay does not block the HTTP bind; a periodic drain sweep exists.
* e2e harness fixes: chain id read from `eth_chainId` (was hardcoded 1; l2l2 is 2,
  so every tx was rejected before parking was reached), `send_raw` no longer aborts
  under `set -e`, and the vehicle is `claimAsset` because the strict-H6 preflight
  (scoped to `DecodedWriteCall::Ger`) refuses uncorroborated roots BEFORE the park
  decision.

## What is NOT resolved (the open blockers)

1. **The durable-lower-nonce refusal does not self-heal.** With pending `A@5`,
   `B@6` is refused. The wording was changed to `invalid nonce` so claimtxman's
   `isNonceError` matches — but its `ReviewMonitoredTx` only ever RAISES the stored
   nonce, so a fresh nonce of 5 against a stored 6 is ignored and `B` retries
   indefinitely. Parking `B` instead is NOT an option as tried: that branch returned
   before the nonce reservation and let a different tx park onto a bound nonce.
   Needs a design that queues the successor WITHOUT bypassing the reservation.

2. **The #90 post-rebuild bootstrap has no sound bound.** Five variants were tried
   (process-local timer; retire-on-empty-queue; retire-on-any-signer-resumed;
   durable window; per-signer grandfathering) and each either retired before a
   continuing wallet returned, or became a permanent amnesty that would seed a
   genuinely NEW wallet at nonce 42 and skip 0..41. The current state is the durable
   window only, whose known residual is that a tx parked in the last blocks before
   the window closes is not seeded afterwards. A correct fix needs a durable
   per-row "parked while recovery was open" marker, so a continuing wallet can be
   told from a new one — not a predicate over `nonce_get() == 0`, which an absent
   row also satisfies.

3. **Bootstrap cancellation vs PostgreSQL.** `main.rs` wraps the whole startup
   resume in a 30s timeout, and dropping an in-flight `tokio-postgres` execute does
   NOT cancel the statement. A cancelled bootstrap can still commit a HIGHER
   baseline after the per-signer lock is released, stranding a lower nonce. Needs a
   statement/lock timeout at the driver or a bootstrap that is safe to abandon.

4. **Sweep fairness vs bootstrap safety are in tension.** Bootstrap must not be
   cancellable (3) but must not block the single serial sweep either. Both cannot
   hold with one un-timed mutation on the sweep path; it needs either a per-signer
   task or a DB-level timeout.

## Recommendation

Land `chore/node-0.16.0-rc1` (#168) on its own — it is independently proven
(codex GOOD TO GO, 4/4 e2e, 6/6 byte-identical recovery drills, 5/6 chaos, ~17.5h
compounding soak, zero integrity violations). Keep this #146 stack open as a draft
with the four items above as its review agenda. Do not merge it on a green unit
suite: every round here had a green suite and still had blockers.
