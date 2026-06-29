#!/usr/bin/env bash
#
# run-all.sh — provision the protocol-0.15 miden-agglayer e2e stack FROM SCRATCH,
# run the tests, and report EVERYTHING: L1 transactions + receipts, synthetic
# BridgeEvents, Miden note IDs (GER / CLAIM / MINT / P2ID / B2AGG), Global Exit
# Roots, AggLayer certificates + settlement txs, and balances on both sides.
#
# Idempotent: each provisioning step is skipped if already satisfied, so re-runs
# are fast. Designed to work on a bare Ubuntu box (installs every dependency).
#
# Usage:
#   ./run-all.sh                 # provision + static checks + instrumented
#                                # L1<->L2 round-trip with full artifact report
#   ./run-all.sh --matrix        # additionally run the full regression matrix
#                                # (run-regression.sh; ~2-4h, fresh stack/suite)
#   ./run-all.sh --report-only   # skip provisioning/tests; just dump artifacts
#                                # from an already-running stack
#
# Env knobs:
#   SKIP_PROVISION=1   skip phase 0 (assume tools/images/fixtures present)
#   SKIP_STATIC=1      skip unit tests / clippy / lint
#   KEEP_UP=1          leave the stack running at the end (default: leave up)
#
set -uo pipefail

# ── Re-exec with the docker group active if we can't reach the daemon ─────────
if ! docker info >/dev/null 2>&1; then
  if id -nG "$USER" 2>/dev/null | grep -qw docker; then
    exec sg docker -c "$(printf '%q ' "$0" "$@")"
  fi
fi

PROJECT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$PROJECT_DIR"
WORK="$(cd "$PROJECT_DIR/.." && pwd)"           # where sibling repos live
OUT="$PROJECT_DIR/out"; mkdir -p "$OUT"
REPORT="$OUT/RUN-ALL-REPORT.txt"
: > "$REPORT"

# Pin coords (kept in sync with the Makefile / setup-fixtures expectations).
export MIDEN_NODE_GIT_URL="https://github.com/0xMiden/node.git"
export MIDEN_NODE_GIT_REF="v0.15.0"
export PATH="/usr/local/bin:$HOME/.cargo/bin:$PATH"
[ -s "$HOME/.cargo/env" ] && . "$HOME/.cargo/env" 2>/dev/null || true

L1_RPC="http://localhost:8545"
L2_RPC="http://localhost:8546"
BRIDGE_SVC="http://localhost:18080"
PG_BRIDGE="miden-agglayer-postgres-1"
C_PROXY="miden-agglayer-miden-agglayer-1"
C_AGGKIT="miden-agglayer-aggkit-1"
C_AUTOCLAIM="miden-agglayer-bridge-autoclaim-1"

# ── pretty output (to terminal AND $REPORT) ──────────────────────────────────
B="\033[1m"; G="\033[0;32m"; Y="\033[0;33m"; R="\033[0;31m"; C="\033[0;36m"; N="\033[0m"
say()    { echo -e "$*" | tee -a "$REPORT"; }
phase()  { say ""; say "${B}${C}╔══════════════════════════════════════════════════════════════════════╗${N}";
           say "${B}${C}║ $* ${N}"; say "${B}${C}╚══════════════════════════════════════════════════════════════════════╝${N}"; }
section(){ say ""; say "${B}── $* ─────────────────────────────────────────${N}"; }
ok()     { say "${G}✓${N} $*"; }
warn()   { say "${Y}!${N} $*"; }
die()    { say "${R}✗ $*${N}"; exit 1; }
run()    { say "${C}\$ $*${N}"; eval "$*" 2>&1 | tee -a "$REPORT"; return "${PIPESTATUS[0]}"; }

MATRIX=0; REPORT_ONLY=0
for a in "$@"; do case "$a" in
  --matrix) MATRIX=1 ;; --report-only) REPORT_ONLY=1 ;;
  *) die "unknown arg: $a" ;;
esac; done

say "${B}miden-agglayer run-all — $(date -u '+%Y-%m-%dT%H:%M:%SZ')${N}"
say "report file: $REPORT"

