# Load funding recovery on the stock bridge service

The September 23 soak stopped before its N30 workload: ten L1 funding deposits
were indexed but never became ready. The stock bridge service observed an L2
GER before the corresponding L1 record, skipped its incomplete notification,
and did not replay that notification when L1 filled in the roots. Advancing
indexers alone could therefore pass preflight while funding remained blocked.

The load now matches its successful funding transaction hashes, waits for those
exact deposits, and uses the existing L2B certificate nudge after 60 seconds if
all deposits are indexed but some remain unready. Six attempts at most fit
inside the 600-second readiness window. A failed or ambiguous send stops the
helper. Every attempt and observation is retained. A readiness timeout stops
setup immediately; it no longer continues into 30 minutes of faucet polling.

The nudge is ordinary test traffic through unchanged components. It does not
write readiness flags, rebuild an index, or certify delivery. Existing balance,
receipt, workload and exact-block completeness gates remain authoritative.
This is bounded fixture recovery; the upstream missed-notification race still
exists and may require ordered index replay if nudging cannot recover it.

Live evidence: one existing certificate nudge at 17:22:13 UTC made all ten
previously stuck deposits ready at 17:22:38 UTC on the original chains. Evidence
is retained in `~/bridge-readiness-live-20260923/` on the soak host. This exposed
a separate stale ClaimTxManager nonce after the earlier proxy database loss;
its existing post-restore sender recovery requires separate claim validation.

Eight regression cases cover exact funding identity, malformed evidence,
already-ready deposits, recovery after a notification miss, unindexed deposits,
ambiguous sends, and exhausted recovery budgets.
