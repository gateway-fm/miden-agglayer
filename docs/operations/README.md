# miden-agglayer — operations

Operational reference for running miden-agglayer in a production-like
deployment (bali is the current reference cluster).

Audience: SRE on call for the bali Miden testnet, or anyone bringing up a
similarly-shaped deployment elsewhere.

## What's in this directory

| Doc | Use it when… |
|---|---|
| [`monitoring.md`](./monitoring.md) | You want to know what to scrape, what to alert on, and what "healthy" vs "stuck" looks like end to end. |
| [`runbook.md`](./runbook.md) | A specific failure mode is firing (IAIC, AccountDataNotFound, GER backlog, stuck claim, ClaimSettler dry, indexer drift) and you need step-by-step recovery. |
| [`diagnostics.md`](./diagnostics.md) | You don't yet know what's wrong — you need the read-only inspection playbook (Loki queries, SQL snapshots, account introspection, single-deposit tracing). |

## What's already documented elsewhere

These existing docs cover specific incidents and design notes — link out
to them rather than re-stating their contents:

- [`../POSTMORTEM_2026-05-11_IAIC_TO_ADNF.md`](../POSTMORTEM_2026-05-11_IAIC_TO_ADNF.md) — the IAIC →
  AccountDataNotFound chain that ran the bali bridge for ~20 days. Anyone
  triaging "mempool conflict" or "account data wasn't found" symptoms
  should read this first.
- [`../REDEPLOY_RUNBOOK_BALI.md`](../REDEPLOY_RUNBOOK_BALI.md) — the v0.4.1 deploy + recovery
  runbook for bali specifically. The redeploy procedure documented there
  is the cure for the postmortem's failure mode on the existing bali
  cluster.
- [`../ger-decomposition.md`](../ger-decomposition.md) — design notes on the GER decomposition
  problem and the `UseUpdateExitRoot` aggoracle mode that is the
  permanent fix (RD-862).
- [`../ger-note-screening-bypass.md`](../ger-note-screening-bypass.md) — design notes on the split
  `execute → prove → submit` flow that GER injection uses to bypass the
  miden-client NoteScreener.
- [`../../README.md`](../../README.md) — service-level overview, CLI flags,
  ClaimSettler env vars, recovery flags (`--reset-miden-store`,
  `--unlock-miden-accounts`, `--restore`, `--init`).
- The [`miden-bali-debug` skill](../../.claude/skills/miden-bali-debug/SKILL.md) (if checked
  out) — read-only diagnostic agent that automates Phases 0-7 of
  `diagnostics.md`.

## Conventions

- All command examples assume the bali cluster
  (`dev-gateway-eks` / `outpost-testnet-miden-testnet` namespace,
  pod `miden-agglayer-0`, image `gatewayfm/miden-agglayer:0.4.1` or
  later). Adapt cluster context, namespace, and pod name for other
  deployments.
- Placeholders are written as `<placeholder>` and need operator input
  before the command will run. Anything labelled `<TODO: ...>` is
  unverified — confirm with the on-call before relying on it.
- Destructive commands (`--reset-miden-store`, `--init`, SQL UPDATE) are
  always called out with their blast radius and the rollback path.
- The diagnostic skill is read-only by contract — it never executes
  recovery actions, even when one is obvious.
