#!/usr/bin/env bash
# ══════════════════════════════════════════════════════════════════════════════
# e2e-full-db-loss-recovery.sh — FULL proxy state loss (postgres store AND
# miden-client sqlite) must restore a FAITHFUL history (#88), keep aggkit's
# cert lineage alive (#89), and mint no poison notes (#86).
#
# This is the disaster-recovery scenario the 2026-08-08 soak proved lossy:
# the drop-restore rebuilt ger_entries with is_injected=false and re-emitted
# NO historical UpdateHashChain logs (MA#28 skipped every historical
# UpdateGerNote as MissingMetadata once the client store was wiped too), so
# aggoracle re-injected registered GERs — every re-inject minting an immortal
# ERR_GER_ALREADY_REGISTERED poison note — and aggsender's next cert
# double-imported claims ("non-inclusion proof for a key present in the SMT").
#
# Post-fix (a0b824f) the restore's block walk feeds node-authoritative
# metadata to the MA#28 check, so the restored synthetic history must be
# IDENTICAL to the pre-drop history. This script asserts exactly that:
#
#   1. UpdateHashChain log count   AFTER == BEFORE
#   2. is_injected count           AFTER == BEFORE
#   3. hash_chain_value            AFTER == BEFORE  (order-sensitive replay)
#   4. BridgeEvent + ClaimEvent    AFTER == BEFORE  (#69/#136 held too)
#   5. ntx-builder: ZERO ERR_GER_ALREADY_REGISTERED kernel asserts post-restore
#   6. liveness: a NEW L1→Miden deposit reaches ready_for_claim and a NEW GER
#      is injected+consumed (is_injected grows) — pipeline works, not just data
#
# Runs against the LIVE stack (any compose project); state accumulated so far
# is the test fixture. DESTRUCTIVE to the proxy store by design — that is the
# scenario. Requires the post-#88 image to be the one the stack runs.
# ══════════════════════════════════════════════════════════════════════════════
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
ENV_FILE="$PROJECT_DIR/fixtures/.env"

export MIDEN_NODE_GIT_URL="${MIDEN_NODE_GIT_URL:-$(grep -m1 '^MIDEN_NODE_GIT_URL' "$PROJECT_DIR/Makefile" | sed 's/.*= *//')}"
export MIDEN_NODE_GIT_REF="${MIDEN_NODE_GIT_REF:-$(grep -m1 '^MIDEN_NODE_GIT_REF' "$PROJECT_DIR/Makefile" | sed 's/.*= *//')}"
export WEB3SIGNER_UID="${WEB3SIGNER_UID:-$(id -u)}" WEB3SIGNER_GID="${WEB3SIGNER_GID:-$(id -g)}"

# PR#164 blocker #6 — this test IRREVERSIBLY DROPS a database. It must NEVER
# guess its victim. Require an explicit PROXY_CONTAINER, or auto-select ONLY
# when exactly one candidate is running; refuse on ambiguity.
CANDIDATES="$(docker ps --format '{{.Names}}' | grep -E -- '-miden-agglayer-1$' || true)"
if [[ -n "${PROXY_CONTAINER:-}" ]]; then
    grep -qxF "$PROXY_CONTAINER" <<<"$CANDIDATES" \
        || { echo "FATAL: PROXY_CONTAINER='$PROXY_CONTAINER' is not a running *-miden-agglayer-1 container"; exit 1; }
else
    n_cand="$(grep -c . <<<"$CANDIDATES" 2>/dev/null || echo 0)"
    if [[ "$n_cand" -eq 0 ]]; then
        echo "FATAL: no running *-miden-agglayer-1 container"; exit 1
    elif [[ "$n_cand" -gt 1 ]]; then
        echo "FATAL: $n_cand proxy stacks are running — refusing to guess which DB to DROP."
        echo "       Set PROXY_CONTAINER=<one of> explicitly:"; sed 's/^/         /' <<<"$CANDIDATES"
        exit 1
    fi
    PROXY_CONTAINER="$CANDIDATES"
fi
PROJECT="${PROXY_CONTAINER%-miden-agglayer-1}"
PG_CONTAINER="$PROJECT-agglayer-postgres-1"
NTX_CONTAINER="$PROJECT-ntx-builder-1"
AGGKIT_CONTAINER="$PROJECT-aggkit-1"
export COMPOSE_PROJECT_NAME="$PROJECT"

# Verify the postgres we are about to DROP is THIS proxy's configured store:
# the container must exist, be in the same compose project, and the proxy must
# actually reference it (its store URL names this postgres host). A destructive
# test must confirm — not assume — its target.
docker inspect "$PG_CONTAINER" >/dev/null 2>&1 \
    || { echo "FATAL: expected store container $PG_CONTAINER not found for proxy $PROXY_CONTAINER"; exit 1; }
PROXY_PROJECT_LABEL="$(docker inspect "$PROXY_CONTAINER" --format '{{ index .Config.Labels "com.docker.compose.project" }}' 2>/dev/null)"
PG_PROJECT_LABEL="$(docker inspect "$PG_CONTAINER"    --format '{{ index .Config.Labels "com.docker.compose.project" }}' 2>/dev/null)"
[[ -n "$PROXY_PROJECT_LABEL" && "$PROXY_PROJECT_LABEL" == "$PG_PROJECT_LABEL" ]] \
    || { echo "FATAL: $PG_CONTAINER (project '$PG_PROJECT_LABEL') is not in the proxy's project ('$PROXY_PROJECT_LABEL')"; exit 1; }
# PR#164 re-review — this MUST fail closed. Previously an unconfirmed target only
# printed a WARN and the script proceeded to DROP DATABASE anyway, so a config
# mismatch (or a store URL we could not parse) still destroyed a database chosen by
# NAME CONVENTION alone. Extract the store URL's HOST and require it to name this
# postgres; an unparsable or mismatched URL is a hard stop, not a warning.
# Override with ALLOW_UNVERIFIED_PG_TARGET=1 for exotic topologies — explicit, not
# accidental.
PROXY_ENV="$(docker inspect "$PROXY_CONTAINER" --format '{{range .Config.Env}}{{println .}}{{end}}' 2>/dev/null)"
# Select the connection string by EXACT variable name. A fuzzy
# `(DATABASE|STORE|POSTGRES)` pattern matched `AGGLAYER_INSECURE_LOCAL_KEYSTORE`
# ("KEY-STORE"!) and parsed its value `true` as the store host, so this guard
# fail-closed on a perfectly healthy stack and aborted the whole recovery gate.
# A safety check that blocks correct runs gets disabled, which is worse than no
# check — so match precisely.
STORE_URL="$(printf '%s\n' "$PROXY_ENV" | sed -n 's/^DATABASE_URL=//p' | head -1)"
[[ -z "$STORE_URL" ]] && STORE_URL="$(printf '%s\n' "$PROXY_ENV" \
    | grep -E '^[A-Z_]*(DATABASE|POSTGRES)_URL=' | head -1 | cut -d= -f2-)"
# Two accepted encodings:
#   libpq keywords : "host=agglayer-postgres user=… dbname=…"   (what this stack uses)
#   URL            : "postgres://user:pass@HOST:port/db"
if [[ "$STORE_URL" == *host=* ]]; then
    STORE_HOST="$(sed -E 's/.*(^|[[:space:]])host=([^[:space:]]+).*/\2/' <<<"$STORE_URL")"
else
    STORE_HOST="$(printf '%s' "$STORE_URL" | sed -E 's#^[a-zA-Z+]+://##; s#^[^@]*@##; s#[:/].*$##')"
