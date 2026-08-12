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
# The proxy addresses postgres by its compose SERVICE name; accept that as well as
# the container name (`<project>-<service>-1`).
PG_SERVICE="${PG_CONTAINER#"$PROJECT-"}"; PG_SERVICE="${PG_SERVICE%-1}"
if [[ -z "$STORE_HOST" ]]; then
    if [[ "${ALLOW_UNVERIFIED_PG_TARGET:-0}" == "1" ]]; then
        echo "WARN: proxy store URL unparsable; proceeding because ALLOW_UNVERIFIED_PG_TARGET=1"
    else
        echo "FATAL: could not parse the proxy's store URL from $PROXY_CONTAINER env, so the"
        echo "       DROP DATABASE target cannot be confirmed. Refusing to destroy a database"
        echo "       chosen by name convention alone. Set ALLOW_UNVERIFIED_PG_TARGET=1 to override."
        exit 1
    fi
elif [[ "$STORE_HOST" != "$PG_CONTAINER" && "$STORE_HOST" != "$PG_SERVICE" \
        && "$STORE_HOST" != "agglayer-postgres" \
        && "$STORE_HOST" != "localhost" && "$STORE_HOST" != "127.0.0.1" ]]; then
    if [[ "${ALLOW_UNVERIFIED_PG_TARGET:-0}" == "1" ]]; then
        echo "WARN: proxy store host '$STORE_HOST' != '$PG_CONTAINER'; proceeding (ALLOW_UNVERIFIED_PG_TARGET=1)"
    else
        echo "FATAL: the proxy's store host is '$STORE_HOST', which is NOT the container this"
        echo "       script would DROP ($PG_CONTAINER). Refusing to destroy a database the"
        echo "       proxy under test does not use. Set ALLOW_UNVERIFIED_PG_TARGET=1 to override."
        exit 1
    fi
else
    echo "verified: proxy store host '$STORE_HOST' matches the drop target $PG_CONTAINER"
fi

COMPOSE=(-f "$PROJECT_DIR/docker-compose.e2e.yml")
[[ -f "$PROJECT_DIR/docker-compose.l2l2.yml" ]] && docker ps --format '{{.Names}}' | grep -q "^$PROJECT-anvil-l2b-1$" \
    && COMPOSE+=(-f "$PROJECT_DIR/docker-compose.l2l2.yml")
if docker ps --format '{{.Names}}' | grep -q "^$PROJECT-web3signer-1$"; then
    COMPOSE+=(-f "$PROJECT_DIR/docker-compose.web3signer.yml")
    # ${AGGLAYER_SIGNER_KEYS:?} is interpolated at compose parse time.
    [[ -f "$PROJECT_DIR/fixtures/web3signer-keys.env" ]] && { set -a; . "$PROJECT_DIR/fixtures/web3signer-keys.env"; set +a; }
fi

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
log_digest() { # $1 = topic0 hex prefix
    pgq "SELECT md5(coalesce(string_agg(
           transaction_hash || ':' || log_index || ':' || block_number || ':' ||
           encode(block_hash,'hex') || ':' || address || ':' ||
           array_to_string(topics, ',') || ':' ||
           transaction_index::text || ':' || removed::text || ':' || data,
           '|' ORDER BY transaction_hash, log_index), '')) \
         FROM synthetic_logs WHERE topics[1] LIKE '$1%'"
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
    pgq "SELECT md5(coalesce(string_agg(
           block_number || ':' || encode(block_hash,'hex') || ':' || address || ':' ||
           array_to_string(topics, ',') || ':' || removed::text || ':' || data,
           '|' ORDER BY block_number, data), '')) \
         FROM synthetic_logs WHERE topics[1] LIKE '0x65d3bf36%'"
}

