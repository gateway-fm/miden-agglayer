# Reject invalid claim proofs before publishing notes

During the September 23 release soak, deposit 373 was ready when the bridge
submitter processed a notification whose mainnet exit root covered only deposits
through 372. The submitter supplied a proof of the empty slot at index 373.
The proxy checked GER availability but not deposit inclusion, accepted the
transaction, and published a CLAIM note that the Miden bridge could never consume.
Recovery kept the linked receipt pending. Chaos correctly failed delivery at 29/30;
event completeness passed because the failed claim never emitted an event.

The public calldata and a later valid proof are retained in
`tests/fixtures/claim-deposit-373.json`. The old siblings folded with an empty leaf
produce the signed root `931799838b4f6cebc32635acdbd045e0d546cc4ab9eba9f2cd9a7409f3e36666`.
With the actual deposit leaf they produce
`3a3a61e455d891caf3806038a9413b5f6c5c33e02191f7c783f8f046c26c5ed9`.

The proxy now checks Keccak inclusion for mainnet and two-level rollup proofs,
using the original uint256 amount and metadata hash. Gas estimation returns
`execution reverted: InvalidSmtProof()` (`0xe0417cec`) once the GER is available.
Admission rejects before reserving a nonce; the writer also checks persisted
requests admitted by an older version. ClaimTxManager estimates before creating a
monitor, so new invalid proofs leave the deposit available for a later valid proof.
The existing zero-amount nonce no-op still creates no note and emits no claim event.

For existing pending requests, recovery first refreshes bridge and exact-note
state. An invalid proof with a definitely absent effect and an unconsumed exact
note is finalized as a failed transaction. An unlinked invalid request can also
be rejected. Missing or ambiguous note identity stays pending. An exact consumed
note remains projector-owned. Atomic storage compares the handoff identity,
preserves terminal receipts and note history, and releases only the failed hash's
non-landed claim reservation. It never marks the deposit unclaimable or emits a
ClaimEvent. A replacement is a new signed transaction carrying a valid proof;
the old transaction's calldata and hash remain unchanged.

An already-created upstream monitor may require an operator to submit a fresh
claim after the old receipt is rejected. This change does not rewrite upstream
monitor history or repair the upstream root-notification race. Existing failed
soak verdicts remain failed. The exploratory historical `receipt.logs` limitation
is outside this change.
