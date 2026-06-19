# `/pending-bridges` rollup-disambiguation bug — why it matters, why only we hit it

> Context: this is the design / decision record behind the in-repo `bridge-autoclaim`.
> The standalone L2→L1 auto-claimer never claimed anything on Bali — every tick logged
> *"No bridges to claim were found"* — even with ready, funded, unclaimed exits waiting.
> The defect is in the upstream bridge service; our claimer routes around it (see below),
> which is why it discovers exits from the proxy's own synthetic BridgeEvent logs instead.

## The defect, in one line

The bridge service's "has this exit been claimed yet?" gate (`GetPendingDepositsToClaim`,
used by `/pending-bridges` and the in-process `ClaimTxManager`) matched a recorded claim on
**`(destination network, leaf_index)`** only — it dropped the **source rollup** part of the
key (`rollup_index` / `mainnet_flag`):

```sql
-- buggy
AND NOT EXISTS (SELECT 1 FROM sync.claim c
                WHERE c.network_id = $1 AND c.index = d.deposit_cnt)
```

A claim is globally identified by the triple **(mainnet_flag, rollup_index, leaf_index)** —
the *global index*. Whether dropping the source part matters depends entirely on **direction**.

## The asymmetry (this is the crux)

- **L1→L2 (deposit) — the direction everyone auto-claims — is collision-proof by construction.**
  The source is **mainnet**: there is exactly one of those, and L1's deposit tree is a single
  global tree, so leaf indices are globally unique. The destination is the rollup, already
  encoded in `network_id`. The source-rollup key is therefore *redundant* here — two deposits
  can never collide. Safe even on a shared rollup manager.

- **L2→L1 (withdrawal) is where it bites.** The source is now **one of many rollups**, each with
  its **own** exit tree whose leaf indices restart at 0 — so *every* rollup has an exit `#23`.
  The destination is **L1**, i.e. `network_id = 0`, shared by every rollup. The source rollup is
  now the *only* thing distinguishing our `#23` from rollup 44's `#23` — and that is exactly the
  part the query threw away. The bridge service indexes the shared L1 bridge's `ClaimEvent`s for
  **every** rollup, all recorded under `network_id = 0`, so once any co-tenant claimed their `#23`,
  that single `(network=0, index=23)` row masked ours **permanently**.

**Field proof (Bali):** our rollup **76** exit `#23` was `ready_for_claim = true` and unclaimed,
but rollups **44 / 49 / 52 / 57** had already claimed *their* `#23` on L1. The unqualified query
returned 0 rows; the same query with origin disambiguation returned our deposits 13, 14, 23.
`/bridges` showed them claimable the whole time — because it uses `GetClaim`, which **was always
rollup-qualified** (`... AND NOT mainnet_flag AND rollup_index + 1 = network_id ...`).

## Why standard agglayer networks don't hit it

The bug needs **all three** conditions simultaneously:

1. you are auto-claiming the **L2→L1** direction (destination `network_id = 0`, shared by all);
2. against a **shared L1 bridge / rollup manager** with **multiple** rollups, so `sync.claim`
   holds other rollups' L1 claims; and
3. **overlapping leaf indices** — always true, since they are per-rollup-local.

Standard Polygon CDK deployments miss it because:

- the auto-claimer / `ClaimTxManager` is overwhelmingly used for **L1→L2** (collision-proof, above);
  L2→L1 withdrawals are typically **user-initiated via the bridge UI**, which calls
  `/bridges/<addr>` + `/merkle-proof` — the correct `GetClaim` path; and
- a **single-rollup** deployment has no co-tenant claims in its table to collide with, even L2→L1.

So it is **not** "they sync the whole tree" — correct and buggy services sync the same events.
It is that **only the shared-manager L2→L1 case ever has more than one possible source rollup in
scope at once**, which is precisely the agglayer "many aggchains, one L1 bridge" model that Bali
is, and that we are now driving auto-claims through. (Our 73→76 rollup re-registration guaranteed
an overlap.) The bug is present identically on upstream `v0.6.4-RC2`, `main`, and `develop` — an
image bump alone does not fix it.

## The fix

Restore the source-rollup part of the key in all three pending-deposit checks
(`GetPendingDepositsToClaim` count + rows, and `GetDepositsFromOtherL2ToClaim`), mirroring
`GetClaim`:

```sql
AND NOT EXISTS (SELECT 1 FROM sync.claim c
  WHERE c.network_id = $1 AND c.index = d.deposit_cnt
    AND ( (d.network_id = 0  AND c.mainnet_flag)
       OR (d.network_id <> 0 AND NOT c.mainnet_flag AND c.rollup_index + 1 = d.network_id) ))
```

No schema/migration change; single-rollup deployments are unaffected. Plus a new
`[AutoClaim].SourceNetworkID` on the standalone autoclaimer so we only *sponsor* (pay gas for)
our own rollup's exits, not co-tenants'.

## Can we fix it in our proxy (miden-agglayer) instead?

**Not the bug itself.** The defect is the bridge service's SQL — the proxy can't change it. Nor can
the proxy dedupe its way out: Miden's per-rollup leaf indices are *meant* to collide with other
rollups' (that's the tree model, not a mistake), so there's no proxy-side data change that makes
them globally unique.

**But the proxy can sidestep the bad endpoint entirely.** A native L2→L1 claimer in the proxy
(behind a flag) can drive the L1 `claimAsset` off **`/bridges/<addr>` + `/merkle-proof`** instead of
`/pending-bridges`. Those use `GetClaim`, which was always rollup-qualified and correct, so they
never had this bug. That removes the dependency on a patched bridge-service image completely; the
trade-off is that we own the claim-submission loop (signer, gas, nonce, retries) rather than reusing
the upstream autoclaimer.

- **PR #1** is the right *upstream* fix (helps every multi-rollup AggLayer deployment).
- The **proxy-side claimer** is the right move if we'd rather not carry a forked bridge image.
