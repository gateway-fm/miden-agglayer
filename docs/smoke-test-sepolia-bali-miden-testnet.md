# Smoke Test Report — Sepolia → Bali → Miden Testnet

| Stage | Value |
| --- | --- |
| L1 deposit tx (0.001 ETH, same bridge wallet) | `0x9fdf66f3cb29cd954258be3c5cddff45e5494ab8a63245fca6e52693e3a6b31a` |
| Broadcast time | `2026-04-24T11:58:38Z` |
| Sepolia block | `0xa39dea` (10722282) |
| Dest Miden addr (v0.14.4 Poseidon2-derived) | `0xd4f1cf38ec8c3210627fd2ea8fdde1` |
| Dest eth-padded | `0x00000000d4f1cf38ec8c3210627fd2ea8fdde100` |
| Account deployment tx (on Miden) | `0x5311b60bbfc3f65d2b64624ec93c9573a32e91ec02ce5385dfa6301849b16644` |
| Global index | `18446744073710679266` |
| L2 miden_tx (claim) | `0xbcfabb1b6d1034173bde946b74916a40209b1e07182a2491abe6f7535666b8d1` |
| L2 eth_tx (ClaimEvent) | `0x7249ccf6475c374d64150728eef2db5bacf7427beb6bbf0188d66c68e9ff1e70` |
| Claim note id (from log) | `0xc5f6e5566b99776cfbd473885768fc61729cf8c7a27f47e0eb005ee6d3a1d4c8` |
| Real on-chain Miden NoteId (from sync) | `0x4ca2850b10864012cf8401b6f484a99bbd7734d463d3507cdd404f992f14ca82` |
| L2 miden block (claim commit) | `117289` |
| Consume tx (user's account absorbs the note) | `0xdadb054060bcc0dc44b9ff37bd3370b81f5d2c6daba3e9409f00bfdeaf731717` |
| Consume synced to block | `117336` |
| Amount received | `100000` miden-eth units (= 0.001 ETH × 10⁸, scale=10 correctly applied) |
| Faucet id | `0xa88a59eb97990060612bc4a6c2f0dc` |
| Broadcast time | `2026-04-24T11:58:38Z` |
| Sepolia block | `0xa39dea` (10722282) |
| Dest Miden addr (v0.14.4 Poseidon2-derived) | `0xd4f1cf38ec8c3210627fd2ea8fdde1` |
| Dest eth-padded | `0x00000000d4f1cf38ec8c3210627fd2ea8fdde100` |
| Account deployment tx (on Miden) | `0x5311b60bbfc3f65d2b64624ec93c9573a32e91ec02ce5385dfa6301849b16644` |
| Global index | `18446744073710679266` |
| L2 miden_tx (claim) | `0xbcfabb1b6d1034173bde946b74916a40209b1e07182a2491abe6f7535666b8d1` |
| L2 eth_tx (ClaimEvent) | `0x7249ccf6475c374d64150728eef2db5bacf7427beb6bbf0188d66c68e9ff1e70` |
| L2 miden_tx (claim) | `0xbcfabb1b6d1034173bde946b74916a40209b1e07182a2491abe6f7535666b8d1` |
| L2 eth_tx (ClaimEvent) | `0x7249ccf6475c374d64150728eef2db5bacf7427beb6bbf0188d66c68e9ff1e70` |
| Claim note id (from log) | `0xc5f6e5566b99776cfbd473885768fc61729cf8c7a27f47e0eb005ee6d3a1d4c8` |
| Real on-chain Miden NoteId (from sync) | `0x4ca2850b10864012cf8401b6f484a99bbd7734d463d3507cdd404f992f14ca82` |
| L2 miden block (claim commit) | `117289` |
| Consume tx (user's account absorbs the note) | `0xdadb054060bcc0dc44b9ff37bd3370b81f5d2c6daba3e9409f00bfdeaf731717` |
| Consume synced to block | `117336` |
| Amount received | `100000` miden-eth units (= 0.001 ETH × 10⁸, scale=10 correctly applied) |
| Faucet id | `0xa88a59eb97990060612bc4a6c2f0dc` |
| Pipeline duration | **19m31s** (broadcast → claim committed) |
| Status | ✅ Full success |
