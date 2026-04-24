# L2 → L1 bridge-out · Bali testnet

Broadcast `2026-04-24T14:06:33Z` · Claim `2026-04-24T15:50:37Z` · **Total elapsed 1h 44m** · ~65m of that was Polygon-side settler nonce self-resolve; happy-path ≈ 30–45m

## L2 SIDE (MIDEN)

| Field | Value |
| --- | --- |
| RPC | `https://rpc.testnet.miden.io:443` |
| Miden tx | `0xb645d24a…d5e6d75d` (block 119445) |
| B2AGG note | `0xb96a3033…2fb408` **CONSUMED** |
| Wallet balance before | 10000 Miden-ETH units |
| Wallet balance after | 0 (Δ −10000 = −0.0001 ETH) |
| Faucet | `0xa88a59eb97990060612bc4a6c2f0dc` |

## PROXY — AGGLAYER SYNTHETIC LOG

| Field | Value |
| --- | --- |
| Synth tx | `0xb5e2f477a926a1364ba0688eaa4c61b716fe68e7bef750cbb3419471c035f483` |
| Synth block | 119451 |
| Deposit count | 0 (first-ever L2→L1 on this deployment) |
| Amount | 100000000000000 wei (0.0001 ETH) |
| Dest network / addr | 0 / `0xEFAD2016599b886A457Ffbf313dae2a7A4bfaa27` |
| Global index | 309237645312 |

## AGGSENDER CERTIFICATE — PROGRESSION

| Time | Status | Cert |
| --- | --- | --- |
| 14:24:01Z | Pending | `0x11d4d425…c12fb9` |
| 14:27:09Z (+3m8s) | Proven | `0x11d4d425…c12fb9` |
| 14:32:09Z (+8m8s) | InError | SettlementError: replacement transaction underpriced (settler EOA nonce collision) |
| ~15:32Z (self-resolve) | Pending (retry 1) | `0x6e180f0b…46148e9` |
| 15:37:09Z (+5m31s) | Settled ✓ | `0x6e180f0b…46148e9` |

| Field | Value |
| --- | --- |
| PreviousLocalExitRoot | `0x27ae5ba08d7291c96c8cbddcc148bf48a6d68c7974b94356f53754ef6171d757` |
| NewLocalExitRoot | `0xa822866a392d5d5226793db37c42ca4452d9d0d778702d0c5510f61eff5539da` |
| Settlement tx (L1) | `0xcd92fabb6e49610fd9e4b554cb3889fdaa3338ce8ca737cbb54f3be93bcfe580` (Sepolia block 10723771) |
| AggLayer settler EOA | `0x3053c702…6c559a` |

## L1 CLAIM — SEPOLIA

| Field | Value |
| --- | --- |
| Claimer EOA (pays gas) | `0xEFAD2016599b886A457Ffbf313dae2a7A4bfaa27` |
| Dest EOA (recipient) | `0xEFAD2016599b886A457Ffbf313dae2a7A4bfaa27` (same — delta = amount − gas) |
| claimAsset tx | `0x391fa2515216d2f5e0b5113d4abbbf2159d57f743c0203adf4fec94f58c23c91` (block 10723851) |
| Receipt status | success (0x1) |
| Gas used × price | 131,339 × 9,234,510 wei = 1,212,851,308,890 wei (gas cost) |
| ClaimEvent | addr `0x1348…d1f` · topic0 `0x1df3f2a973…fda4d` (PolygonZkEVMBridgeV2.ClaimEvent) |
| bridge-api claim_tx_hash | `0x391fa2515216d2f5e0b5113d4abbbf2159d57f743c0203adf4fec94f58c23c91` **populated** |
| isClaimed on-chain | true |
| L1 balance before | 100,487,612,856,866,724,750 wei |
| L1 balance after | 100,487,711,644,015,415,860 wei |
| **Balance Δ** | **+98,787,148,691,110 wei** |
| expected (amount − gas) | +98,787,148,691,110 wei · exact match ✓ |

## OBSERVABILITY

| Field | Value |
| --- | --- |
| Loki namespace | `outpost-testnet-miden-testnet` |
| Grafana | `https://grafana.dev.eu-north-3.gateway.fm` |
| Cert Loki filter | `\|~ "11d4d425\|6e180f0b"` |
| Synth Loki filter | `\|~ "b5e2f477"` |
| Etherscan | `https://sepolia.etherscan.io/tx/0x391fa2515216d2f5e0b5113d4abbbf2159d57f743c0203adf4fec94f58c23c91` |

---

First-ever L2 → L1 round-trip on Bali · Miden burn → synthetic BridgeEvent → aggsender cert (InError → auto-retry → Settled) → claimAsset → dest EOA funded exactly amount − gas.