# ════════════════════════════════════════════════════════════════════════════
# PHASE 0 — PROVISION FROM SCRATCH (idempotent)
# ════════════════════════════════════════════════════════════════════════════
provision() {
  phase "PHASE 0 — PROVISION (toolchain, repos, images, L1 fixtures)"

  section "0a · host toolchain"
  if ! command -v cc >/dev/null;   then sudo env DEBIAN_FRONTEND=noninteractive NEEDRESTART_MODE=a apt-get install -y build-essential pkg-config; fi
  if ! command -v psql >/dev/null; then sudo env DEBIAN_FRONTEND=noninteractive NEEDRESTART_MODE=a apt-get install -y postgresql-client; fi
  if ! command -v jq >/dev/null;   then sudo env DEBIAN_FRONTEND=noninteractive NEEDRESTART_MODE=a apt-get install -y jq; fi
  command -v docker >/dev/null || die "docker not installed (apt-get install docker.io)"
  docker buildx version >/dev/null 2>&1 || sudo env DEBIAN_FRONTEND=noninteractive apt-get install -y docker-buildx || true
  if ! command -v cargo >/dev/null; then
    curl -sSL https://sh.rustup.rs -o /tmp/rustup.sh && sh /tmp/rustup.sh -y --default-toolchain stable --profile minimal
    . "$HOME/.cargo/env"
  fi
  rustup component add rustfmt clippy >/dev/null 2>&1 || true
  command -v cast >/dev/null || { f=/tmp/foundry.tgz; curl -sSL "https://github.com/foundry-rs/foundry/releases/download/stable/foundry_stable_linux_amd64.tar.gz" -o "$f" && (cd /tmp && tar xzf "$f" forge cast anvil && sudo install -m755 forge cast anvil /usr/local/bin/); }
  command -v kurtosis >/dev/null || { tag=$(curl -sSL https://api.github.com/repos/kurtosis-tech/kurtosis-cli-release-artifacts/releases/latest | grep -oP '"tag_name":\s*"\K[^"]+' | head -1); curl -sSL "https://github.com/kurtosis-tech/kurtosis-cli-release-artifacts/releases/download/${tag}/kurtosis-cli_${tag}_linux_amd64.tar.gz" -o /tmp/k.tgz && (cd /tmp && tar xzf k.tgz kurtosis && sudo install -m755 kurtosis /usr/local/bin/); }
  command -v taplo >/dev/null || { curl -sSL "https://github.com/tamasfe/taplo/releases/latest/download/taplo-linux-x86_64.gz" -o /tmp/taplo.gz && gunzip -f /tmp/taplo.gz && sudo install -m755 /tmp/taplo /usr/local/bin/taplo; }
  command -v typos >/dev/null || { tv=$(curl -sSL https://api.github.com/repos/crate-ci/typos/releases/latest | grep -oP '"tag_name":\s*"\K[^"]+' | head -1); curl -sSL "https://github.com/crate-ci/typos/releases/download/${tv}/typos-${tv}-x86_64-unknown-linux-musl.tar.gz" -o /tmp/typos.tgz && (cd /tmp && tar xzf typos.tgz ./typos && sudo install -m755 typos /usr/local/bin/typos); }
  if ! command -v node >/dev/null; then
    export NVM_DIR="$HOME/.nvm"; [ -d "$NVM_DIR" ] || git clone --depth 1 --branch v0.40.1 https://github.com/nvm-sh/nvm.git "$NVM_DIR"
    . "$NVM_DIR/nvm.sh"; nvm install --lts; nb="$(dirname "$(nvm which default)")"; sudo ln -sf "$nb/node" /usr/local/bin/node; sudo ln -sf "$nb/npm" /usr/local/bin/npm
  fi
  ok "tools: $(cargo --version | cut -d' ' -f1-2), cast $(cast --version | head -1 | awk '{print $2}'), kurtosis $(kurtosis version 2>/dev/null | awk '/CLI/{print $3}'), node $(node --version)"

  section "0b · companion repos (siblings of this checkout)"
  [ -d "$WORK/aggkit-proxy" ]   || git clone https://github.com/mandrigin/aggkit-proxy.git "$WORK/aggkit-proxy"
  # The 0.15 migration is now on main (feat/protocol-0.15-migration merged in).
  git -C "$WORK/aggkit-proxy" checkout -q main 2>/dev/null || true
  git -C "$WORK/aggkit-proxy" pull -q --ff-only 2>/dev/null || true
  # L1-snapshot-only gate so kurtosis doesn't demand the local miden/aggkit images:
  local cdkpkg="$WORK/aggkit-proxy/kurtosis/miden-cdk"
  grep -q "deploy_miden_services" "$cdkpkg/main.star" 2>/dev/null || warn "miden-cdk main.star lacks deploy_miden_services gate (already patched on main)"
  grep -q "deploy_miden_services: false" "$cdkpkg/params.yaml" 2>/dev/null || \
    sed -i 's/^miden:/miden:\n  deploy_miden_services: false/' "$cdkpkg/params.yaml" 2>/dev/null || true
  [ -d "$WORK/kurtosis-cdk" ]   || git clone --depth 1 https://github.com/0xPolygon/kurtosis-cdk.git "$WORK/kurtosis-cdk"
  [ -d "$WORK/miden-node-src" ] || git clone --depth 1 --branch "$MIDEN_NODE_GIT_REF" "$MIDEN_NODE_GIT_URL" "$WORK/miden-node-src"
  # Raise the ntx-builder remote-prover client timeout (10s default < ~12.5s
  # B2AGG proof on a normal VM) so L2->L1 consumption isn't cancelled.
  for f in bin/ntx-builder/src/lib.rs bin/ntx-builder/src/actor/mod.rs; do
    p="$WORK/miden-node-src/$f"
    grep -q "with_timeout(Duration::from_secs(180))" "$p" 2>/dev/null || \
      sed -i 's#RemoteTransactionProver::new(\(self\.\)\?\(tx_prover_url\|url\)\.as_str())#&\n                    .with_timeout(Duration::from_secs(180))#' "$p" 2>/dev/null || true
  done
  [ -d "$WORK/zkevm-bridge-service" ] || git clone --depth 1 --branch fix/pending-bridges-rollup-disambiguation https://github.com/revitteth/zkevm-bridge-service.git "$WORK/zkevm-bridge-service"
  ok "repos present under $WORK"

  section "0c · docker images (built only if missing)"
  build_node_img() { # bin port tag
    docker image inspect "$3" >/dev/null 2>&1 && { ok "$3 (cached)"; return; }
    say "building $3 ..."; ( cd "$WORK/miden-node-src" && DOCKER_BUILDKIT=1 docker build --build-arg CREATED=2026-01-01T00:00:00Z --build-arg VERSION="$MIDEN_NODE_GIT_REF" --build-arg COMMIT="$(git rev-parse HEAD)" --build-arg BIN="$1" --build-arg PORT="$2" -t "$3" . ) | tail -2
  }
  build_node_img miden-validator     50101 miden-validator
  build_node_img miden-node          57291 miden-node
  build_node_img miden-ntx-builder   50301 miden-ntx-builder
  build_node_img miden-remote-prover 50051 miden-remote-prover
  if ! docker image inspect zkevm-bridge-service:v0.6.4-RC2-pendingbridges >/dev/null 2>&1; then
    say "building patched zkevm-bridge-service ..."; ( cd "$WORK/zkevm-bridge-service" && DOCKER_BUILDKIT=1 docker build -t zkevm-bridge-service:v0.6.4-RC2-pendingbridges -f ./Dockerfile . ) | tail -2
  else ok "zkevm-bridge-service:v0.6.4-RC2-pendingbridges (cached)"; fi

  section "0d · L1 fixtures (kurtosis CDK snapshot -> anvil replay)"
  if [ -s "$PROJECT_DIR/fixtures/.env" ] && [ -s "$PROJECT_DIR/fixtures/l1-raw-txs.txt" ]; then
    ok "fixtures already generated ($(wc -l <fixtures/l1-raw-txs.txt) L1 txs)"
  else
    kurtosis engine start >/dev/null 2>&1 || true
    KURTOSIS_CDK_DIR="$cdkpkg" bash scripts/setup-fixtures.sh
  fi
  ./scripts/ensure-e2e-secrets.sh
  ./scripts/ensure-sponsor-key.sh
  ok "fixtures ready"
}

# ════════════════════════════════════════════════════════════════════════════
# PHASE 1 — STATIC CHECKS
# ════════════════════════════════════════════════════════════════════════════
static_checks() {
  phase "PHASE 1 — STATIC CHECKS (build · unit tests · lint)"
  section "cargo build"; run "cargo build --workspace --all-targets" || die "build failed"
  section "unit tests";  run "cargo test --workspace --lib" || die "unit tests failed"
  section "make lint (fmt · toml · typos · clippy)"; run "make lint" || die "lint failed"
  ok "static checks green"
}

# ════════════════════════════════════════════════════════════════════════════
# Helpers for the artifact report
# ════════════════════════════════════════════════════════════════════════════
addr() { jq -r ".$1" "$PROJECT_DIR/fixtures/combined.json" 2>/dev/null; }
psql_bridge() { docker exec "$PG_BRIDGE" psql -U bridge_user -d bridge_db -t -A -F' | ' -c "$1" 2>/dev/null; }

dump_l1_bridge_events() {
  local bridge; bridge="$(addr polygonZkEVMBridgeAddress)"
  section "L1 bridge contract ($bridge) — events + tx receipts"
  local hashes
  hashes=$(cast logs --from-block 1 --to-block latest --address "$bridge" --rpc-url "$L1_RPC" 2>/dev/null \
            | awk '/transactionHash:/{print $2}' | sort -u)
  if [ -z "$hashes" ]; then say "(no bridge events on L1 yet)"; return; fi
  local h blk frm to status
  for h in $hashes; do
    blk=$(cast receipt "$h" --rpc-url "$L1_RPC" 2>/dev/null | awk '/^blockNumber/{print $2}')
    frm=$(cast tx "$h" from --rpc-url "$L1_RPC" 2>/dev/null)
    to=$(cast tx "$h" to --rpc-url "$L1_RPC" 2>/dev/null)
    status=$(cast receipt "$h" status --rpc-url "$L1_RPC" 2>/dev/null)
    say "  tx $h  block=$blk status=$status"
    say "      from=$frm to=$to"
  done
}

dump_l2_synth_logs() {
  section "L2 -> L1 synthetic BridgeEvents (bridge-out exits)"
  # eth_getLogs on the proxy is range-limited AND its eth_blockNumber mirror can
  # lag the synthetic-log tip, so we read the authoritative emission straight
  # from the proxy log (note id, synthetic tx hash, amount, L2 block).
  local out
  out=$(docker logs "$C_PROXY" 2>&1 | grep -iE "emitted BridgeEvent" \
        | sed -E 's/.*note_id: ([0-9a-f]+), synthetic_tx_hash: (0x[0-9a-f]+), deposit_count: ([0-9]+), destination_network: ([0-9]+), amount: ([0-9]+), block_number: ([0-9]+).*/note_id=\1 synthetic_tx=\2 deposit_count=\3 dest_net=\4 amount=\5 l2_block=\6/' \
        | sort -u)
  [ -n "$out" ] && say "$out" | sed 's/^/  /' || say "  (no bridge-out exits yet)"
}

dump_note_ids() {
  section "Miden note IDs & note activity (from proxy log)"
  docker logs "$C_PROXY" 2>&1 \
    | grep -iE "note_id|UpdateGerNote|inserted GER|CLAIM (note|committed)|MINT|P2ID|emitted BridgeEvent|bridge-out" \
    | grep -ivE "alloy_transport|reqwest" | tail -60 | sed 's/^/  /' || say "(none)"
}

dump_gers() {
  section "Global Exit Roots — injected on L2 (proxy) & synced (bridge-service)"
  say "  proxy GER injections:"
  docker logs "$C_PROXY" 2>&1 | grep -iE "GER injection: submitting|inserted GER with eth txn" | tail -10 | sed 's/^/    /' || true
  say "  aggkit aggoracle GER injects:"
  docker logs "$C_AGGKIT" 2>&1 | grep -iE "inject GER transaction submitted" | tail -10 | sed 's/^/    /' || true
  say "  bridge-service exit_root (GER) table:"
  psql_bridge "SELECT id, block_id, network_id, encode(global_exit_root,'hex') FROM sync.exit_root ORDER BY id;" | sed 's/^/    /' || true
}

dump_deposits_claims() {
  section "bridge-service — deposits (L1->L2 and L2->L1)"
  say "  deposit_cnt | net | dest_net | ready | orig_addr | tx_hash"
  psql_bridge "SELECT deposit_cnt, network_id, dest_net, ready_for_claim, encode(orig_addr,'hex'), encode(tx_hash,'hex') FROM sync.deposit ORDER BY network_id, deposit_cnt;" | sed 's/^/  /' || true
  section "bridge-service — claims (settled on destination)"
  psql_bridge "SELECT index, orig_net, network_id, encode(tx_hash,'hex') FROM sync.claim ORDER BY network_id, index;" | sed 's/^/  /' || say "  (no claims yet)"
}

dump_certs() {
  section "AggLayer certificates (aggsender) — unique id · status · settlement tx"
  local u
  u=$(docker logs "$C_AGGKIT" 2>&1 \
      | sed -nE 's/.*Height: ([0-9]+), CertificateID: (0x[0-9a-f]+),.*Status: ([A-Za-z?]+)\. SettlementTxnHash: (0x[0-9a-f]+).*/  height=\1  cert=\2  status=\3  settleTx=\4/p' \
      | sort -u)
  [ -n "$u" ] && say "$u" || say "  (no settled certificates yet)"
  say "  status transitions:"
  docker logs "$C_AGGKIT" 2>&1 | grep -iE "changed status .* to .*|certificate .* sent" | tail -8 | sed 's/^/    /' || true
}

dump_balances() {
  section "Balances & chain tips"
  say "  L1 (anvil) head:      $(cast block-number --rpc-url "$L1_RPC" 2>/dev/null)"
  say "  L2 (proxy) head:      $(cast block-number --rpc-url "$L2_RPC" 2>/dev/null) (eth_blockNumber; may lag synthetic-log tip)"
  # ETH balances of the L1 addresses that actually appear in the bridge txs
  # (kurtosis admin + claim sponsor), discovered from the bridge events.
  local bridge addrs a bal
  bridge="$(addr polygonZkEVMBridgeAddress)"
  addrs=$(cast logs --from-block 1 --to-block latest --address "$bridge" --rpc-url "$L1_RPC" 2>/dev/null \
          | awk '/transactionHash:/{print $2}' | sort -u \
          | while read -r h; do cast tx "$h" from --rpc-url "$L1_RPC" 2>/dev/null; done | sort -u)
  say "  L1 ETH balances of bridge-tx senders:"
  for a in $addrs; do
    bal=$(cast balance "$a" --rpc-url "$L1_RPC" 2>/dev/null)
    say "    $a  ${bal} wei"
  done
  say "  (token amounts per bridge are in the deposits/claims tables above)"
}

artifact_report() {
  phase "ARTIFACT REPORT — L1 txs · note IDs · GERs · certificates · balances"
  if ! docker ps --format '{{.Names}}' | grep -q "$C_PROXY"; then warn "stack not running — bring it up first"; return; fi
  dump_l1_bridge_events
  dump_l2_synth_logs
  dump_note_ids
  dump_gers
  dump_deposits_claims
  dump_certs
  dump_balances
  ok "artifact report complete"
}

# ════════════════════════════════════════════════════════════════════════════
# PHASE 2 — E2E: bring up the stack, run an instrumented L1<->L2 round-trip
# ════════════════════════════════════════════════════════════════════════════
e2e_observe() {
  phase "PHASE 2 — E2E STACK + INSTRUMENTED BRIDGE ROUND-TRIP"
  section "tearing down any previous stack"
  make e2e-down >/dev/null 2>&1 || true; sudo rm -rf "$PROJECT_DIR/.miden-agglayer-data" 2>/dev/null || true

  section "bringing up the full stack (anvil · miden node microservices · proxy · agglayer · aggkit · bridge-service · autoclaim)"
  run "make e2e-up" || die "stack failed to come up"
  say ""; run "docker compose -f docker-compose.e2e.yml --env-file fixtures/.env ps --format '{{.Service}}\t{{.Status}}'"

  section "L1 -> L2 bridge-in (deposit -> GER inject -> CLAIM -> MINT -> P2ID)"
  run "./scripts/e2e-l1-to-l2.sh" || die "L1->L2 failed"

  section "L2 -> L1 bridge-out (B2AGG -> BridgeEvent -> certificate -> settle -> claimAsset)"
  run "./scripts/e2e-l2-to-l1.sh" || die "L2->L1 failed"

  artifact_report
}

# ════════════════════════════════════════════════════════════════════════════
# main
# ════════════════════════════════════════════════════════════════════════════
if [ "$REPORT_ONLY" = 1 ]; then artifact_report; exit 0; fi
[ "${SKIP_PROVISION:-0}" = 1 ] || provision
[ "${SKIP_STATIC:-0}" = 1 ]    || static_checks
e2e_observe

if [ "$MATRIX" = 1 ]; then
  phase "PHASE 3 — FULL REGRESSION MATRIX (run-regression.sh)"
  run "bash ./run-regression.sh"
  section "regression summary"; run "cat out/REGRESSION-SUMMARY.txt"
fi

phase "DONE"
ok "Full report saved to: $REPORT"
[ "${KEEP_UP:-1}" = 1 ] && warn "stack left running (KEEP_UP=1). Tear down with: make e2e-down" || { make e2e-down >/dev/null 2>&1; ok "stack torn down"; }
