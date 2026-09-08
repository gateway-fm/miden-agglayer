# How I ship a fix — my operator playbook

This is how I take an issue to a merged fix on miden-agglayer. I run the loop and
make the calls; the tools do the legwork. Reconstructed from the merged PRs and
how I actually drive the remote box, so a stand-in could run it the way I do.

## Who does what, from my seat

- **Me (operator).** I frame the issue, brief the driver, decide what's a real
  regression, triage the security alerts, and own the merge. Nothing merges on an
  agent's say-so — it merges on mine.
- **Claude Code (the driver).** Does the reading, the diagnosis, the edits, the
  commits — on a brief from me, reporting back with evidence I can check.
- **GPT (`codex`) — my second reviewer.** I run it on the diff before anything is
  pushed. It's the independent read that catches what the author (and I) am blind
  to.
- **The remote e2e agent (the box).** Proves the change on the real, growing
  chain. It reports PASS/FAIL with logs; I read the verdict, I don't take it on faith.
- **The PR reviewers — CodeQL, Copilot, and teammates.** The PR-level gate. I
  triage CodeQL myself, have the Copilot and teammate comments addressed round by
  round, and a teammate approves within a scope they state.

```
  issue ─▶ 0 frame ─▶ 1 diagnose ─▶ 2 fix ─▶ 3 local gates
                                                   │
  merge ◀─ 6 PR gate ◀─ 5 prove on box ◀─ 4 GPT review ◀─┘
   (my call)                  │
                     red? ────┘  back to 2, re-run that one target
```

---

## 0 — I frame the issue