fi
# The proxy addresses postgres by its compose SERVICE name; accept that or the
# container name (`<project>-<service>-1`) — and NOTHING else. Review 0814:
# `localhost`/`127.0.0.1` are NOT acceptable proof — in this topology localhost
# is the proxy container itself, not $PG_CONTAINER, so accepting it would let
# the drill destroy a database the proxy under test does not use.
PG_SERVICE="${PG_CONTAINER#"$PROJECT-"}"; PG_SERVICE="${PG_SERVICE%-1}"
# The DB NAME must also be confirmed before `DROP DATABASE agglayer_store`:
# libpq keyword form carries `dbname=`, URL form carries it as the path.
if [[ "$STORE_URL" == *dbname=* ]]; then
    STORE_DB="$(sed -E 's/.*(^|[[:space:]])dbname=([^[:space:]]+).*/\2/' <<<"$STORE_URL")"
else
    STORE_DB="$(printf '%s' "$STORE_URL" | sed -E 's#^[a-zA-Z+]+://[^/]*##; s#^/##; s#[?].*$##')"
fi
if [[ -z "$STORE_HOST" || -z "$STORE_DB" ]]; then
    if [[ "${ALLOW_UNVERIFIED_PG_TARGET:-0}" == "1" ]]; then
        echo "WARN: proxy store URL host/dbname unparsable; proceeding because ALLOW_UNVERIFIED_PG_TARGET=1"
    else
        echo "FATAL: could not parse the proxy's store host+dbname from $PROXY_CONTAINER env"
        echo "       (host='$STORE_HOST' dbname='$STORE_DB'), so the DROP DATABASE target cannot"
        echo "       be confirmed. Refusing to destroy a database chosen by name convention"
        echo "       alone. Set ALLOW_UNVERIFIED_PG_TARGET=1 to override."
        exit 1
    fi
elif [[ "$STORE_HOST" != "$PG_CONTAINER" && "$STORE_HOST" != "$PG_SERVICE" ]] \
        || [[ "$STORE_DB" != "agglayer_store" ]]; then
    if [[ "${ALLOW_UNVERIFIED_PG_TARGET:-0}" == "1" ]]; then
        echo "WARN: proxy store target '$STORE_HOST/$STORE_DB' != '$PG_SERVICE/agglayer_store'; proceeding (ALLOW_UNVERIFIED_PG_TARGET=1)"
    else
        echo "FATAL: the proxy's store target is '$STORE_HOST' db '$STORE_DB', which is NOT the"
        echo "       exact database this script would DROP ($PG_SERVICE / agglayer_store)."
        echo "       Refusing to destroy a database the proxy under test does not use."
        echo "       Set ALLOW_UNVERIFIED_PG_TARGET=1 to override."
        exit 1
    fi
else
    echo "verified: proxy store target '$STORE_HOST' db '$STORE_DB' matches the drop target $PG_CONTAINER/agglayer_store"
fi

# Shared resolver: the restore one-shot below MUST run under the same custody
# overlay as the stack it is repairing, plus any site overlay (EXTRA_COMPOSE_FILES).
. "$PROJECT_DIR/scripts/lib-compose.sh"
compose_env_load
mapfile -t COMPOSE < <(compose_files)

L1_RPC="${L1_RPC:-http://localhost:8545}"
L1_BRIDGE_ADDRESS="${L1_BRIDGE_ADDRESS:-0xC8cbEBf950B9Df44d987c8619f092beA980fF038}"
BRIDGE_SERVICE_URL="${BRIDGE_SERVICE_URL:-http://localhost:18080}"
SIGNER_KEY="${SIGNER_KEY:-0x12d7de8621a77640c9241b2595ba78ce443d05e94090365ab3bb5e19df82c625}"
DEPOSIT_WEI="${DEPOSIT_WEI:-10000000000000}"
RUN_SUFFIX="$(date +%s)"
EVIDENCE="/tmp/full-db-loss-recovery-${RUN_SUFFIX}.txt"

ts()   { date +%H:%M:%S; }
say()  { printf '[%s] %s\n' "$(ts)" "$*" | tee -a "$EVIDENCE"; }
step() { printf '\n[%s] STEP: %s\n' "$(ts)" "$*" | tee -a "$EVIDENCE"; }
fail() { printf '[%s] FAIL: %s\n' "$(ts)" "$*" | tee -a "$EVIDENCE" >&2; exit 1; }
pass() { printf '[%s] PASS: %s\n' "$(ts)" "$*" | tee -a "$EVIDENCE"; }

pgq() { docker exec "$PG_CONTAINER" psql -U agglayer -d agglayer_store -tAc "$1"; }

# PR#164 blocker #7 — COUNT comparison is unsafe: losing one row and
# reconstructing a different row passes with the same count. `fingerprint`
# digests the ORDERED identities/content of each row set (so a swapped row
# changes the digest), and `counts` stays for human logging + the thinness gate.
#   - synthetic_logs digest: md5 over (tx_hash:log_index:data) ordered
#   - injected-GER set digest: md5 over ger_hash ordered
#   - hash_chain_value: the order-sensitive rolling-chain assertion (kept as-is)
# FULL normalized log-row fingerprint (PR#164 re-review). The previous digest
# covered only (transaction_hash, log_index, data), so a regression in block
# identity, emitting address, TOPICS (i.e. the indexed event fields — the very
# thing an "indexed event regression" would corrupt), or the `removed` flag
# reproduced a different row while the digest matched. Fingerprint every column
# that a faithful restore must reproduce.
#
# `topics` is a BYTEA[]; array_to_string over its hex encoding gives a stable,
# order-preserving rendering (topic order is semantic: topic0 is the event
# signature, topics 1..n the indexed args).
# The stored `log_index` column is the proxy's INTERNAL emission counter, and within a
# block the interleave of the GER-writer and projector paths is a wall-clock race — so
# internal counters can legitimately differ across a faithful restore. The SERVED
# `logIndex` is now a canonical per-block rank (Ethereum semantics; see
# `log_synthesis::assign_canonical_block_indices`), asserted by the eth_getLogs
# comparison below. SQL digests therefore compare CONTENT ONLY, ordered by content.
log_digest() { # $1 = topic0 hex prefix
    pgq "SELECT md5(coalesce(string_agg(
           transaction_hash || ':' || block_number || ':' ||
           encode(block_hash,'hex') || ':' || address || ':' ||
           array_to_string(topics, ',') || ':' ||
           transaction_index::text || ':' || removed::text || ':' || data,
           '|' ORDER BY block_number, array_to_string(topics, ','), data), '')) \
         FROM synthetic_logs WHERE topics[1] LIKE '$1%' $(wb)"
}
# GER / UpdateHashChain content digest — deliberately EXCLUDES transaction_hash
# and log_index, because neither can survive a full DB loss even when the
# restored history is perfectly faithful:
#
#   * transaction_hash — the injection is carried by an eth-tx aggoracle submits
#     to THIS proxy's own L2 RPC (`eth_sendRawTransaction` -> `insert_ger`). Its
#     hash is derived from the signed raw tx and is recorded ONLY in the proxy's
#     `transactions` / `tx_note_links` rows, which this drill drops. It was never
#     on L1 and is not carried by the Miden note, so there is no source left to
#     recover it from; restore falls back to a deterministic derived hash.
#   * log_index — a SINGLE global counter shared by Bridge/Claim/UHC emissions.
#     Restore replays by phase (all B2AGG, then CLAIM, then GER) rather than in
#     the original interleaved arrival order, so the indices are re-numbered even
#     when every event is faithfully reproduced.
#
# What MUST be identical is the part that carries meaning: each injected GER's
# VALUE, its per-block attribution, its ORDER (asserted separately and exactly by
# the rolling `hash_chain_value`) and its count. Those are asserted strictly.
# Bridge/Claim keep the FULL digest (tx identity included) — theirs is
# reconstructed from node-authoritative note bodies, so it must survive.
# Same FULL-row rigor as `log_digest` (address, block identity, topics, removal
# state all included, so an indexed-event regression cannot slip through) minus
# exactly the two unrecoverable fields documented above.
uhc_content_digest() {
    # ORDER BY must discriminate SAME-BLOCK rows or string_agg order is
    # UNSPECIFIED and the digest is nondeterministic across runs. UHC `data` is
    # always '0x' (everything lives in topics), so ordering by (block, data)
    # produced a FALSE-POSITIVE "content differs" with EQUAL counts (loop cycle
    # 5, 2026-08-13: getLogs byte-identical, digest mismatch). Order by topics —
    # the ger+chain values — which is total for UHC rows.
    pgq "SELECT md5(coalesce(string_agg(
           block_number || ':' || encode(block_hash,'hex') || ':' || address || ':' ||
           array_to_string(topics, ',') || ':' || removed::text || ':' || data,
           '|' ORDER BY block_number, array_to_string(topics, ',')), '')) \
         FROM synthetic_logs WHERE topics[1] LIKE '0x65d3bf36%' $(wb)"
}

