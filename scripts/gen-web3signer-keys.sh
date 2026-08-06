#!/usr/bin/env bash
# ══════════════════════════════════════════════════════════════════════════════
# gen-web3signer-keys.sh — provision the e2e signer's secp256k1 key.
#
# Writes fixtures/web3signer-keys/<pubkey>.yaml in Web3Signer's raw-file key
# format. That file is the ONLY thing production changes: swap `type: file-raw`
# for `type: aws-kms` (or azure/hashicorp) and the same signer serves the same
# API from a vault — the proxy never knows the difference.
#
# The key is generated here rather than committed so no private key ever lives
# in the repo; the directory is gitignored.
#
# Idempotent: keeps an existing key so a re-run does not orphan accounts that
# were already deployed against it.
# ══════════════════════════════════════════════════════════════════════════════
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."
KEY_DIR="fixtures/web3signer-keys"
mkdir -p "$KEY_DIR"
chmod 700 "$KEY_DIR"

if compgen -G "$KEY_DIR/*.yaml" >/dev/null; then
    echo "[web3signer-keys] reusing existing key: $(ls "$KEY_DIR"/*.yaml)"
    exit 0
fi

# `cast wallet new` gives us a secp256k1 keypair; Web3Signer's file-raw format
# wants the private key hex (no 0x) and derives the public key itself.
WALLET="$(cast wallet new)"
PRIV="$(echo "$WALLET" | awk '/Private key:/ {print $3}' | sed 's/^0x//')"
ADDR="$(echo "$WALLET" | awk '/Address:/ {print $2}')"
[ -n "$PRIV" ] || { echo "[web3signer-keys] FAIL: could not generate a key with cast" >&2; exit 1; }

# Name the file after the address purely for human traceability; Web3Signer
# identifies keys by their public key at the API layer regardless of filename.
KEY_FILE="$KEY_DIR/${ADDR}.yaml"
cat > "$KEY_FILE" <<EOF
# Web3Signer raw-file key (E2E ONLY — production uses aws-kms/azure/hashicorp).
# Swapping this block for a vault type is the entire production delta:
#   type: aws-kms
#   region: eu-west-1
#   kmsKeyId: arn:aws:kms:...   # NOTE: kmsKeyId, not keyId
type: file-raw
keyType: SECP256K1
privateKey: "0x${PRIV}"
EOF
chmod 600 "$KEY_FILE"
echo "[web3signer-keys] wrote $KEY_FILE (address $ADDR)"
