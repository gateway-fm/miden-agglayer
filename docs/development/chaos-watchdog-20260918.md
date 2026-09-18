# Release-soak watchdog failure, 18 September 2026

The `988c18d` clean soak passed three full E2Es and one complete growing-chain
cycle. Cycle 2 failed during chaos: all 30 transfers were submitted, only one
was delivered, and Aggkit was stopped. The attempt remains failed; recovery does
not retroactively pass its delivery or final-completeness gates.

## Cause

The watchdog combined two unrelated observations: a count of
`already exists in monitoring DB` lines, and the last `ID: 0x…` anywhere in the
container logs. The latter also matches `CertificateID`. At least 21 of the 26
recorded attempts targeted hashes found in preserved certificate records.

Even an injection's monitoring ID is not its signed transaction hash. The final
incident contains these three different identities:

| Identity | Value |
| --- | --- |
| Incorrectly selected certificate | `0x944336df53ee5afd1fc85fd9614c9b4f6fadb3dedb195492d92cf0eeee020107` |
| Actual injection monitoring record | `0x51211c69b95d4ed609e9fe91d9c91080228842e79c541e60dc033d74486ccce0` |
| Linked signed transaction | `0x2adce5394f0316e843ace49fbe9f6c19c0f64f675a32a89b1f30123e3718d9d1` |

The `signed tx sent to the network` line carries the monitoring ID in its
`monitoredTxId` field. Only the signed hash belongs in the proxy's
`transactions.tx_hash` admission probe. That signed hash was present when the
incident was investigated. Absence of a certificate or monitoring ID from that
table says nothing about whether the injection was admitted.

The final warning burst spanned only eight seconds at the watchdog observation.
Counting ten lines did not establish the documented 60-second persistent wedge.

The unnecessary heal then overlapped an intentional proxy restart:

| UTC | Evidence |
| --- | --- |
| 00:47:39 | Watchdog begins recovery for the certificate ID. |
| 00:47:42 | Aggkit is recreated; content and ownership manifests match. |
| 00:47:44 | Chaos restarts the proxy. |
| 00:47:46 | Aggkit startup fails its bridge sanity check: proxy connection refused. |
| 00:47:48 | Docker starts Aggkit again; its logs show successful startup and processing. |
| 00:48:08 | Aggkit stops; helper returns failure. Retained restart count is five. |

The helper treated any restart since recreation as terminal failure and called
`docker stop`, even after the dependency and service had recovered. Its fixed
25-second log window could also include fatal output from an earlier process.
The retained lifecycle and logs match this failure path, reproduced by the
regression tests. The precise predicate was not logged: the watchdog redirected
the helper's entire output to `/dev/null`.

## Fix and validation

- Parse only aggoracle injection records and explicitly correlate their signed
  broadcasts. Require same-record retries spanning 60 seconds, fresh evidence,
  and a successful database probe showing all linked signed hashes absent.
- Revalidate under the healer's lock; automatic calls cannot inherit `FORCE=1`.
- Preserve the content/ownership checks. After restore, require a new stable
  process window within a bounded deadline. Health/proof failure retains
  diagnostics and leaves the intact service under Docker's restart policy.
- Confirm admission using a signed hash from post-heal injection logs. Preserve
  the distinct deferred-proof exit for full database-loss drills.
- Retain attempt logs, compressed trigger/container logs and final lifecycle
  state. Bound failed attempts as well as successful ones.

`python3 scripts/test-aggkit-recovery.py` covers identity confusion, transient
and permanent failures, unavailable probes, real missing transactions, admission
proof, deferred recovery, corrupt restores, attempt limits and diagnostic
retention. `make test-scripts` runs this suite on Linux in CI. The tests substitute
Docker and touch no live fixture. Historical replay uses the preserved continuous
logs; current database contents cannot prove historical admission after a later
database-loss drill.

This change is confined to this repository's validation/recovery scripts. It
does not patch Aggkit or other AggLayer components and does not resolve the
separate retained-PG restore-window bug [#222](https://github.com/gateway-fm/miden-agglayer/issues/222).