# Row-level dump for diagnosis. A digest tells you THAT something differs; this
# tells you WHICH FIELD. Written before the drop and after the restore so a
# mismatch prints an exact per-field diff instead of two opaque md5s.
dump_rows() { # $1 = topic0 prefix, $2 = output file
    # No internal log_index: it is not served and can differ across a faithful
    # restore (see log_digest's comment). Content-ordered so the pre/post diff
    # aligns row-for-row.
    pgq "SELECT 'blk=' || block_number || ' | txh=' || transaction_hash
         || ' | txi=' || transaction_index || ' | rm=' || removed
         || ' | bh=' || encode(block_hash,'hex') || ' | addr=' || address
         || ' | topics=' || array_to_string(topics, ',') || ' | data=' || data
         FROM synthetic_logs WHERE topics[1] LIKE '$1%'
         ORDER BY block_number, array_to_string(topics, ','), data" > "$2" 2>/dev/null
}

# ClaimEvent digest — every field EXCEPT transaction_hash.
#
# A claim's tx_hash has TWO possible values, and a full DB loss forces the second:
#
#   observed_tx_hash (note_handoff)  -> real eth-tx hash   [table DROPPED by this drill]
#   get_tx_for_note  (tx_note_links) -> real eth-tx hash   [table DROPPED by this drill]
#   otherwise                        -> derive_manual_claim_tx_hash(note_commitment)
#
# Both real-hash sources live ONLY in the proxy's Postgres. A claim that rode a real
# eth-tx (the `publish_claim` path) therefore CANNOT keep that hash across a full DB
# loss: it was never on L1 and the Miden note does not carry it. Restore falls back to
# the derived hash, which is a deterministic keccak over the note commitment.
#
# So the rule is: a claim's tx_hash is rewritten real->derived on its FIRST restore and
# is a bit-exact no-op on EVERY restore thereafter. The number of rows that move equals
# the number of claims that rode a real eth-tx since the previous restore — which for a
# first restore of a production store is ALL of them, not an edge case.
#
# Verified 2026-08-12 by recomputing keccak(tag||note_commitment) with `cast` for all 17
# restored claims: every one matched the derived hash, and the 16 that compared equal
# pre/post were already derived (converged by an earlier restore of this long-lived
# stack). Only the claim created live since the last restore moved. Every other field —
# log_index (51 == 51), block_number, block_hash, address, topics, data,
# transaction_index, removed — was byte-identical.
#
# This is not data loss: #136/#67 guarantee the full claimAsset calldata is servable
# under WHICHEVER hash carries the event, which is why aggkit keeps settling across the
# rewrite.
#
# BridgeEvent deliberately KEEPS transaction_hash: its hash is commitment-derived on BOTH
# paths (never real-eth-tx-linked), so it is stable by construction and verified identical
# on the same run — nothing is loosened where identity is actually provable.
claim_content_digest() {
    pgq "SELECT md5(coalesce(string_agg(
           block_number || ':' || encode(block_hash,'hex') || ':' ||
           address || ':' || array_to_string(topics, ',') || ':' ||
           transaction_index::text || ':' || removed::text || ':' || data,
           '|' ORDER BY block_number, data), '')) \
         FROM synthetic_logs WHERE topics[1] LIKE '0x1df3f2a9%' $(wb)"
}
# CONSUMER-LEVEL capture: what an actual client sees via eth_getLogs, not what our
# own SQL says. The SQL digests above read the table we wrote; this reads the JSON-RPC
# surface aggkit/bridge-service actually consume, so a serialisation-level regression
# (field omitted, hex padding, ordering) cannot hide behind a matching row digest.
#
# Emits one TSV line per log, ordered by (blockNumber, logIndex) NUMERICALLY — hex
# strings of differing width do not sort lexically.
PROXY_RPC="${PROXY_RPC:-http://localhost:8546}"
# The proxy enforces a max eth_getLogs range (10000 blocks) as a DoS guard and returns
# -32602 "block range too large, paginate" beyond it, so the capture PAGINATES — which
# also exercises that guard on the real serving path. Window kept under the limit.
GETLOGS_WINDOW="${GETLOGS_WINDOW:-9000}"
rpc() { curl -s -m 120 -X POST "$PROXY_RPC" -H 'content-type: application/json' -d "$1"; }
getlogs_dump() { # $1 = output file
    local tip from to raw chunk err
    tip=$(rpc '{"jsonrpc":"2.0","id":1,"method":"eth_blockNumber","params":[]}' | jq -r '.result // empty')
    [[ -n "$tip" ]] || { say "getlogs: eth_blockNumber returned nothing"; return 1; }
    tip=$((tip)); raw="${1}.raw"; : > "$raw"
    for (( from=0; from<=tip; from+=GETLOGS_WINDOW )); do
        to=$(( from + GETLOGS_WINDOW - 1 )); (( to > tip )) && to=$tip
        chunk=$(rpc "$(printf '{"jsonrpc":"2.0","id":1,"method":"eth_getLogs","params":[{"fromBlock":"0x%x","toBlock":"0x%x"}]}' "$from" "$to")")
        err=$(printf '%s' "$chunk" | jq -r '.error.message // empty')
        [[ -n "$err" ]] && { say "getlogs: window $from-$to failed: $err"; return 1; }
        printf '%s' "$chunk" | jq -r '
            def h2d: ltrimstr("0x") | explode
                     | reduce .[] as $c (0; . * 16 + (if $c >= 48 and $c <= 57 then $c - 48
                                                      elif $c >= 97 then $c - 87 else $c - 55 end));
            .result[]
            | [ (.blockNumber|h2d|tostring), (.logIndex|h2d|tostring), .address,
                (.topics|join(",")), .data, .transactionIndex, (.removed|tostring),
                .transactionHash ] | @tsv' >> "$raw" || return 1
    done
    # DO NOT SORT THE SUBJECT UNDER TEST. The served (block, logIndex) order IS
    # the contract aggkit consumes; sorting the capture before comparing it
    # means a projector that returns the right entries in the WRONG order
    # compares equal on both sides and the drill certifies the one property it
    # exists to protect. Instead: assert the served stream is ALREADY ordered
    # (that is the real assertion), then compare it as served.
    #
    # Pagination appends windows in ascending block order, so a correctly
    # ordered feed yields an already-sorted concatenation.
    if ! sort -c -t"$(printf '\t')" -k1,1n -k2,2n "$raw" 2>/dev/null; then
        say "getlogs: SERVED ORDER VIOLATION — the feed is not ascending by (block, logIndex)."
        say "         First out-of-order boundary:"
        sort -c -t"$(printf '\t')" -k1,1n -k2,2n "$raw" 2>&1 | head -3 | sed 's/^/         /'
        return 1
    fi
    cp "$raw" "$1"
    [[ -s "$1" ]]
}

