# Resume runtime faucet provisioning after a restart

The `117fb14` soak's first chaos cycle exposed a separate defect from the claim-proof bug fixed by #232. The MOP claim `0xe50e44192ac316a635b38ab4c8bf112453fcf5087b351e8c1d2b63b0c3aa6d2e` funded faucet `0x3cdbe6f614f2f4115a495cd1676f9f` with 215040 fee units at 19:58:22 UTC on September 24. Its deployment committed at 19:58:42. The proxy received SIGTERM at 19:58:44 before it saved the completed registry row. Retrying the same claim funded a second faucet, `0xe4a8b88a02edee91517dca79c81d0d`, at 19:59:12.

The final COL probe then had only 178500 units available for its 215040-unit cascade. The stage had budgeted 24 new faucets, and the duplicate consumed the reserve for the last legitimate one. Raising that budget would hide the duplicate deployment.

## Runtime behavior

Claim auto-creation and admin creation now share a durable provisioning journal in the proxy Store. The bridge ID, origin network and token address select one initial account, saved with its creation seed before any funding. Its immutable binding includes the chain genesis, service, bridge, token metadata, decimals, fee asset and funding policy. Competing requests use the first stored identity; conflicting bindings fail.

Each funding, deployment and registration transaction is executed and proved, then its exact `TransactionResult` and `ProvenTransaction` are saved before submission. The journal retains all attempts. A restart first synchronizes with the chain and checks the exact public output note, or the deployed account for the deployment stage:

- A committed effect advances the workflow even if the client never applied the outgoing transaction locally.
- An unresolved transaction within its expiration block is replayed from the saved bytes.
- A replacement may be prepared only after synchronization beyond the old transaction's expiration and a successful lookup proving its effect absent. It uses the same faucet identity.
- RPC errors, unreadable records, mismatched bindings and invalid handoffs fail closed. A timeout or submission acknowledgment is not treated as commitment.

The completed `faucet_registry` and existing on-chain registration recovery remain in place. Bootstrap and standalone tool lifecycles are unchanged. Migration `029_faucet_provisioning.sql` is additive and embedded in the startup migrator. Preserve these journal tables in backups: destroying both the journal and client store during an incomplete, unregistered deployment is outside this restart guarantee. A fully registered faucet remains recoverable from the chain.

## Validation and limits

The regressions exercise concurrent identity/transaction selection, restarts before send, lost submission responses, interruption after commit before local apply, exact replay until expiry, late commitment before recovery, unavailable lookup, immutable generations, seed round-trip, and real SDK transaction serialization. The same journal/restart contract is run against PostgreSQL. A mock-node test exercises the pinned SDK's empty-note response, subsequent commitment without local apply, and RPC failure.

This fix does not explain the separate TT5 exit delay in the same chaos run. That exit's successful L1 receipt appeared at 21:08:57 UTC, 22 seconds after the strict 29/30 workload verdict. The failed run stays FAILED. Neither unit tests nor a later recovery changes a soak verdict or counts as a completed cycle/full E2E.
