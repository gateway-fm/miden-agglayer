# shellcheck shell=bash
#
# Shared compose-file resolution: every entry point must drive the stack with
# the SAME custody posture the stack was brought up with.
#
# WHY THIS EXISTS
#
# `docker-compose.e2e.yml` on its own is LOCAL-keystore custody. When the stack
# is brought up with the web3signer overlay (remote custody — Web3Signer, which
# may itself be backed by a cloud KMS), any later `docker compose run/up` of the
# proxy that omits that overlay comes up under a DIFFERENT custody than the
# stack it is driving. For a one-shot such as `--restore` that is not a cosmetic
# difference: the recovery path would run unsigned-or-locally-signed against a
# remotely-custodied history. Before this helper, `make e2e-up` could not enable
# the overlay at all (only the l2l2 compose did), and e2e-restore.sh /
# e2e-cantina6-faucet-identity-restore.sh hardcoded the base file.
#
# USAGE
#   . "$(dirname "${BASH_SOURCE[0]}")/lib-compose.sh"
#   compose_env_load                      # must run in the CALLER's shell
#   mapfile -t COMPOSE < <(compose_files)
#   docker compose "${COMPOSE[@]}" --env-file "$ENV_FILE" ...
#
# EXTRA_COMPOSE_FILES layers a site overlay onto every entry point, e.g. cloud
# credentials for the signer:
#   export EXTRA_COMPOSE_FILES="-f /path/to/docker-compose.kms-local.yml"

_compose_dir() {
    if [[ -n "${PROJECT_DIR:-}" ]]; then printf '%s' "$PROJECT_DIR"
    else (cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd); fi
}

_compose_signer_active() {
    local project="${COMPOSE_PROJECT_NAME:-miden-agglayer}"
    [[ "${WITH_WEB3SIGNER:-0}" = 1 ]] && return 0
    docker ps --format '{{.Names}}' 2>/dev/null | grep -q "^${project}-web3signer-1$"
}

# `${AGGLAYER_SIGNER_KEYS:?}` in the web3signer overlay is interpolated at
# compose PARSE time, so the keys env must be loaded into the caller's shell —
# doing it inside the command substitution below would lose it with the subshell.
compose_env_load() {
    local dir; dir="$(_compose_dir)"
    if _compose_signer_active && [[ -f "$dir/fixtures/web3signer-keys.env" ]]; then
        set -a; . "$dir/fixtures/web3signer-keys.env"; set +a
    fi
}

# Emits one -f argument per line (newline-delimited so paths with spaces survive).
compose_files() {
    local dir project extra
    dir="$(_compose_dir)"
    project="${COMPOSE_PROJECT_NAME:-miden-agglayer}"
    printf '%s\n%s\n' -f "$dir/docker-compose.e2e.yml"
    # `ps -a`, not `ps`: chaos stops containers, and a healer that dropped the
    # l2l2 overlay just because anvil-l2b was down would fail with "no such
    # service". Including the overlay whenever the l2l2 stack EXISTS is always
    # safe; omitting it when it exists is not.
    if [[ -f "$dir/docker-compose.l2l2.yml" ]] \
       && docker ps -a --format '{{.Names}}' 2>/dev/null | grep -q "^${project}-.*l2b"; then
        printf '%s\n%s\n' -f "$dir/docker-compose.l2l2.yml"
    fi
    if _compose_signer_active; then
        printf '%s\n%s\n' -f "$dir/docker-compose.web3signer.yml"
    fi
    # Unquoted on purpose: EXTRA_COMPOSE_FILES is a pre-split "-f a -f b" list.
    # shellcheck disable=SC2086
    for extra in ${EXTRA_COMPOSE_FILES:-}; do printf '%s\n' "$extra"; done
}