fingerprint() {  # -> "uhc_d inj_d bridge_d claim_d hcv"  (digests, for identity assertion)
    local uhc inj bridge claim hcv
    uhc=$(uhc_content_digest)
    inj=$(pgq "SELECT md5(coalesce(string_agg(encode(g.ger_hash,'hex'), '|' ORDER BY g.ger_hash), '')) \
               FROM ger_entries g WHERE g.is_injected=true $(wb_ger)")
    bridge=$(log_digest '0x50178120')
    claim=$(claim_content_digest)
    hcv=$(pgq "SELECT encode(hash_chain_value,'hex') FROM service_state WHERE id=1")
    echo "$uhc $inj $bridge $claim $hcv"
}
# WINDOW BOUND (see SNAP_BLOCK below): every comparison is restricted to blocks
# the projector had already covered when the pre-drop fingerprint was taken.
# Without it the drill compares a snapshot of a MOVING system against a restore
# that reads the authoritative node LATER: GER injection runs on its own timer,
# so a GER consumed after the snapshot legitimately appears in the rebuilt store
# and the AFTER==BEFORE assertions fail on a PERFECTLY FAITHFUL restore.
# Observed 2026-09-04: UHC and injected both 3 -> 4, Bridge/Claim unchanged,
# eth_getLogs byte-identical — a live injection 13s after the snapshot.
# Loss or duplication INSIDE the window still fails exactly as before.
wb() { # window predicate for synthetic_logs ONLY — MIDEN block space
    [[ -n "${SNAP_BLOCK:-}" ]] && echo "AND block_number <= $SNAP_BLOCK" || echo ""
}
# ── The GER window is a DIFFERENT BLOCK SPACE ────────────────────────────────
# `SNAP_BLOCK` is the projector cursor: a MIDEN synthetic block height (~100).
# `ger_entries.block_number` is the L1 block the GER was observed at (`INSERT
# INTO ger_entries (... block_number ...)` is fed `l1_block_number`) and runs in
# the hundreds-to-thousands on anvil. Applying `wb()` to ger_entries therefore
# compared two unrelated counters and silently shrank the injected-GER set:
# measured on this stack at SNAP_BLOCK=105, the L1-space bound selected 1 of the
# 4 injected GERs the window actually contains, so the "#88: injected-GER set
# differs across restore" digest was computed over a near-empty set and could
# not have failed. Not a product dedup bug — the four GER VALUES are distinct
# and each has exactly one UpdateHashChain log; the harness was measuring the
# wrong thing. (It also explains why the pre-window run recorded UHC=3
# injected=3 and the windowed runs recorded UHC=4 injected=1.)
#
# The correct bound is by the GER's OWN UpdateHashChain log, which lives in the
# Miden block space: a GER is inside the window iff its UHC log is. topics[2] of
# a UHC log is the GER value; `synthetic_logs.topics` is text[] holding
# lowercase `0x…`, so it compares against `'0x' || encode(ger_hash,'hex')`.
# An injected GER with NO UHC log is excluded here and caught by the separate
# UHC count/content assertions, which is where that defect belongs.
wb_ger() { # window predicate for ger_entries (alias `g`) — via the Miden-space UHC log
    [[ -n "${SNAP_BLOCK:-}" ]] || { echo ""; return; }
    echo "AND EXISTS (SELECT 1 FROM synthetic_logs l \
                      WHERE l.topics[1] LIKE '0x65d3bf36%' \
                        AND lower(l.topics[2]) = '0x' || encode(g.ger_hash,'hex') \
                        AND l.block_number <= $SNAP_BLOCK)"
}
counts() {  # -> "uhc inj bridge claim"  (integers, for logging + thinness gate)
    local uhc inj bridge claim w; w=$(wb)
    uhc=$(pgq "SELECT count(*) FROM synthetic_logs WHERE topics[1] LIKE '0x65d3bf36%' $w")
    inj=$(pgq "SELECT count(*) FROM ger_entries g WHERE g.is_injected=true $(wb_ger)")
    bridge=$(pgq "SELECT count(*) FROM synthetic_logs WHERE topics[1] LIKE '0x50178120%' $w")
    claim=$(pgq "SELECT count(*) FROM synthetic_logs WHERE topics[1] LIKE '0x1df3f2a9%' $w")
    echo "$uhc $inj $bridge $claim"
}

wait_healthy() {
    local deadline=$((SECONDS + ${1:-180}))
    while (( SECONDS < deadline )); do
        [[ "$(docker inspect -f '{{.State.Health.Status}}' "$PROXY_CONTAINER" 2>/dev/null)" == healthy ]] && return 0
        sleep 3
    done
    return 1
}

# ── Phase 0: pre-drop fingerprint on the live, quiesced stack ────────────────
step "Phase 0 — pre-drop fingerprint (accumulated state is the fixture)"
# The projector cursor is the exact frontier of what this store has projected;
# blocks beyond it are precisely what a still-running pipeline may add while the
# drill works. Bound every later comparison to it.
# QUIESCE FIRST, then bound. Quiescing alone is not enough (a note can land
# between the last sample and the drop) and bounding alone is not enough (the
# pre-drop store may not yet have projected everything at that height), so the
# drill needs both: wait for the projector to catch up and the writer to drain,
# then compare pre vs post at exactly that projected height.
. "$PROJECT_DIR/scripts/lib-quiesce.sh"
# 600s, not 180: quiescing is now a NO-PENDING-WORK gate (writer drained, store
# drained, L1 GER injected, log count stable), and after a chaos storm or a 30-way
# load run the pipeline legitimately needs minutes to get there. A ceiling that
# expires while the system is still draining turns host load into a fake product
# failure. On a quiet stack this costs nothing — it quiesces in ~10s.
quiesce_projection "${QUIESCE_TIMEOUT_SECS:-600}" \
    || fail "projection never quiesced — refusing to fingerprint a moving pipeline"

# BELT AND BRACES. Quiescing proves nothing is PENDING; it cannot stop the
# aggoracle from starting something NEW one second later. With the pipeline
# proven drained, freeze the source of unsolicited work for the whole
# fingerprint window (Phase 0 capture through the Phase 3 comparison) so the
# before/after pair provably covers the same history.
#
# ORDER MATTERS: quiesce FIRST, freeze SECOND. Freezing before quiescing would
# strand any GER the aggoracle had accepted-but-not-yet-submitted, and
# condition (c) — "L1's current root is already injected" — could then never
# become true.
FROZE_AGGKIT=0
unfreeze_aggkit() {
    [[ "$FROZE_AGGKIT" == "1" ]] || return 0
    FROZE_AGGKIT=0
    docker start "$AGGKIT_CONTAINER" >/dev/null 2>&1 \
        && say "aggkit restarted" \
        || echo "WARN: could not restart $AGGKIT_CONTAINER — the stack is left with aggkit DOWN" >&2
}
# Fires on fail()/set -e too: leaving another test's stack with aggkit stopped
# would poison every scenario that runs after this one.
trap unfreeze_aggkit EXIT
if [[ "${FREEZE_AGGKIT:-1}" == "1" ]] && docker inspect "$AGGKIT_CONTAINER" >/dev/null 2>&1; then
    docker stop "$AGGKIT_CONTAINER" >/dev/null && FROZE_AGGKIT=1 \
        && say "aggkit frozen for the fingerprint window ($AGGKIT_CONTAINER)"
    # Re-confirm on the frozen stack. Short: everything already held once, this
    # only proves the freeze itself did not leave something half-done.
    quiesce_projection 90 2 \
        || fail "projection did not re-quiesce after freezing aggkit"
else
    say "aggkit NOT frozen (FREEZE_AGGKIT=${FREEZE_AGGKIT:-1}, container present: \
$(docker inspect "$AGGKIT_CONTAINER" >/dev/null 2>&1 && echo yes || echo no))"
fi

SNAP_BLOCK=$(projected_height)
[[ -n "$SNAP_BLOCK" && "$SNAP_BLOCK" =~ ^[0-9]+$ ]] || fail "could not read projector_cursor for the comparison window"
say "comparison window: blocks <= $SNAP_BLOCK (quiesced projector cursor)"
read -r NUHC0 NINJECTED0 NBR0 NCL0 <<<"$(counts)"
read -r UHC0 INJ0 BR0 CL0 HCV0 <<<"$(fingerprint)"
dump_rows '0x50178120' "/tmp/fdl-bridge-before-${RUN_SUFFIX}.txt"
dump_rows '0x1df3f2a9' "/tmp/fdl-claim-before-${RUN_SUFFIX}.txt"
dump_rows '0x65d3bf36' "/tmp/fdl-uhc-before-${RUN_SUFFIX}.txt"
if getlogs_dump "/tmp/fdl-getlogs-before-${RUN_SUFFIX}.tsv"; then
    say "before: eth_getLogs captured ($(wc -l < "/tmp/fdl-getlogs-before-${RUN_SUFFIX}.tsv") logs) from $PROXY_RPC"