# Row-level dump for diagnosis. A digest tells you THAT something differs; this
# tells you WHICH FIELD. Written before the drop and after the restore so a
# mismatch prints an exact per-field diff instead of two opaque md5s.
dump_rows() { # $1 = topic0 prefix, $2 = output file
    pgq "SELECT log_index || ' | blk=' || block_number || ' | txh=' || transaction_hash
         || ' | txi=' || transaction_index || ' | rm=' || removed
         || ' | bh=' || encode(block_hash,'hex') || ' | addr=' || address
         || ' | topics=' || array_to_string(topics, ',') || ' | data=' || data
         FROM synthetic_logs WHERE topics[1] LIKE '$1%'
         ORDER BY block_number, log_index" > "$2" 2>/dev/null
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
           log_index || ':' || block_number || ':' || encode(block_hash,'hex') || ':' ||
           address || ':' || array_to_string(topics, ',') || ':' ||
           transaction_index::text || ':' || removed::text || ':' || data,
           '|' ORDER BY block_number, log_index), '')) \
         FROM synthetic_logs WHERE topics[1] LIKE '0x1df3f2a9%'"
}
# CONSUMER-LEVEL capture: what an actual client sees via eth_getLogs, not what our
# own SQL says. The SQL digests above read the table we wrote; this reads the JSON-RPC
# surface aggkit/bridge-service actually consume, so a serialisation-level regression
# (field omitted, hex padding, ordering) cannot hide behind a matching row digest.
#
# Emits one TSV line per log, ordered by (blockNumber, logIndex) NUMERICALLY — hex
# strings of differing width do not sort lexically.
PROXY_RPC="${PROXY_RPC:-http://localhost:8546}"
getlogs_dump() { # $1 = output file
    curl -s -m 120 -X POST "$PROXY_RPC" -H 'content-type: application/json' \
        -d '{"jsonrpc":"2.0","id":1,"method":"eth_getLogs","params":[{"fromBlock":"0x0","toBlock":"latest"}]}' \
    | jq -r '
        def h2d: ltrimstr("0x") | explode
                 | reduce .[] as $c (0; . * 16 + (if $c >= 48 and $c <= 57 then $c - 48
                                                  elif $c >= 97 then $c - 87 else $c - 55 end));
        [ .result[] | { b: (.blockNumber|h2d), i: (.logIndex|h2d), r: . } ]
        | sort_by([.b, .i])[]
        | [ (.b|tostring), (.i|tostring), .r.address, (.r.topics|join(",")), .r.data,
            .r.transactionIndex, (.r.removed|tostring), .r.transactionHash ]
        | @tsv' > "$1" 2>/dev/null
    [[ -s "$1" ]]
}

fingerprint() {  # -> "uhc_d inj_d bridge_d claim_d hcv"  (digests, for identity assertion)
    local uhc inj bridge claim hcv
    uhc=$(uhc_content_digest)
    inj=$(pgq "SELECT md5(coalesce(string_agg(encode(ger_hash,'hex'), '|' ORDER BY ger_hash), '')) \
               FROM ger_entries WHERE is_injected=true")
    bridge=$(log_digest '0x50178120')
    claim=$(claim_content_digest)
    hcv=$(pgq "SELECT encode(hash_chain_value,'hex') FROM service_state WHERE id=1")
    echo "$uhc $inj $bridge $claim $hcv"
}
counts() {  # -> "uhc inj bridge claim"  (integers, for logging + thinness gate)
    local uhc inj bridge claim
    uhc=$(pgq "SELECT count(*) FROM synthetic_logs WHERE topics[1] LIKE '0x65d3bf36%'")
    inj=$(pgq "SELECT count(*) FROM ger_entries WHERE is_injected=true")
    bridge=$(pgq "SELECT count(*) FROM synthetic_logs WHERE topics[1] LIKE '0x50178120%'")
    claim=$(pgq "SELECT count(*) FROM synthetic_logs WHERE topics[1] LIKE '0x1df3f2a9%'")
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
[[ "$NUHC0" -ge 1 && "$NINJECTED0" -ge 1 ]] || fail "fixture too thin (UHC=$NUHC0 inj=$NINJECTED0) — run traffic first"

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

# ── Phase 4: no poison minted, pipeline alive ────────────────────────────────
step "Phase 4 — no ERR_GER_ALREADY_REGISTERED poison; pipeline processes NEW traffic"
sleep 45   # window for aggoracle to (wrongly) re-inject + ntx to (wrongly) assert
NTX_NOW=$(docker logs "$NTX_CONTAINER" 2>&1 | grep -c "1007209807211405110" || true)
[[ "$NTX_NOW" -le "$NTX_MARK" ]] \
    || fail "#86: NEW poison-note kernel asserts after restore ($NTX_MARK -> $NTX_NOW) — aggoracle re-injected registered GERs"
pass "zero new poison-note kernel asserts ($NTX_NOW)"

CNT=$(cast call "$L1_BRIDGE_ADDRESS" 'depositCount()(uint256)' --rpc-url "$L1_RPC")
DEST="0x00000000000000000000000000$(printf '%014x' "$RUN_SUFFIX")"
say "liveness: bridgeAsset cnt=$CNT dest=$DEST"
cast send --rpc-url "$L1_RPC" --private-key "$SIGNER_KEY" "$L1_BRIDGE_ADDRESS" \
  'bridgeAsset(uint32,address,uint256,address,bool,bytes)' \
  1 "$DEST" "$DEPOSIT_WEI" 0x0000000000000000000000000000000000000000 true 0x \
  --value "$DEPOSIT_WEI" >/dev/null

deadline=$((SECONDS + 300))
while :; do
    READY=$(curl -sf "$BRIDGE_SERVICE_URL/bridges/$DEST?limit=25&offset=0" 2>/dev/null \
        | python3 -c "import json,sys; ds=json.load(sys.stdin).get('deposits',[]); print(any(d.get('ready_for_claim') for d in ds))" 2>/dev/null || echo err)
    [[ "$READY" == "True" ]] && break
    (( SECONDS >= deadline )) && fail "post-restore deposit never ready_for_claim in 300s — pipeline dead after restore"
    sleep 5
done
pass "post-restore deposit ready_for_claim"

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
