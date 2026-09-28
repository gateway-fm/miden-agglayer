# Recover claims skipped by the origin-amount decoder

Issue #225 affected consumed CLAIM notes whose origin-token amount exceeded
`u64::MAX`. A scaled Miden mint could succeed while the proxy skipped its
ClaimEvent and left the linked Ethereum receipt pending. The fixed projector
preserves the full uint256 amount in the event, independently of the scaled
Miden amount.

A normal restart resumes at the saved projection cursor. It does not revisit
these older skipped notes. Use the existing `--restore --read-only` operation
with retained PostgreSQL to replay them through the shared projector.

1. Stop the proxy and back up PostgreSQL and the Miden client store. Rehearse
   on a same-host database copy first. Keep the transaction envelopes and
   note handoffs; they preserve the original Ethereum transaction hashes.
2. Run the fixed binary with the deployment's normal account, node, network,
   L1/L2 RPC, signer and database settings, plus `--restore --read-only`.
   Use the existing client store or the documented fresh-client restore
   procedure. No new claim submission is needed.
3. Check that `eth_getTransactionReceipt` succeeds at the CLAIM consumption
   block, `eth_getTransactionByHash` retains the original claimAsset calldata,
   and the exact-block ClaimEvent contains the original uint256 amount.
   Check the complete event audit before restarting normal service.

The [restore runbook](runbook.md) describes the full invocation and store
ownership requirements. Restore resets projection cursors and uses the same
per-note projection code as normal processing. The atomic event commit also
finalizes the linked pending receipt. Repeating the replay does not duplicate
the event. This recovery changes proxy history; it does not reset the chain or
mint the assets again.