else
    fail "eth_getLogs baseline capture FAILED against $PROXY_RPC — refusing to run a drill \
whose consumer-level comparison cannot be made (set PROXY_RPC)"
fi
say "before: counts UHC=$NUHC0 injected=$NINJECTED0 Bridge=$NBR0 Claim=$NCL0  hash_chain=${HCV0:0:16}…"
say "before: digests uhc=${UHC0:0:12} inj=${INJ0:0:12} bridge=${BR0:0:12} claim=${CL0:0:12}"
# Review 0814 (blocking): all FOUR event families must be present in the
# fixture, or the Bridge/Claim equality assertions later pass VACUOUSLY and the
# advertised full event-fidelity recovery is green without testing either path.
[[ "$NUHC0" -ge 1 && "$NINJECTED0" -ge 1 && "$NBR0" -ge 1 && "$NCL0" -ge 1 ]] \
    || fail "fixture too thin (UHC=$NUHC0 inj=$NINJECTED0 Bridge=$NBR0 Claim=$NCL0) — every family must be >=1; run traffic first"

# BASELINE PROVENANCE — the drill is only meaningful if the "before" state was built
# LIVE. If this store is itself the output of an earlier restore, every value has
# already converged to its post-restore form (claim tx_hashes rewritten real->derived,
# ordering already replay-ordered) and the comparison degenerates into "restore is
# idempotent" — necessary, but NOT the fidelity claim this drill exists to make.
# `nonce_ledger_rebuilt` is written ONLY by restore (finalize_restore_cursors), so it is
# an exact tripwire. Caught live 2026-08-12: a P4 diagnostic had been re-run on an
# already-restored stack and its green verdict silently meant far less than it read.
BASELINE_RESTORED=$(pgq "SELECT coalesce(bool_or(nonce_ledger_rebuilt), false) FROM service_state")
if [[ "$BASELINE_RESTORED" == "t" ]]; then
    if [[ "${ALLOW_RESTORED_BASELINE:-0}" == "1" ]]; then
        say "WARNING: baseline store is RESTORE OUTPUT (nonce_ledger_rebuilt=t). This run \
measures restore-vs-restore IDEMPOTENCE, not live-vs-restore FIDELITY. Proceeding \
because ALLOW_RESTORED_BASELINE=1."
        BASELINE_KIND="restored (idempotence only)"
    else
        fail "baseline store is RESTORE OUTPUT (service_state.nonce_ledger_rebuilt=t) — this \
would compare restore-vs-restore, not live-vs-restore, and would report a misleadingly \
green verdict. Run this drill on a stack whose state was built LIVE (fresh stack + \
traffic), or set ALLOW_RESTORED_BASELINE=1 to explicitly accept an idempotence-only run."
    fi
else
    BASELINE_KIND="live (true fidelity comparison)"
fi
say "baseline provenance: $BASELINE_KIND"
[[ "$NUHC0" == "$NINJECTED0" ]] || say "note: UHC($NUHC0) != injected($NINJECTED0) pre-drop — carrying the delta forward"
NTX_MARK=$(docker logs "$NTX_CONTAINER" 2>&1 | grep -c "1007209807211405110" || true)
say "ntx kernel-assert (poison) lines so far: $NTX_MARK"

# ── Phase 1: FULL loss — drop the postgres store AND the miden-client store ──
step "Phase 1 — full state loss: stop proxy, DROP agglayer_store, wipe client sqlite"
docker compose "${COMPOSE[@]}" --env-file "$ENV_FILE" stop miden-agglayer >/dev/null
docker exec "$PG_CONTAINER" psql -U agglayer -d postgres -c \
    "DROP DATABASE agglayer_store WITH (FORCE);" >/dev/null
docker exec "$PG_CONTAINER" psql -U agglayer -d postgres -c \
    "CREATE DATABASE agglayer_store OWNER agglayer;" >/dev/null
pass "postgres store dropped + recreated empty (migrations run on next boot)"
# The client sqlite is wiped by --reset-miden-store in the one-shot below.

# ── Phase 2: documented operator recovery, faithful to the RUNNING config ────
step "Phase 2 — one-shot: <live proxy command> --reset-miden-store --restore"
# Clone the live container's exact command (signer flags, hardening flags, …)
# so the restore runs under the same custody/config as the service.
mapfile -t LIVE_CMD < <(docker inspect "$PROXY_CONTAINER" --format '{{range .Config.Cmd}}{{println .}}{{end}}' | sed '/^$/d')
RESTORE_LOG="/tmp/full-db-loss-restore-${RUN_SUFFIX}.log"
set +e
docker compose "${COMPOSE[@]}" --env-file "$ENV_FILE" \
    run --rm --no-deps miden-agglayer \
    "${LIVE_CMD[@]}" --reset-miden-store --restore \
    >"$RESTORE_LOG" 2>&1
RESTORE_EXIT=$?
set -e
say "restore one-shot exit=$RESTORE_EXIT (log: $RESTORE_LOG)"
if [[ "$RESTORE_EXIT" -ne 0 ]]; then
    # Review 0814 (blocking): a failed one-shot must NOT restart the serving
    # proxy on a partial store. Live discovery/cursors are not yet the restore
    # pipeline (#167), `/health` does not prove projector/reconcile catch-up,
    # so a restarted proxy can SERVE incomplete history — and because
    # `nonce_ledger_rebuilt` was never finalized, the next drill would mislabel
    # the reconstructed store as a fresh live baseline. Leave the proxy STOPPED
    # and fail with the repair path.
    tail -20 "$RESTORE_LOG" >&2 || true
    echo "REPAIR: the proxy is left STOPPED on a partial store (serving it would expose" >&2
    echo "        incomplete history). Fix the cause shown above, then re-run the one-shot:" >&2
    echo "          docker compose <files> run --rm --no-deps miden-agglayer <live-cmd> \\" >&2
    echo "              --reset-miden-store --restore" >&2
    echo "        (idempotent; safe to repeat). Start the proxy ONLY after it exits 0.
        THEN re-base every tx-holder that survived the restore, or the pipeline
        stays frozen even though the proxy is healthy (#90 proxy ledger is done
        by the restore itself; the other two are NOT):
          scripts/bridge-claimtxman-heal.sh          # bridge-service claimtxman (#111)
          scripts/aggkit-preserve-heal.sh aggkit     # aggoracle ethtxmanager (#113)" >&2
    fail "reset+restore one-shot exited $RESTORE_EXIT (proxy left stopped; repair + re-run --restore)"
fi
[[ "$RESTORE_EXIT" -eq 0 ]] || { tail -15 "$RESTORE_LOG" | tee -a "$EVIDENCE"; fail "restore one-shot failed"; }
grep -q "reset_miden_store" "$RESTORE_LOG" || fail "reset_miden_store marker missing — client store was NOT wiped"
GER_RESTORED=$(grep -c "rebuilt GER from consumed UpdateGerNote" "$RESTORE_LOG" || true)
say "restore log: rebuilt $GER_RESTORED GER(s) from consumed UpdateGerNotes"

# ── Phase 3: normal start, then fidelity assertions ──────────────────────────
step "Phase 3 — start proxy, assert restored history is IDENTICAL"
docker compose "${COMPOSE[@]}" --env-file "$ENV_FILE" start miden-agglayer >/dev/null
wait_healthy 180 || fail "proxy not healthy within 180s after restore"
pass "proxy healthy"
sleep 10   # let the reconcile catch-up settle anything the one-shot left


