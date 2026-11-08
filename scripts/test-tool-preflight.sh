#!/usr/bin/env bash
set -euo pipefail

fixture="$(mktemp -d)"
trap 'rm -rf "$fixture"' EXIT
mkdir -p "$fixture/project/scripts" "$fixture/project/target/debug" "$fixture/bin" "$fixture/logs"
cp "$(dirname "${BASH_SOURCE[0]}")/lib-tool-preflight.sh" "$fixture/project/scripts/"
cat > "$fixture/bin/cargo" <<'SH'
#!/usr/bin/env bash
set -euo pipefail
[[ "$*" == 'build --locked --bin bridge-out-tool' ]]
[[ "${PREFLIGHT_TEST_FAIL:-0}" == 0 ]] || exit 1
printf 'built\n' >> cargo-calls
printf '#!/usr/bin/env bash\nprintf "fresh\\n"\n' > target/debug/bridge-out-tool
chmod +x target/debug/bridge-out-tool
SH
chmod +x "$fixture/bin/cargo"
export PATH="$fixture/bin:$PATH"
source "$fixture/project/scripts/lib-tool-preflight.sh"

# An executable from an older checkout must not bypass the build.
printf '#!/usr/bin/env bash\nprintf "stale\\n"\n' > "$fixture/project/target/debug/bridge-out-tool"
chmod +x "$fixture/project/target/debug/bridge-out-tool"
TOOL_BIN= preflight_bridge_out_tool "$fixture/logs"
[[ "$("$fixture/project/target/debug/bridge-out-tool")" == fresh ]]
[[ "$(wc -l < "$fixture/project/cargo-calls" | tr -d ' ')" == 1 ]]

# Build failure must fail the gate even though the old executable exists.
if PREFLIGHT_TEST_FAIL=1 TOOL_BIN= preflight_bridge_out_tool "$fixture/logs"; then
    echo 'FAIL: stale executable hid a failed rebuild' >&2
    exit 1
fi

# Explicit external binaries remain caller-owned.
cp "$fixture/project/target/debug/bridge-out-tool" "$fixture/custom-tool"
TOOL_BIN="$fixture/custom-tool" preflight_bridge_out_tool "$fixture/logs"
[[ "$(wc -l < "$fixture/project/cargo-calls" | tr -d ' ')" == 1 ]]
if TOOL_BIN="$fixture/missing-tool" preflight_bridge_out_tool "$fixture/logs"; then
    echo 'FAIL: missing custom tool accepted' >&2
    exit 1
fi
echo 'tool preflight regressions passed'
