# L2→L2 e2e (Miden ↔ OP-Stack) — scaffold notes & design (task #25)

**Status: DESIGN + skeleton only** (branch `feat/l2-to-l2-e2e`). This is a large multi-day task; below is the map so it can be picked up cleanly. Absorbs task #15 (same-address/different-origin faucet isolation).

## Goal
Exercise the true cross-L2 bridge path through agglayer, which today's e2e (single Miden L2 ↔ L1) never covers: deploy an ERC-20 on a **second** L2 (OP-Stack), bridge it **OP-Stack → Miden** (foreign-origin → Miden provisions a wrapped-asset faucet), then bridge it **Miden → OP-Stack** back, asserting exact-block completeness and faucet isolation.

## Current single-rollup topology (`docker-compose.e2e.yml`)
One rollup = **Miden L2 (network_id=1, chain_id=2)**, made of:
- `anvil` — L1 (chain-id 271828), hosts the agglayer RollupManager + GER + bridge contracts.
- `agglayer` (`agglayer:0.4.4`) + `fixtures/agglayer-config.toml` — the settlement layer; **this is where rollups are registered** (multi-rollup config lives here).
- `aggkit:0.8.3-rc1` (`--components=aggoracle,aggsender`) — aggoracle injects L1→L2 GER, aggsender submits certs. One instance per rollup.
- Miden node microservices: `node-bootstrap-{validator,sequencer,ntx}`, `validator`, `tx-prover`, `miden-node`, `ntx-builder`.
- `miden-agglayer` (the proxy, :8546), `bridge-service`, `bridge-autoclaim`.
- Postgres x2 (proxy store + agglayer store).

Key IDs: `fixtures/.env` → `ROLLUP_ADDRESS`, `NETWORK_ID=1`, `CHAIN_ID=2`. Fixtures extracted via `make e2e-setup` → `scripts/setup-fixtures.sh` (from Kurtosis).

## What a second OP-Stack L2 requires
1. **The OP-Stack chain itself** — the biggest lift. Options, cheapest→most-faithful:
   - (a) A plain second `anvil`/reth EVM devnet acting as "L2 #2" — simplest, but not a real OP-Stack (no L2→L1 proof semantics). Good enough to exercise the *agglayer bridge* path if agglayer treats it as a registered rollup.
   - (b) A real OP-Stack minimal devnet (`op-geth` + `op-node` + `op-batcher`/`op-proposer`) — faithful but heavy (several more services + L1 deposit contracts).
   Recommend starting with (a) to unblock the *agglayer cross-rollup* logic, then upgrade to (b) if OP-specific behavior matters.
2. **Register it as rollup #2 in the agglayer** — add it to `fixtures/agglayer-config.toml` (a `[rollup]`/network entry with its `network_id=2`/`3`, its L1 RollupID from RollupManager, its bridge contract address). Deploy the agglayer **bridge contracts** on the OP-Stack L2 (mirror how they're deployed for Miden — see setup-fixtures.sh / the L1 RollupManager registration).
3. **Its own `aggkit` instance** (aggoracle+aggsender) pointed at the OP-Stack L2's RPC + its bridge/GER contracts, with its own keystore.
4. **An ERC-20** deployed on the OP-Stack L2 (distinct symbol, e.g. `OPT0`).
5. **GER propagation**: the shared L1 GER must carry both rollups' exit roots; the Miden proxy's L1-InfoTree indexer + the OP-Stack aggoracle both read/write it.

## Test flow — `scripts/e2e-l2-to-l2.sh` (skeleton on the branch)
1. Deploy ERC-20 `OPT0` on OP-Stack L2 (origin_network = OP-Stack rollupID, NOT L1).
2. **Forward (OP-Stack → Miden)**: `bridgeAsset(destNet=Miden, token=OPT0, amount)` on the OP-Stack bridge → wait for GER to include the new exit root → claim on Miden. Miden proxy sees a foreign-origin token → **provisions a faucet keyed by `hash(tokenAddress || origin_network)`** (the #108 (addr,network) keying) → mints the wrapped asset. Assert: faucet exists for `(OPT0, OP-Stack-net)`, wrapped balance correct, ClaimEvent at the exact consumption block (N-run exact-block check).
3. **Faucet isolation (absorbs #15)**: deploy an ERC-20 at the **same 20-byte address** on L1 and bridge it in too; assert the two resolve to **distinct** Miden faucets (no collision) — proves `(addr, origin_network)` keying.
4. **Back (Miden → OP-Stack)**: bridge-out from Miden (burn wrapped asset) → claim on OP-Stack → assert the round-trip returns `OPT0` to the original holder (balances net to zero on Miden, restored on OP-Stack).
5. Exact-block asserts throughout (0 missing/extra/locks), + an N-run loadtest variant.

## Hard parts / open questions (for the picker-upper)
- **agglayer multi-rollup config format** — need the exact `agglayer-config.toml` schema for a 2nd rollup + how RollupManager assigns the 2nd RollupID on L1. Check the agglayer 0.4.4 docs / an existing multi-rollup Kurtosis config.
- **OP-Stack bridge contract deployment** — reuse the agglayer bridge deploy scripts against the OP-Stack L2 RPC; wire its address into the aggkit + the test.
- **Root-owned data dirs** — the 2nd L2 will create its own data dir; apply the same root-container-clean fix (see memory `aggkit-0.8.3-rc1-aggoracle-wedge`) to it before every bringup.
- **Resource** — a 2nd full L2 roughly doubles container count; watch the node-RPC-under-load flakiness (same class as `e2e-claim-provenance`).

## Where I landed (this session)
- Branch `feat/l2-to-l2-e2e` created off `ec2e58f` with a **documented skeleton** `scripts/e2e-l2-to-l2.sh` (the 5-step flow as commented TODO stubs) + this notes file. No compose/chain wiring yet (that's the multi-day part).
- **Next step**: decide chain option (a) vs (b), then wire the 2nd rollup into `agglayer-config.toml` + a `docker-compose.l2l2.yml` override adding the OP-Stack L2 + its aggkit, and flesh out step 1-2 of the script (deploy + forward-bridge).
