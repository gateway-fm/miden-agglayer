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

---

## UPDATE (2026-07-09): L1 registration PROVEN live + recipe captured

Dry-ran against the real anvil snapshot (brought up the `anvil` service alone). **Rollup #2 registration works end-to-end** — see `scripts/setup-l2b.sh` (steps 1-2 verified, step 3 bytecode-extraction verified viable):

1. **Decoded the snapshot's own creation txs** (`fixtures/l1-raw-txs.txt`, blocks 83-85):
   - blk83 `addNewRollupType(0xabcb5198)`: consensusImpl `0xFB054898…` (AggchainECDSAMultisig), verifier 0, forkID 0, verifierType 2, genesis 0, "kurtosis-devnet", vkey 0 → **rollupTypeId 1 (reusable for rollup #2 — no new type needed)**.
   - blk84 `attachAggchainToAL(0x97d289a3)`: `(typeId=1, chainID=2, abi.encode(aggchainAdmin))`.
   - blk85 aggchain init (selector `0x697427f6`): `(admin, trustedSequencer, gasToken, sequencerURL, networkName, bytes32(0), signers[(addr,url)], threshold)`. **The original rollup #1 was an OP-reth sovereign chain** — its init literally contains `http://op-el-1-op-reth-op-node-001:8545` / `"op-sovereign"`. The Miden proxy replaced it. Our L2B mirrors the same consensus shape, so a plain anvil L2B is consensus-equivalent (mock-verifier + ECDSA-multisig certs).
2. **Live dry-run results**: `attachAggchainToAL(1, 31338, abi.encode(0xE34a…))` from the admin key → rollupCount 2, aggchain at `0x5D1A491A…bd0E` (address is snapshot-deterministic but the script reads it from `rollupIDToRollupData(2)`); hand-built init calldata (layout above) → `trustedSequencer=0x5b06… ✓ networkName="l2b-sovereign" ✓ threshold=1 ✓`.
3. **Keys**: admin `0xE34aaF64…9970` = kurtosis-cdk standard key (in setup-l2b.sh, TEST-ONLY); committee[0] reuses the existing `sequencer.keystore` (`0x5b06…`) so **aggkit-l2b's aggsender can reuse the same keystore** and agglayer `[proof-signers] 2` = same signer.
4. **Bridge on L2B**: L1 bridge impl (PolygonZkEVMBridgeV2, 13150 bytes at EIP-1967 impl of `0xC8cb…`) extracts cleanly via `cast code` → `anvil_setCode` on L2B at the same proxy address + fresh `initialize(networkID=2, …)` (step 3 of setup-l2b.sh, written not yet exercised).

### Remaining (next session)
- **Sovereign L2-GER contract on L2B** — not on the L1 snapshot; vendor a minimal contract with the sovereign ABI (`insertGlobalExitRoot`, `updateExitRoot`, `globalExitRootMap`) at `0xa40D…` or compile the real `GlobalExitRootManagerL2SovereignChain`.
- **Compose override** `docker-compose.l2l2.yml`: `anvil-l2b` (plain anvil, chain-id 31338, port 9545) + `aggkit-l2b` (copy of aggkit config with `L2URL=anvil-l2b`, RollupID/NetworkID 2, same keystores) + run `setup-l2b.sh` as a one-shot service after anvil healthy.
- **agglayer config**: add `[full-node-rpcs] 2 = "http://anvil-l2b:8545"` + `[proof-signers] 2 = "0x5b06…"` (use a separate `agglayer-config-l2l2.toml` mounted by the override, so the base stack is untouched).
- **bridge-service config**: append L2B to the `L2URLs` / `L2PolygonBridgeAddresses` / `RequireSovereignChainSmcs` / `L2PolygonZkEVMGlobalExitRootAddresses` lists (it's already multi-network by design).
- Then flesh out `e2e-l2-to-l2.sh` steps 1-2 (ERC20 on L2B → bridgeAsset → cert → GER → claim on Miden).

---

## UPDATE 2 (2026-07-09): full L2B wiring written — ready for live smoke

- **`docker-compose.l2l2.yml`** — override adding `anvil-l2b` (chain-id 31338, :9545) + `aggkit-l2b` (aggoracle+aggsender, reuses aggoracle/sequencer keystores) + swaps agglayer/bridge-service configs for network-2-aware variants.
- **`scripts/gen-l2b-configs.sh`** — derives `agglayer-config-l2l2.toml` / `aggkit-l2b-config.toml` / `bridge-config-l2l2.toml` from the base fixtures at setup time (gitignored; assert-guarded so base-config drift fails loudly).
- **`fixtures/SovereignGER.sol`** — minimal sovereign-GER (insertGlobalExitRoot/updateExitRoot/globalExitRootMap + events), setCode-deployable (no constructor; `initialize(bridge, updater)`).
- **`scripts/setup-l2b.sh`** extended: rollup-2 address guard, L2B account funding, GER-stub deploy at `0xa40D…` (updater = aggoracle addr derived from keystore at runtime), bridge proxy+impl setCode + `initialize(networkID=2,…)` — all idempotent.
- **`scripts/e2e-l2-to-l2.sh` step 0** wired: gen-configs → compose-up L2B services → wait → setup-l2b.

### Next session
1. **Live smoke step 0**: base stack up (`make e2e-up` w/ root-clean), then `./scripts/e2e-l2-to-l2.sh` — expect: rollup #2 registered, L2B bridge `networkID()==2`, GER stub live, aggkit-l2b logs syncing (not crash-looping). Watch: agglayer accepting config with an unreachable-then-reachable network 2; aggkit-l2b's `L1ChainID=31338` assumption; bridge impl `initialize` ABI matching this contracts version (if it reverts, decode the L1 bridge's own init tx from l1-raw-txs for the exact signature — same technique as blk83-85).
2. **Step 1**: `forge create fixtures/TestToken.sol:TestToken` against :9545 → OPT0; approve bridge; `bridgeAsset(destNet=1, …)` on L2B.
3. **Step 2**: watch aggsender-l2b cert → agglayer settle → L1 GER → Miden aggoracle inject → claim on Miden via bridge-service proof (`/merkle-proof` for network 2) → assert foreign-origin faucet keyed (OPT0, net-2).

---

## UPDATE 3 (2026-07-09): FORWARD PIPELINE PROVEN LIVE — L2B deposit settled to L1 + GER on Miden

Full live run on the real stack (base + `docker-compose.l2l2.yml`):

1. `setup-l2b.sh` end-to-end ✓ (idempotent): rollup #2 attached+initialized on L1; L2B funded; SovereignGER stub setCode'd+initialized; bridge impl+proxy setCode'd + **initialize gated by the ProxyAdmin-owner** (the fork reads the EIP-1967 admin slot and staticcalls `owner()` — solved by replicating the L1 ProxyAdmin at `0xd60F1B…` with our admin as owner); **getTokenMetadata helper** at `0xcC87d4…` (immutable in the impl) copied from L1 — without it every ERC-20 `bridgeAsset` bare-reverts.
2. **aggkit-l2b healthy** (no crash-loop) — synced rollup 2, connected to agglayer.
3. `OPT0` deployed on L2B; `bridgeAsset(destNet=1, 500 OPT0)` → `depositCount=1`, GER stub `lastRollupExitRoot=0xe3c6b488…`.
4. **aggsender-l2b built a cert → agglayer SETTLED it** (`settled certificate from AggLayer: 0/0xbbcc2031…`; agglayer NetworkTask network_id=2).
5. **L1**: rollup #2 `lastLocalExitRoot == 0xe3c6b488…` (exact match), rollupExitRoot updated, new GER `0x3e591e9e…`.
6. **Miden**: `zkevm_getLatestGlobalExitRoot == 0x3e591e9e…` — the Miden aggoracle injected the cross-L2 GER.

`e2e-l2-to-l2.sh` steps 1–2a now encode this proven flow (deploy + bridgeAsset + GER-propagation wait).

### Debug lessons (this stack's bridge fork)
- Bare `execution reverted, data: "0x"` from the bridge → `cast send --gas-limit 3M` + `cast run <hash>` traces the real cause ("call to non-contract address X").
- Two hidden L1 dependencies must be replicated on any fresh EVM L2: the **ProxyAdmin** (initialize gate) and the **metadata helper** (bridgeAsset). Both are now in setup-l2b.sh.
- Transparent-proxy note: with an empty admin slot, `cast call` (default `from=0x0`) hits the admin dispatch and reverts — set the admin slot (done) or pass `--from`.

### Remaining for the full e2e
- **Step 2b**: claim on Miden — bridge-service `/merkle-proof` for a network-2 deposit + `claimAsset` via the proxy → assert foreign-origin faucet keyed `(OPT0, net 2)` (#108) + wrapped balance + exact-block ClaimEvent. (bridge-service indexes network 2 via the generated config; verify its sync of anvil-l2b.)
- **Step 3**: same-address/different-origin faucet isolation (absorbs #15).
- **Step 4**: Miden → L2B back-bridge (bridge-out + claim on L2B against an injected GER — needs aggoracle-l2b GER injection into the stub, already wired).
- **Step 5**: exact-block asserts + N-run variant; wire into `e2e-test.sh` as `l2-to-l2`.