# Print the exact differing rows for a kind, then fail with the digest pair.
fail_with_diff() { # $1=label $2=before-file $3=after-file $4=digest-before $5=digest-after
    echo "--- $1: per-field diff (before -> after) ---" | tee -a "$EVIDENCE"
    diff -u "$2" "$3" 2>/dev/null | head -40 | tee -a "$EVIDENCE" || true
    echo "--- end $1 diff ---" | tee -a "$EVIDENCE"
    fail "$1 rows differ (digest $4 -> $5)"
}

read -r NUHC1 NINJECTED1 NBR1 NCL1 <<<"$(counts)"
read -r UHC1 INJ1 BR1 CL1 HCV1 <<<"$(fingerprint)"
dump_rows '0x50178120' "/tmp/fdl-bridge-after-${RUN_SUFFIX}.txt"
dump_rows '0x1df3f2a9' "/tmp/fdl-claim-after-${RUN_SUFFIX}.txt"
dump_rows '0x65d3bf36' "/tmp/fdl-uhc-after-${RUN_SUFFIX}.txt"

# ── Consumer-level verdict: eth_getLogs before vs after ─────────────────────
# Captured BEFORE the post-restore liveness injection, so both sides cover the same
# history. Compared twice: whole-record, then with transactionHash blanked. If the
# whole-record diff is non-empty but the blanked diff is EMPTY, the ONLY thing that
# moved is transaction_hash — the one field a full DB loss provably cannot preserve
# (see claim_content_digest above). Anything else differing is a real regression.
GL_BEFORE="/tmp/fdl-getlogs-before-${RUN_SUFFIX}.tsv"
GL_AFTER="/tmp/fdl-getlogs-after-${RUN_SUFFIX}.tsv"
if ! getlogs_dump "$GL_AFTER"; then
    fail "eth_getLogs post-restore capture FAILED against $PROXY_RPC — cannot render a \
consumer-level verdict"
fi
NGL0=$(wc -l < "$GL_BEFORE"); NGL1=$(wc -l < "$GL_AFTER")
say "eth_getLogs: before=$NGL0 logs after=$NGL1 logs"
(( NGL1 >= NGL0 )) || fail "eth_getLogs LOST logs across restore: $NGL0 -> $NGL1 (consumer-visible history shrank)"
# Blank field 8 (transactionHash) on both sides.
blank_txh() { awk -F'\t' 'BEGIN{OFS="\t"} {$8="<txh>"; print}' "$1"; }
blank_txh "$GL_BEFORE" > "${GL_BEFORE}.notxh"; blank_txh "$GL_AFTER" > "${GL_AFTER}.notxh"
# Compare only the shared prefix of history (after >= before; extra tail is new traffic).
head -n "$NGL0" "$GL_AFTER" > "${GL_AFTER}.trunc"
head -n "$NGL0" "${GL_AFTER}.notxh" > "${GL_AFTER}.notxh.trunc"
GL_FULL_DIFF=$(diff -u "$GL_BEFORE" "${GL_AFTER}.trunc" | grep -c '^[+-][^+-]' || true)
GL_NOTXH_DIFF=$(diff -u "${GL_BEFORE}.notxh" "${GL_AFTER}.notxh.trunc" | grep -c '^[+-][^+-]' || true)
if (( GL_NOTXH_DIFF == 0 )); then
    if (( GL_FULL_DIFF == 0 )); then
        say "PASS: eth_getLogs BIT-IDENTICAL across restore ($NGL0 logs, transaction_hash included)"
    else
        NTXH=$(( GL_FULL_DIFF / 2 ))
        say "PASS: eth_getLogs identical across restore EXCEPT transaction_hash on ${NTXH} log(s) \
— every other field (block, log_index, address, topics, data, tx_index, removed) byte-identical"
    fi
else
    say "eth_getLogs DIFFERS in fields other than transaction_hash — first 40 diff lines:"
    diff -u "${GL_BEFORE}.notxh" "${GL_AFTER}.notxh.trunc" | head -40 || true
    fail "CONSUMER-LEVEL REGRESSION: eth_getLogs differs beyond transaction_hash \
($GL_NOTXH_DIFF differing lines) — baseline=$BASELINE_KIND"
fi
say "after : counts UHC=$NUHC1 injected=$NINJECTED1 Bridge=$NBR1 Claim=$NCL1  hash_chain=${HCV1:0:16}…"
# Ordered-content digests (blocker #7): a swapped/reconstructed row changes the
# digest even when the count is identical.
[[ "$NUHC1" == "$NUHC0" ]] || fail "#88: UpdateHashChain log COUNT changed across restore ($NUHC0 -> $NUHC1) — GER history was lost or duplicated"
[[ "$UHC1" == "$UHC0" ]] || fail "#88: UpdateHashChain CONTENT differs across restore (per-block GER values; digest $UHC0 -> $UHC1; counts $NUHC0 -> $NUHC1)"
pass "UpdateHashChain content identical — same GER values at the same blocks (count $NUHC1, digest ${UHC1:0:12}); tx_hash/log_index intentionally excluded (unrecoverable after full DB loss — see log_digest notes), with ORDER asserted exactly by hash_chain_value below"
[[ "$INJ1" == "$INJ0" ]] || fail "#88: injected-GER set differs across restore (digest $INJ0 -> $INJ1; counts $NINJECTED0 -> $NINJECTED1)"
pass "injected-GER set identical (count $NINJECTED1, digest ${INJ1:0:12})"
[[ "$HCV1" == "$HCV0" ]] || fail "#88: hash_chain_value diverged (order-sensitive replay broke): $HCV0 vs $HCV1"
pass "hash_chain_value identical (order-faithful replay)"
[[ "$BR1" == "$BR0" ]] || fail_with_diff "#69/#136 BridgeEvent" \
    "/tmp/fdl-bridge-before-${RUN_SUFFIX}.txt" "/tmp/fdl-bridge-after-${RUN_SUFFIX}.txt" "$BR0" "$BR1"
[[ "$CL1" == "$CL0" ]] || fail_with_diff "#69/#136 ClaimEvent (all fields except tx_hash)" \
    "/tmp/fdl-claim-before-${RUN_SUFFIX}.txt" "/tmp/fdl-claim-after-${RUN_SUFFIX}.txt" "$CL0" "$CL1"
pass "BridgeEvent (count $NBR1) + ClaimEvent (count $NCL1) rows identical"

# The fingerprint window is closed: every before/after comparison is made.
# Phase 4 needs the aggoracle running again (it asserts a NEW GER gets injected).
unfreeze_aggkit

# ── Phase 4: no poison minted, pipeline alive ────────────────────────────────
step "Phase 4 — no ERR_GER_ALREADY_REGISTERED poison; pipeline processes NEW traffic"

# FINDING #111 (loop cycles 1-3, 2026-08-18): the restore COMPACTS the
# ClaimTxManager sponsor's chain nonce (live ~177 -> restored 66 observed), but
# the bridge-service claimtxman allocates nonces from its own monitored-tx
# history — every post-restore claim it creates is R4-rejected forever and the
# retry storm starves the shared synchronizer loop, so the L1-GER index falls
# behind unboundedly and /merkle-proof 500s (code=2 l1GER-not-found) for every
# net-1 deposit. Same class as the aggkit ethtxmanager wipe: any component
# holding nonces across a restore must be re-based. FORCE=1 — post-restore the
# stranded state is deterministic, no detection needed.
# The heal is RELEASE-REQUIRED, so its failure is the drill's failure. It used
# to be advisory (missing script → skipped, non-zero → a WARN), which meant the
# drill could print FULL DB LOSS RECOVERY while claimtxman was still
# nonce-wedged and every post-restore claim was being R4-rejected. `pipefail` is
# set, but `| tee` would otherwise mask the exit status, so capture it directly.
[[ -x "$SCRIPT_DIR/bridge-claimtxman-heal.sh" ]] \
    || fail "bridge-claimtxman-heal.sh is missing/not executable — the #111 heal is required after a full DB loss"
