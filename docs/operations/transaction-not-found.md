# Acknowledged transaction missing after restart

## Cause and evidence

The September 16, 2026 SSH chaos run of the #194 → #197 → #198 stack
(`4ca55bad`) reproduced a claim that was acknowledged but could not be queried.
The affected hash was
`0x8f69c98d89112bd548e66924ac6173637f2bde4fa2257f1b4f61459eca72e3a1`,
from signer `0x635243a11b41072264df6c9186e3f473402f94e9`, nonce 15.

| UTC | Evidence |
|---|---|
| 11:28:08.899 | Proxy passes nonce admission; PostgreSQL creates the reservation |
| 11:28:48 | Chaos restarts the proxy; the original send ends with EOF |
| 11:28:50.934 | Bridge service resends the same signed transaction; proxy reports no stored transaction, durable intent or inflight job |
| 11:28:51.037 | Proxy returns `eth_sendRawTransaction: OK` for the hash |
| 11:28:51.925 onward | Bridge service repeatedly reports `error getting txByHash ... not found` |
| 11:30:08.910 | Persisted reservation lease expires; no envelope exists for recovery |
| 12:54:15 | Chaos verdict fails; database snapshot still contains only the reservation for this hash |

`reserve_nonce` protects one executor per signer/nonce. It runs before the
transaction envelope is durably saved. A process can die between those steps.
Before the fix, a same-hash retry while the old lease was still valid returned
`OwnedBySame`, which the RPC path turned into a successful hash unconditionally.
A reservation contains the hash but not the signed envelope: neither transaction
lookup nor the recovery worker can reconstruct the missing transaction from it.

The tested bridge-service revision (`2e87f97`) retains successfully submitted
hashes in `sync.monitored_txs.history`. Its `checkTxHistory` marks a missing hash
as not mined, and subsequent monitoring keeps polling instead of resubmitting
it. A send error removes that attempt from history and leaves it retryable.
Thus the incorrect success response converts a recoverable crash into a stall.
This also explains why waiting longer than the lease duration does not help.

The unconditional acknowledgment was introduced by `b82dcfa`
(`fix(claim): reservation is a fenced admission-lease lifecycle, not a bare row`,
July 14, 2026), to prevent duplicate execution across replicas. The exclusivity
requirement was valid; treating lease ownership as durable acceptance was not.
The affected admission code is identical on the tested #198 head and main
`7caae58`. This is separate from the claim execution lease
(`CLAIM_RESUBMIT_TTL_SECS`); increasing that timeout does not repair this gap.

The matching symptom alone does not establish this cause in another deployment.
Correlate an actual successful send with missing durable records and the
reservation before attributing a production incident to this bug.

## Fix and recovery behavior

A same-hash lease conflict now re-reads the transaction store. It returns success
only if the signed transaction is durable. Otherwise it returns JSON-RPC error
`-32005` with `transaction admission is still in progress; retry the same signed
transaction`. It neither steals the live lease nor starts a second executor.
Once the owner completes admission, or its lease expires and the sender retries,
the same transaction becomes queryable and follows normal recovery.

No database migration or lease-duration change is required. This prevents new
false acknowledgments. It does not reconstruct envelopes for already stranded
hashes: preserve sender history, nonce state and note handoffs first, then recover
and resubmit the exact original signed transaction through the normal RPC path.
Do not manufacture a receipt, advance the nonce manually, or clear the sender's
history wholesale. A null receipt alone is normal for a pending transaction and
is insufficient evidence for this procedure.

## Diagnostics

Add these directives to the proxy's existing `RUST_LOG` and restart through the
normal deployment procedure:

```text
RUST_LOG=info,rpc::admission=debug,writer_diagnostics=debug,miden_agglayer_service::service=debug
```

Search by exact transaction hash, and these messages:

- `nonce reservation acquired; durable admission has not completed`
- `signed transaction durably stored; advancing nonce before dispatch`
- `same-hash nonce reservation checked for durable admission`
- `nonce reservation exists without a durable transaction`
- `transaction admission finished`
- `eth_sendRawTransaction: OK`, `eth_sendRawTransaction: ERR`
- `eth_getTransactionByHash: unknown hash, returning null`

Admission traces include the hash, signer, nonce, fence or durable-row decision,
and admission outcome. They do not log signed envelopes or credentials.
`rpc_nonce_reservation_unadmitted_retry_total` counts retries refused because the
nonce lease exists without a stored envelope. An increase identifies the gap;
it is not itself proof of a stalled transaction because the retry can recover.

Collect these read-only queries with `psql -v tx_hash=0x...` against the proxy DB:

```sql
SELECT signer, nonce, tx_hash, state, fence_token, created_at,
       lease_expires_at, lease_expires_at <= now() AS expired
FROM nonce_reservations WHERE tx_hash = :'tx_hash';

SELECT tx_hash, signer, status, created_at, updated_at, error_message
FROM transactions WHERE tx_hash = :'tx_hash';

SELECT signer, nonce, tx_hash, expires_at
FROM queued_txns WHERE tx_hash = :'tx_hash';

SELECT tx_hash, note_id, handoff_state, prepared_expiration_block
FROM tx_note_links WHERE tx_hash = :'tx_hash';

SELECT n.address, n.nonce
FROM nonces n JOIN nonce_reservations r ON r.signer = n.address
WHERE r.tx_hash = :'tx_hash';
```

Also retain proxy logs spanning the first send, restart and retry; the sender's
send response and subsequent polls; both RPC lookup responses; and sender
monitoring history. `executing` plus no transaction/queue/handoff row and an
actual successful send is the distinctive signature. A durable pending row,
a prepared handoff, a rejected send or a synthetic event requires a different
investigation; see [writer stalls](writer-stalls.md).

## Reproduction and validation

The regression recreates the persisted crash state by reserving a nonce without
saving the envelope, then constructing new process state over the same store.
It does not require a slow chain or a proof. The HTTP regression uses real
PostgreSQL and the production JSON-RPC router with the test Miden client; a
zero-amount claim keeps proof execution outside the admission test.

Before the fix, both regressions fail: the send returns a hash while transaction
lookup and receipt lookup return null. After the fix, the send returns `-32005`;
after expiring that test reservation, the identical envelope succeeds, lookup
returns it, a success receipt appears and repeated submission advances the nonce
only once. A separate regression verifies that a durable live owner's lease is
not stolen. The concurrent HTTP regression sends bursts of 32 identical requests
to one active proxy before and after natural lease expiry, then recreates process
state and verifies that the completed transaction remains queryable without
being dispatched again. It checks one lease takeover and one nonce advance.
This matches the supported single-active-proxy topology; active-active execution
across proxies remains the separate scope of #142.

Run PostgreSQL tests only against an isolated test database with the numbered
migrations applied:

```sh
cargo test --locked --features postgres --lib reservation_only -- --nocapture
```

The original chaos fixture and failed-run evidence were preserved. These focused
tests do not replace the full chaos and database-loss validation of the stack.