- I read the issue in full and write its asks as a **numbered checklist**. That
  list is the contract: the PR will answer it row by row (that's the "Issue #N
  checklist" table you see in my PRs).
- I write down the **invariant the fix must not break** before any code moves —
  certificates keep settling, claim indices stay stable, `--restore` stays
  byte-identical, the sealing contract holds. That line becomes the reviewer's
  brief at step 4.
- **My rule: check the source, don't trust the log.** When the behaviour depends
  on aggkit, zkevm-bridge-service, or the Miden node, I make the driver read
  *their* source at the pinned version and cite `file:line`. A runtime log is a
  hint; the source is the answer.

## 1 — I get it diagnosed from code *and* live state

- I want the real code at `file:line`, not a plausible story. If there's a
  running chain, I want it confirmed there too: the store row, the decoded
  calldata, an `eth_call`, the container logs.
- "It should heal" is not an answer I accept. The DB row and the chain height
  are. If we can't yet tell a product bug from a harness/timing artifact, I send
  it back for evidence — restart the proxy, advance the chain, re-read the row —
  rather than let a guess through.

## 2 — I have the fix written on a branch

- One typed branch per issue: `fix/0.16-<issue>-<slug>`, `feat/016-…`,
  `refactor/…`, `chore/…`, `docs/…`, `test/…`.
- Conventional, issue-scoped commits, small and many — I read the history, so it
  has to be readable. Tests land in the same PR as the fix.
- **Two rules I enforce every time, because we paid for them:**
  - **Comments say WHY, not HOW.** No restating the code. A comment earns its
    place by naming a non-obvious invariant or an upstream quirk. I have the
    driver do a dedicated "comment pass" before the PR.
  - **Harness fixes go into `scripts/`, committed — never `/tmp`.** If a drill had
    to be fixed to measure the thing, that fix is part of the deliverable. We were
    re-fixing the same harness every run until I made this stick.

## 3 — I don't review anything until the local gates are green

```sh
cargo test --lib --bins        # I want the count in the PR: "622 passed, 0 failed"
cargo clippy --all-targets -- -D warnings
cargo fmt --all                # cargo fmt --check is a CI gate
typos
```

- `upstream-api-gate` must be clean: no `[patch.crates-io]`, `vendor/` empty. A
  vendor or a patch is a tracked exception I sign off on, never a silent one.
- Branch rebased up to `main`, and the PR says so.

## 4 — I run the GPT review, and loop it to consensus

This is my independent second read. I run `codex` (its strongest profile) as an
adversary, not a rubber stamp — and I don't stop at one pass. **I fix and
re-review until codex is happy**: "keep re-reviewing until it reaches consensus
that the code is good to go." Reaching that consensus is what earns the full
e2e + soak re-run in step 5.

```sh
./scripts/gpt-review.sh                 # HEAD vs origin/main
./scripts/gpt-review.sh --commit <sha>  # one commit
./scripts/gpt-review.sh --uncommitted   # working tree, pre-commit
```

The script feeds it my step-0 invariant and asks specifically for correctness
holes, over-claims, and missed edge cases. **I read every finding and decide**:
fix it, or record why it's a non-issue. A finding I wave off without a reason is
a finding I ignored. This pass never shows up in the PR — it's what makes the PR
worth approving. Only once codex is satisfied do I hand it to the box.

## 5 — I have it proven on the box, on a live growing chain

The box (`mandrigin@159.223.127.5`) runs the real stack under the resident e2e
agent. This is the gate that keeps a PR in **draft until it's green** — I don't
un-draft on "should work."

```sh
make e2e-battery ITERATIONS=n KEEP_CHAIN=1
```

What I insist on here:

- **One chain, always growing.** Same genesis, node volume, L1 anvil, bridge and
  faucets across every iteration. Only the proxy Postgres and the client store
  ever get wiped — that's the whole point, I'm testing ever-larger recovery
  histories, so every assertion is a **delta** from a baseline, not a
  genesis-fresh count.
- **PASS is decided by quiesce, then a fingerprint diff** — projector at tip,
  writer drained, no non-terminal receipts. I don't accept a fingerprint taken
  off a moving pipeline.
- **A target green on `main` that goes red here is a regression.** I have it
  diagnosed product-vs-harness, fixed on the branch (src/ if it's product; the
  script only if the assertion encoded old behaviour, and only once I'm sure it
  isn't masking a real break), then re-run — just that one target.
- **Box discipline, because it's shared:** all non-e2e docker stacks stopped
  first, few background shells (it OOM-kills around ~16), one foreground `make`
  at a time. Every run lands under `e2e-results/<issue>-<UTC>/` with a `MATRIX.md`;
  I get the verdict back as a `REPORT.md` and a PR comment.

## The quality gate — which tests, how many times

Nothing reaches the PR gate until it clears all of this. This is the bar I say
out loud every time ("all the quality gates: linters, 3× e2e and N=20").

- **Linters / CI green.** `make lint` — clippy (with the relevant features,
  e.g. `postgres`), `typos`, `taplo`/`fmt` — plus every GH CI check.
- **e2e, three times.** The full suite has to pass, then pass **twice more**.
  Once is not enough — **flaky tests are dangerous**, and a suite that passes
  once but not on rerun is not ready. I prove non-flakiness before I trust it.
- **Loadtest, by tier.** After the 3× e2e is clean: **N=20** bridges back and
  forth as the baseline gate; **N=50 / N=250** after a bug fix or for a bigger
  soak; the growing-chain battery's **N=30 + chaos-soak + full-DB-loss +
  post-chaos** for anything that touches recovery.
- **A regression test per fix.** Every Cantina/issue finding gets a test that
  reproduces it — an e2e where it belongs, a unit test where that's enough. A fix
  without a test doesn't land. I also add the specific invariant checks (e.g.
  `eth_blockNumber` / `getBlockByNumber("latest")` consistency, `getLogs`
  completeness).
- **No swallowed errors.** The e2e pipeline surfaces every failure; I want each
  one classified on purpose — what's a stopper, what's a warning.
- **Realistic, isolated data.** The chain carries realistic seeded activity
  (different wallets, public and private), and tests are isolated data-wise so one
  can't mask another.
- **Upgrade/migration test when params or schema change.** Seed the previous
  release with data, migrate to the new build **preserving the DB**, and confirm
  `getLogs` and aggkit are unbroken — with an upgrade guide for any changed params.
- **I want every run reported as it lands** — pass or fail, each e2e finish,
  especially for a fast-tracked or flakiness-sensitive PR.

**How I pace it: sequential, stability over speed.** I don't parallelize the
gate — "I want stability… it's fine to be sequential, no need to optimize for
speed." One branch, one gate at a time.

## 6 — I take it through the PR gate and merge

I push **early** and open the PR to get eyes on it — reviewing is a group effort,
and I'd rather get feedback while the change is fresh than sit on it.

- **The PR body follows my template:** `Closes #N` → what-and-why → the issue
  checklist table (one ✅ per ask) → how it works → behavior changes (migrations,
  fail-closed shifts) → **Verification** (test counts, clippy/fmt/typos clean,
  "merged up to main", and an explicit **"not yet run"** line for any open gate).
- **Draft until green, and I make CI enforce it.** While a blocker is open the PR
  stays draft — and where it matters I wire the GH CI to *fail* until it's fixed,
  so nothing can slip in on a green checkmark it didn't earn. I un-draft only when
  nothing's left "not yet run".
- **One concern per PR.** Each fix is its own PR — unless I explicitly
  *consolidate* a tightly-coupled set (private-note + cursor-persistence + the
  speedup went into one), and then I clean up the branches that folded in.
- **I drive the review rounds.** The PR collects CodeQL, Copilot, and teammate
  comments. I have each round addressed — but I check every comment for
  **validity** first, I don't apply feedback blindly — then push again for a quick
  re-review ("new review", "nit", "two final things") until it's clean. A push
  that gets a fast re-review still has to pass the e2e + N gate before it counts.
- **CodeQL I triage myself.** It has to go fail → pass. I examine each alert and
  dismiss false positives with the rationale recorded **on the alert** — e.g. the
  `TxLegacy.nonce` hits are an EVM sequence number, not a crypto nonce, and the
  sites are test-only — and I check the flagged findings are exercised in tests. I
  don't blanket-silence.
- **Human approval is scoped, and I respect the scope.** The reviewer states what
  they reviewed (say, the CodeQL triage) and what they didn't (functional
  correctness rests on my e2e proof, not their approval).

**The merge, and the branch chain.** The merge is my call. My branches are
usually *stacked*, so I land them in order: merge one, rebase the next onto the
new `main`, make lint clean, push, and re-run the e2e gate (at least 1× on the
rebased branch) before merging it too. That's why I'm strict about rebasing —
each branch has to be gated against the `main` it will actually land on, not the
one it was written against.

---

## What I hold the line on, regardless

- **Faithful reporting.** A failing target is reported failing, with the output.
  A red I can't yet explain stays red and named — I don't let a real wedge get
  relabelled a "timing artifact" to green a matrix.
- **Evidence over assertion**, at every step — from "check the source" at 0 to
  "quiesce before you fingerprint" at 5.
- **Two independent reviewers by design:** GPT on the diff (correctness), CodeQL +
  a human at the PR (security + scoped sign-off). Neither replaces the live e2e
  proof, and none of them replaces my merge decision.
- **Durable facts get written down** — what's non-obvious about the code, a
  decision and its why, a live-endpoint pointer — so I'm not re-deriving them next
  time.