# rc 3 = "wipe done, service running, but no local proof was available" — the
# expected outcome on a quiet post-restore stack, where sync.block records only
# event-bearing blocks so the cursor legitimately does not move. This drill
# proves the claim pipeline itself further down (the exact deposit reaching
# ready_for_claim), which is what makes accepting 3 honest here rather than a
# shrug. Any OTHER non-zero rc is a real failure.
#
# `set -e` is active, so a bare invocation would exit before $? is read.
if PROJECT="$COMPOSE_PROJECT_NAME" FORCE=1 L1_RPC="$L1_RPC" \
        "$SCRIPT_DIR/bridge-claimtxman-heal.sh" >>"$EVIDENCE" 2>&1; then
    CTM_RC=0
else
    CTM_RC=$?
fi
case "$CTM_RC" in
    0) say "claimtxman heal confirmed L1 sync progress" ;;
    3) say "claimtxman heal completed the wipe; no local proof available (quiet stack) — this drill proves the claim pipeline below" ;;
    *)
        tail -20 "$EVIDENCE" || true
        fail "claimtxman heal FAILED (#111, rc=$CTM_RC) — post-restore claims would be R4-rejected and the L1-GER index starved"
        ;;
esac

# FINDING #113 (drill-caught 2026-08-19, RELEASE-REQUIRED): the restore heals
# the proxy nonce ledger (#90) and the bridge-service claimtxman (#111) — but
# NOT aggkit's aggoracle ethtxmanager. Any GER-inject tx in flight when the
# store is dropped is lost in transit: aggkit's monitoring DB still holds it,
# its deterministic-ID dedup refuses to re-send, and GER INJECTION FREEZES
# PERMANENTLY (observed: proxy received ZERO txs for 5+ min, aggoracle looping
# "already exists in monitoring DB" on a tx the proxy had never seen; Phase 4
# then fails with "no NEW GER injected+consumed"). Healed live by wiping ONLY
# ethtxmanager-aggoracle.sqlite, after which injections resumed immediately.
# Every tx-holder that survives a restore must be re-based — this is the third.
[[ -x "$SCRIPT_DIR/aggkit-preserve-heal.sh" ]] \
    || fail "aggkit-preserve-heal.sh is missing/not executable — the #113 heal is required after a full DB loss"
# HEAL_ALLOW_DEFERRED_PROOF=1: on a quiet stack there may be no pending
# injection for the heal to await, and this drill proves the injection pipeline
# itself a few lines below (the NINJECTED1 -> INJ2 gate). The heal must still
# fail on every NEGATIVE signal — crash loop, re-wedge, restore mismatch.
# `set -e` is active: a bare invocation that exits 3 aborts the WHOLE script
# before `$?` is ever read, so the exit-3 contract below would never run — the
# same shape as the psql subshell bug. `if ...; then ... else HEAL_RC=$?; fi`
# is the form that survives it.
if PROJECT="$COMPOSE_PROJECT_NAME" FORCE=1 HEAL_ALLOW_DEFERRED_PROOF=1 \
        "$SCRIPT_DIR/aggkit-preserve-heal.sh" aggkit >>"$EVIDENCE" 2>&1; then
    HEAL_RC=0
else
    HEAL_RC=$?
fi
# rc 3 = "restored and running, but the healer observed no injection to prove
# the pipeline with". That is the expected outcome on a quiet stack and is why
# this drill passes HEAL_ALLOW_DEFERRED_PROOF=1 — the NINJECTED1 -> INJ2 gate
# below is the proof. Any OTHER non-zero rc is a real heal failure.
case "$HEAL_RC" in
    0) say "aggkit aggoracle heal confirmed an injection" ;;
    # "did not confirm" — NOT "confirmed no". Failing to observe an injection is
    # not evidence that none occurred, and exit 3 also covers the path where
    # NO target was extracted and no wait ran at all, so "watches one target"
    # would be wrong there too.
    3) say "aggkit aggoracle heal restored the service but did not confirm an exact injection (it checks at most one extracted target; it does not observe all injections) — this drill proves the pipeline itself below" ;;
    *)
        tail -20 "$EVIDENCE" || true
        fail "aggkit aggoracle heal FAILED (#113, rc=$HEAL_RC) — GER injection would stay frozen"
        ;;
esac
sleep 45   # window for aggoracle to (wrongly) re-inject + ntx to (wrongly) assert
NTX_NOW=$(docker logs "$NTX_CONTAINER" 2>&1 | grep -c "1007209807211405110" || true)
[[ "$NTX_NOW" -le "$NTX_MARK" ]] \
    || fail "#86: NEW poison-note kernel asserts after restore ($NTX_MARK -> $NTX_NOW) — aggoracle re-injected registered GERs"
pass "zero new poison-note kernel asserts ($NTX_NOW)"

