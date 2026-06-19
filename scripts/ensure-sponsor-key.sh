#!/usr/bin/env bash
# Decrypt the test-only claimsponsor keystore into SPONSOR_PRIVATE_KEY in
# fixtures/.env, so the Rust `bridge-autoclaim` service (docker-compose.e2e.yml)
# can sign claimAsset on the local Anvil L1.
#
# The Go zkevm-autoclaimer consumed an encrypted keystore + password directly;
# our Rust binary takes a raw key from an env var (--sponsor-key-env, default
# SPONSOR_PRIVATE_KEY) populated from the secret store in production. For the
# local e2e stack the "secret store" is the committed test keystore, decrypted
# here at run time. The key is written ONLY to the gitignored fixtures/.env and
# is never printed.
#
# Idempotent: safe to run before every `make e2e-l2-to-l1-autoclaim`.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
FIXTURES_DIR="$(cd "$SCRIPT_DIR/.." && pwd)/fixtures"
ENV_FILE="$FIXTURES_DIR/.env"
KEYSTORE_NAME="claimsponsor.keystore"

# Fixed test-only password — the same literal setup-fixtures.sh uses for the
# kurtosis-cdk dev keystores (local Anvil only; not a production credential).
# Override via the environment if a future setup randomises it.
KEYSTORE_PASSWORD="${KEYSTORE_PASSWORD:-pSnv6Dh5s9ahuzGzH9RoCDrKAMddaX3m}"

[[ -f "$FIXTURES_DIR/$KEYSTORE_NAME" ]] || { echo "ensure-sponsor-key: $KEYSTORE_NAME missing — run 'make e2e-setup' first" >&2; exit 1; }
[[ -f "$ENV_FILE" ]] || { echo "ensure-sponsor-key: fixtures/.env missing — run 'make e2e-setup' first" >&2; exit 1; }
command -v cast >/dev/null || { echo "ensure-sponsor-key: cast (foundry) not found" >&2; exit 1; }

# Decrypt (separate step so cast's own error is visible), then extract the 0x
# key. `grep -m1` (not `... | head -1`) avoids the SIGPIPE-under-pipefail abort.
ERR_FILE="$(mktemp)"
trap 'rm -f "$ERR_FILE"' EXIT
RAW=$(cast wallet decrypt-keystore "$KEYSTORE_NAME" \
        --keystore-dir "$FIXTURES_DIR" \
        --unsafe-password "$KEYSTORE_PASSWORD" 2>"$ERR_FILE") || {
    echo "ensure-sponsor-key: cast decrypt-keystore failed:" >&2
    cat "$ERR_FILE" >&2
    exit 1
}
# Extract the key; accept with or without 0x prefix and normalise to 0x.
HEX=$(printf '%s' "$RAW" | grep -oiE -m1 '(0x)?[0-9a-f]{64}' | sed 's/^0x//I' || true)
if [[ -z "$HEX" ]]; then
    echo "ensure-sponsor-key: could not extract a private key from decrypt output. Masked output:" >&2
    printf '%s\n' "$RAW" | sed -E 's/[0-9a-fA-F]{16,}/<HEX-REDACTED>/g' >&2
    exit 1
fi
KEY="0x$HEX"

# Idempotently replace any existing SPONSOR_PRIVATE_KEY line. Never echo $KEY.
grep -v '^SPONSOR_PRIVATE_KEY=' "$ENV_FILE" > "$ENV_FILE.tmp" || true
echo "SPONSOR_PRIVATE_KEY=$KEY" >> "$ENV_FILE.tmp"
mv "$ENV_FILE.tmp" "$ENV_FILE"

SPONSOR_ADDR=$(cast wallet address --private-key "$KEY")
echo "ensure-sponsor-key: SPONSOR_PRIVATE_KEY set in fixtures/.env (sponsor $SPONSOR_ADDR)"
