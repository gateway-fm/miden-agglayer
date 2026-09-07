# shellcheck shell=bash
#
# Preflight for `bridge-out-tool` — the binary every completeness verdict shells
# out to.
#
# WHY THIS EXISTS
#
# loadtest-N30 (38 min), verify-event-completeness and chaos-soak all end in the
# same completeness verdict. When the tool was missing, each burned its full
# runtime and then died on
#     FAIL: .../target/debug/bridge-out-tool not built
# with the load itself having passed 30/30 and zero undelivered bridges. Three
# battery iterations recorded three red targets that read like a product
# regression and were a five-second provisioning miss.
#
# Checking is not enough: a check that only reports still costs the run. BUILD
# it, once, before anything expensive starts — and if the build fails, say so
# loudly here rather than 38 minutes later.
#
# Usage:
#   . "$PROJECT_DIR/scripts/lib-tool-preflight.sh"
#   preflight_bridge_out_tool            # exits non-zero on failure
#   preflight_bridge_out_tool "$LOGDIR"  # build log goes there
# Sets TOOL_BIN to the verified absolute path and exports it.

preflight_bridge_out_tool() {
    local log_dir="${1:-}" root build_log
    root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
    TOOL_BIN="${TOOL_BIN:-$root/target/debug/bridge-out-tool}"

    if [[ ! -x "$TOOL_BIN" ]]; then
        # An explicit TOOL_BIN names a binary the caller expects to exist; do
        # not silently build over the top of a path they chose.
        if [[ "$TOOL_BIN" != "$root/target/debug/bridge-out-tool" ]]; then
            echo "FATAL: TOOL_BIN='$TOOL_BIN' is not an executable file." >&2
            return 1
        fi
        build_log="${log_dir:+$log_dir/}preflight-bridge-out-tool-build.log"
        [[ -n "$log_dir" ]] || build_log="$(mktemp)"
        echo "[preflight] $TOOL_BIN missing — building it now (log: $build_log)"
        if ! (cd "$root" && cargo build --bin bridge-out-tool) >"$build_log" 2>&1; then
            echo "FATAL: 'cargo build --bin bridge-out-tool' FAILED — the completeness" >&2
            echo "       verdict at the end of this run could not have run either." >&2
            echo "       Last 20 lines of $build_log:" >&2
            tail -20 "$build_log" >&2 || true
            return 1
        fi
    fi
    [[ -x "$TOOL_BIN" ]] || {
        echo "FATAL: $TOOL_BIN still not executable after the build." >&2
        return 1
    }
    export TOOL_BIN
    echo "[preflight] bridge-out-tool OK: $TOOL_BIN"
}