CNT=$(cast call "$L1_BRIDGE_ADDRESS" 'depositCount()(uint256)' --rpc-url "$L1_RPC")
# REAL, MAPPABLE destination — finding #103. The old synthetic dest
# ("0x…<run-suffix>") never embedded a valid Miden account id, so every
# liveness claim took the RD-860 unresolvable-destination SHORT-CIRCUIT: the
# proxy accepts the tx and emits a NOTE-LESS ClaimEvent that exists only in
# Postgres — unrecoverable by any restore (Miden holds no trace of it). That
# planted one guaranteed-lost event per drill and was the true cause of every
# "#101" loss (the erased-note theory was wrong). Embed the SERVICE account id
# (always exists, C5 zero-pad-resolvable on any stack) so the claim MINTS to a
# real account and its ClaimEvent rides a real consumed note.
SVC_BECH32=$(docker exec "$PROXY_CONTAINER"     sh -c 'grep "^service" /var/lib/miden-agglayer-service/bridge_accounts.toml'     | grep -o '"[a-z0-9]*"' | tr -d '"')
SVC_HEX=$(python3 - "$SVC_BECH32" <<'PYEOF'
import sys
CHARSET = "qpzry9x8gf2tvdw0s3jn54khce6mua7l"
_, data = sys.argv[1].rsplit("1", 1)
vals = [CHARSET.index(c) for c in data][:-6]
acc = bits = 0; out = []
for v in vals:
    acc = (acc << 5) | v; bits += 5
    while bits >= 8:
        bits -= 8; out.append((acc >> bits) & 0xFF)
print(bytes(out[1:]).hex())  # drop the 1-byte bech32 payload prefix
PYEOF
)
[[ ${#SVC_HEX} -eq 30 ]] || fail "liveness dest: decoded service id is not 15 bytes ('$SVC_HEX')"
DEST="0x00000000${SVC_HEX:0:16}${SVC_HEX:16:14}00"
say "liveness: bridgeAsset cnt=$CNT dest=$DEST (embeds service account $SVC_BECH32)"
# Keep the receipt: depositCount() was read BEFORE the send, so on a stack with
# concurrent traffic (the soak runs loadtests against this same stack) another
# deposit can take that index and leave ours at count+1 — the drill would then
# validate somebody else's deposit. The transaction hash is ours alone.
LIVE_TX=$(cast send --rpc-url "$L1_RPC" --private-key "$SIGNER_KEY" "$L1_BRIDGE_ADDRESS" \
  'bridgeAsset(uint32,address,uint256,address,bool,bytes)' \
  1 "$DEST" "$DEPOSIT_WEI" 0x0000000000000000000000000000000000000000 true 0x \
  --value "$DEPOSIT_WEI" --json | jq -r '.transactionHash')
[[ "$LIVE_TX" =~ ^0x[0-9a-fA-F]{64}$ ]] \
    || fail "liveness bridgeAsset did not return a transaction hash (got '${LIVE_TX:-<none>}')"
say "liveness: bridgeAsset tx=$LIVE_TX (expected deposit_cnt≈$CNT on network 0)"

# Assert on THIS deposit, by its exact index. `$DEST` is derived from the
# service account, so it is the SAME destination in every drill on a
# compounding stack: "any of the latest 25 deposits is ready_for_claim" was
# satisfied by deposits from previous cycles (and by concurrent soak traffic),
# i.e. it could pass with the post-restore pipeline completely dead. `$CNT` was
# read from depositCount() immediately BEFORE the bridgeAsset call, so it is
# this deposit's own deposit_cnt.
# Match on (tx_hash, network_id=0): the transaction identifies OUR deposit
# uniquely and cannot be satisfied by a concurrent one, and pinning the
# origin network stops an equal deposit_cnt on another network from
# answering for it.
deadline=$((SECONDS + 300))
while :; do
    READY=$(curl -sf --max-time 20 "$BRIDGE_SERVICE_URL/bridges/$DEST?limit=100&offset=0" 2>/dev/null \
        | LIVE_TX="$LIVE_TX" python3 -c "
import json, os, sys
want = os.environ['LIVE_TX'].lower()
ds = json.load(sys.stdin).get('deposits', [])
mine = [d for d in ds
        if str(d.get('tx_hash', '')).lower() == want and int(d.get('network_id', -1)) == 0]
if not mine:
    print('absent')
elif len(mine) > 1:
    print('ambiguous')
else:
    print('True' if mine[0].get('ready_for_claim') else 'notready')
" 2>/dev/null || echo err)
    [[ "$READY" == "True" ]] && break
    (( SECONDS >= deadline )) && fail "the post-restore deposit from tx $LIVE_TX (network 0, dest $DEST) never became ready_for_claim in 300s (last state: $READY) — pipeline dead after restore"
    sleep 5
done
DEPOSIT_CNT_SEEN=$(curl -sf --max-time 20 "$BRIDGE_SERVICE_URL/bridges/$DEST?limit=100&offset=0" 2>/dev/null \
    | LIVE_TX="$LIVE_TX" python3 -c "
import json, os, sys
want = os.environ['LIVE_TX'].lower()
ds = json.load(sys.stdin).get('deposits', [])
print(next((d.get('deposit_cnt') for d in ds if str(d.get('tx_hash','')).lower() == want), '?'))
" 2>/dev/null || echo '?')
pass "post-restore deposit from OUR tx $LIVE_TX (deposit_cnt=$DEPOSIT_CNT_SEEN, network 0) is ready_for_claim"

# READY IS NOT CLAIMED. ready_for_claim proves the INDEXER caught up; it says
# nothing about ClaimTxManager actually submitting and settling the claim — and
# claimtxman starvation (#111) is precisely a failure that leaves deposits ready
# forever while no money moves. Both healers above are allowed to end UNPROVEN
# on the promise that this drill supplies the functional proof, so it has to be
# a real one: the claim transaction for OUR deposit must land.
deadline=$((SECONDS + 600))
CLAIM_TX=""
CLAIM_GI=""
while :; do
    # --max-time: without a per-request bound, one accepted-but-stalled request
    # holds the loop open forever and the deadline below is never evaluated —
    # the drill would hang instead of naming the #111 shape. The pinned server
    # sets no response deadline of its own.
    CLAIM_TX=$(curl -sf --max-time 20 "$BRIDGE_SERVICE_URL/bridges/$DEST?limit=100&offset=0" 2>/dev/null \
        | LIVE_TX="$LIVE_TX" python3 -c "
import json, os, sys
want = os.environ['LIVE_TX'].lower()
ds = json.load(sys.stdin).get('deposits', [])
print(next((d.get('claim_tx_hash') or '' for d in ds
            if str(d.get('tx_hash','')).lower() == want and int(d.get('network_id', -1)) == 0), ''))
" 2>/dev/null || echo "")
    [[ "$CLAIM_TX" =~ ^0x[0-9a-fA-F]{64}$ ]] && break
    (( SECONDS >= deadline )) && fail "the post-restore deposit from tx $LIVE_TX became ready_for_claim but was NEVER CLAIMED within 600s — the sponsor (claimtxman) is not settling claims, which is exactly the #111 shape both heals above were allowed to leave unproven"
    sleep 10
done

# A claim_tx_hash alone is NOT settlement. The RD-860 short-circuit accepts a
# claim whose destination cannot be resolved and emits a note-less ClaimEvent —
# the deposit gets a claim tx hash and no asset ever moves. A transient
# mapping-store error can put a perfectly good destination on that path, so the
# marker must be checked rather than assumed absent: the proxy records exactly
# these in `unclaimable_claims`, keyed by global index.
CLAIM_GI=$(curl -sf --max-time 20 "$BRIDGE_SERVICE_URL/bridges/$DEST?limit=100&offset=0" 2>/dev/null \
    | LIVE_TX="$LIVE_TX" python3 -c "
import json, os, sys
want = os.environ['LIVE_TX'].lower()
ds = json.load(sys.stdin).get('deposits', [])
print(next((str(d.get('global_index','')) for d in ds
            if str(d.get('tx_hash','')).lower() == want and int(d.get('network_id', -1)) == 0), ''))
" 2>/dev/null || echo "")
[[ -n "$CLAIM_GI" ]] || fail "could not read the global index of our deposit (tx $LIVE_TX) to verify the claim was not a note-less short-circuit"
# global_index is a U256 and routinely exceeds 64 bits (a Miden-destined
# deposit starts at 2^64). `printf '0x%x'` CLAMPS those to 0xffffffffffffffff
# and still exits 0 — with only a warning on stderr — so the hex form never
# matched the stored value and this gate silently passed every time, which is
# precisely the false "money moved" it exists to prevent. Convert with
# arbitrary precision, and refuse to guess if the value is not a plain decimal.
[[ "$CLAIM_GI" =~ ^[0-9]+$ ]] \
    || fail "global index '$CLAIM_GI' is not a decimal integer — cannot verify the claim against unclaimable_claims"
CLAIM_GI_HEX=$(python3 -c 'import sys; print(hex(int(sys.argv[1])))' "$CLAIM_GI") \
    || fail "could not convert global index '$CLAIM_GI' to hex"
UNCLAIMABLE=$(pgq "SELECT count(*) FROM unclaimable_claims WHERE global_index IN ('$CLAIM_GI_HEX', '$CLAIM_GI')")
[[ "$UNCLAIMABLE" == "0" ]] \
    || fail "our deposit (gi=$CLAIM_GI) has a claim tx ($CLAIM_TX) but is recorded in unclaimable_claims — that is the RD-860 note-less short-circuit: an event was emitted and NO asset moved (#103)"
pass "post-restore claim SETTLED for our deposit: claim_tx=$CLAIM_TX, gi=$CLAIM_GI ($CLAIM_GI_HEX), no unclaimable_claims row (a real claim, not a note-less short-circuit)"

# PR#164 re-review — compare COUNT to COUNT. `INJ1` is the injected-set MD5 from
# `fingerprint()`, not a number: `[[ "$INJ2" -gt "$INJ1" ]]` compared an integer to
# a hex digest, so bash arithmetic evaluated the non-numeric side as 0 and the
# guard passed on the FIRST poll regardless of whether a new GER ever landed. The
# advertised fresh-GER liveness gate therefore asserted nothing. The numeric
# post-restore count is `NINJECTED1` (from `counts()`).
deadline=$((SECONDS + 240))
while :; do
    INJ2=$(pgq "SELECT count(*) FROM ger_entries WHERE is_injected=true")
    [[ "${INJ2:-0}" =~ ^[0-9]+$ ]] || fail "injected-GER count query returned non-numeric '$INJ2'"
    (( INJ2 > NINJECTED1 )) && break
    (( SECONDS >= deadline )) && fail "no NEW GER injected+consumed post-restore in 240s (count stuck at $INJ2, pre-restore baseline $NINJECTED1)"
    sleep 5
done
pass "new GER injected+consumed post-restore (count $NINJECTED1 -> $INJ2)"

step "RESULT"
pass "FULL DB LOSS RECOVERY: faithful history + no poison + live pipeline (evidence: $EVIDENCE)"
