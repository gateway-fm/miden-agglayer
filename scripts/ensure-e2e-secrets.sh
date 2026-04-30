#!/usr/bin/env bash
# Ensure fixtures/.env contains every per-run secret the e2e stack needs,
# generating fresh values when missing. Idempotent: existing values are
# left untouched so a single docker-compose lifecycle stays consistent.
#
# Run as a prereq for `make test-e2e` and any standalone `docker compose
# up` that points at docker-compose.e2e.yml.
#
# Why generate instead of commit
# ------------------------------
# GitHub secret-scanning rightly flags hardcoded API keys, and a "dev-only"
# value committed once tends to drift into production via copy-paste. The
# proxy's R1 admin auth fails closed without ADMIN_API_KEY set, so this
# script must run before `docker compose up` or `admin_*` calls 401.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ENV_FILE="$SCRIPT_DIR/../fixtures/.env"

if [[ ! -f "$ENV_FILE" ]]; then
    echo "fixtures/.env missing — run scripts/setup-fixtures.sh first" >&2
    exit 1
fi

ensure_secret() {
    local key="$1"
    local generator="$2"
    if grep -q "^${key}=" "$ENV_FILE"; then
        return 0
    fi
    local value
    value=$(eval "$generator")
    printf '%s=%s\n' "$key" "$value" >> "$ENV_FILE"
    echo "ensure-e2e-secrets: generated $key"
}

ensure_secret "ADMIN_API_KEY" "openssl rand -hex 32"
