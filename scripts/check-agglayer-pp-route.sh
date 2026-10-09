#!/usr/bin/env bash
# Verify that the L1 pessimistic-proof route registered by the `l1-pp-route`
# compose one-shot matches the agglayer image the stack actually runs.
#
# The L1 AggLayerGateway routes settlement proofs by the agglayer's pessimistic
# vkey SELECTOR. If the agglayer image is bumped without updating PP_SELECTOR /
# PP_VKEY, every settlement reverts `RouteNotFound(<selector>)` and no
# certificate ever settles — which surfaces only as a 900 s "certificate
# settled" timeout deep inside test-e2e. This makes that a one-second,
# self-explaining failure at provisioning time instead.
#
# Values are derived exactly as kurtosis-cdk does (src/vkey/agglayer.star):
# `agglayer vkey-selector` and `agglayer vkey`, run from the pinned image.
set -euo pipefail
cd "$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)" || exit 1
COMPOSE=docker-compose.e2e.yml

img=$(awk '/^  agglayer:/{f=1} f && /image:/{print $2; exit}' "$COMPOSE")
want_sel=$(awk '/^  l1-pp-route:/{f=1} f && /PP_SELECTOR:/{gsub(/"/,"",$2); print $2; exit}' "$COMPOSE")
want_vkey=$(awk '/^  l1-pp-route:/{f=1} f && /PP_VKEY:/{gsub(/"/,"",$2); print $2; exit}' "$COMPOSE")
[[ -n "$img" && -n "$want_sel" && -n "$want_vkey" ]] || {
    echo "check-agglayer-pp-route: could not read agglayer image / PP_SELECTOR / PP_VKEY from $COMPOSE" >&2; exit 1; }

got_sel=$(docker run --rm --entrypoint agglayer "$img" vkey-selector | tr -d '\n')
got_vkey=$(docker run --rm --entrypoint agglayer "$img" vkey | tr -d '\n')

if [[ "$got_sel" != "$want_sel" || "$got_vkey" != "$want_vkey" ]]; then
    cat >&2 <<EOF
check-agglayer-pp-route: MISMATCH for $img
  image says:    selector=$got_sel vkey=$got_vkey
  compose has:   selector=$want_sel vkey=$want_vkey  (service l1-pp-route)
Update PP_SELECTOR / PP_VKEY in $COMPOSE, or every L1 settlement will revert
RouteNotFound($got_sel).
EOF
    exit 1
fi
echo "check-agglayer-pp-route: OK — $img selector=$got_sel matches l1-pp-route"
