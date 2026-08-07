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

# One key PER ROLE (service, ger-manager): blast-radius isolation, and the proxy
# now refuses to bind two roles to the same key (PR #162 review).
ROLES=(service ger-manager)

# The proxy binds roles to keys by IDENTIFIER, so emit the role=publickey pairs
# compose feeds in as AGGLAYER_SIGNER_KEYS. Written on BOTH the reuse and the
# generate path — a reused key set still needs its env file, and forgetting that
# is how the file goes stale while the keys look fine.
emit_env() {
    local env_file="$KEY_DIR/../web3signer-keys.env" sep="" role priv pub
    {
        printf 'AGGLAYER_SIGNER_KEYS='
        for role_name in "${ROLES[@]}"; do
            f="$(ls "$KEY_DIR/${role_name}-0x"*.yaml 2>/dev/null | head -1)"
            [ -n "$f" ] || { echo "[web3signer-keys] FAIL: no key file for role $role_name" >&2; return 1; }
            role="$role_name"
            priv="$(awk -F'"' '/privateKey:/ {print $2}' "$f")"
            pub="$(cast wallet public-key --private-key "$priv" 2>/dev/null)"
            [ -n "$pub" ] || { echo "[web3signer-keys] FAIL: cannot derive public key for $role" >&2; return 1; }
            printf '%s%s=%s' "$sep" "$role" "$pub"
            sep=","
        done
        printf '\n'
    } > "$env_file" || return 1
    echo "[web3signer-keys] wrote $env_file"
}
# Count keys PER ROLE, not files. A pre-role-binding key dir holds one
# unprefixed <ADDR>.yaml; counting files would (a) see "1 of 2" and generate two
# MORE, leaving three, and (b) later let emit_env turn that legacy filename into
# a bogus role. With two arbitrary files it would even read as complete. So:
# require one file per REQUIRED role, and ignore anything that is not role-named.
missing_roles=()
for role in "${ROLES[@]}"; do
    compgen -G "$KEY_DIR/${role}-0x*.yaml" >/dev/null || missing_roles+=("$role")
done
legacy=$(compgen -G "$KEY_DIR/0x*.yaml" >/dev/null && ls "$KEY_DIR"/0x*.yaml 2>/dev/null | wc -l || echo 0)
if [ "$legacy" -gt 0 ]; then
    echo "[web3signer-keys] NOTE: ignoring $legacy legacy pre-role key file(s) (0x*.yaml)."
    echo "                 They are not role-named, so they cannot be bound to a role."
    echo "                 Delete them if the signer should not load them at all."
fi
if [ ${#missing_roles[@]} -eq 0 ]; then
    echo "[web3signer-keys] reusing existing per-role key(s):"
    for role in "${ROLES[@]}"; do ls "$KEY_DIR/${role}-0x"*.yaml | sed 's/^/  /'; done
    emit_env
    exit $?
fi
echo "[web3signer-keys] generating key(s) for role(s): ${missing_roles[*]}"

# `cast wallet new` gives us a secp256k1 keypair; Web3Signer's file-raw format
# wants the private key hex (no 0x) and derives the public key itself.
for ROLE in "${missing_roles[@]}"; do
WALLET="$(cast wallet new)"
PRIV="$(echo "$WALLET" | awk '/Private key:/ {print $3}' | sed 's/^0x//')"
ADDR="$(echo "$WALLET" | awk '/Address:/ {print $2}')"
[ -n "$PRIV" ] || { echo "[web3signer-keys] FAIL: could not generate a key with cast" >&2; exit 1; }

# Name the file after the address purely for human traceability; Web3Signer
# identifies keys by their public key at the API layer regardless of filename.
KEY_FILE="$KEY_DIR/${ROLE}-${ADDR}.yaml"
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
echo "[web3signer-keys] wrote $KEY_FILE (role $ROLE, address $ADDR)"
done

emit_env
