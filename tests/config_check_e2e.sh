#!/usr/bin/env bash
# End-to-end config dry-run test for real web binaries.
#
# This intentionally runs the compiled binaries against real --config TOML
# files. --check-config exits before DB/Hydra/server startup, so no services
# need to be live.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
BIN="$ROOT/target/debug"
TMPDIR="$(mktemp -d -t zeroship-config-check-e2e-XXXXXX)"

PASS=0
FAIL=0
LAST_STATUS=0
LAST_STDOUT=""
LAST_STDERR=""

cleanup() {
    rm -rf "$TMPDIR"
}
trap cleanup EXIT

pass() {
    PASS=$((PASS + 1))
    echo "PASS: $1"
}

fail() {
    FAIL=$((FAIL + 1))
    echo "FAIL: $1"
}

show_last_output() {
    echo "  status: $LAST_STATUS"
    echo "  stdout:"
    if [ -s "$LAST_STDOUT" ]; then
        sed 's/^/    /' "$LAST_STDOUT"
    else
        echo "    <empty>"
    fi
    echo "  stderr:"
    if [ -s "$LAST_STDERR" ]; then
        sed 's/^/    /' "$LAST_STDERR"
    else
        echo "    <empty>"
    fi
}

run_cmd() {
    local name="$1"
    shift

    LAST_STDOUT="$TMPDIR/$name.stdout"
    LAST_STDERR="$TMPDIR/$name.stderr"

    set +e
    env -i PATH="$PATH" HOME="${HOME:-}" "$@" >"$LAST_STDOUT" 2>"$LAST_STDERR"
    LAST_STATUS=$?
    set -e
}

expect_status() {
    local expected="$1"
    local label="$2"

    if [ "$LAST_STATUS" -eq "$expected" ]; then
        pass "$label"
    else
        fail "$label (expected status $expected, got $LAST_STATUS)"
    fi
}

expect_nonzero() {
    local label="$1"

    if [ "$LAST_STATUS" -ne 0 ]; then
        pass "$label"
    else
        fail "$label (expected non-zero status)"
    fi
}

expect_stdout_contains() {
    local needle="$1"
    local label="$2"

    if grep -Fq "$needle" "$LAST_STDOUT"; then
        pass "$label"
    else
        fail "$label (missing $needle)"
    fi
}

expect_stdout_not_contains() {
    local needle="$1"
    local label="$2"

    if grep -Fq "$needle" "$LAST_STDOUT"; then
        fail "$label (unexpected $needle)"
    else
        pass "$label"
    fi
}

expect_stderr_contains() {
    local needle="$1"
    local label="$2"

    if grep -Fq "$needle" "$LAST_STDERR"; then
        pass "$label"
    else
        fail "$label (missing $needle)"
    fi
}

cat >"$TMPDIR/shared.toml" <<'TOML'
[auth]
hydra_admin_url = "http://hydra-from-file:4445"
hydra_public_url = "http://hydra-pub-from-file:4444"
trusted_oauth_clients = ["zeroship-builder", "zeroship-console"]

[observability]
rust_log = "info,zeroship_=debug"
log_format = "json"
TOML

cat >"$TMPDIR/remote-hydra.toml" <<'TOML'
[auth]
hydra_admin_url = "http://evil.example.com:4445"
TOML

cat >"$TMPDIR/bad-filter.toml" <<'TOML'
[observability]
rust_log = '!!!not a valid filter!!!'
TOML

echo "============================================"
echo "  zeroship config --check-config E2E"
echo "============================================"
echo ""

echo "=== Build ==="
if cargo build -p zeroship-control -p zeroship-gateway -p zeroship-worker -p zeroship-auth >"$TMPDIR/build.log" 2>&1; then
    pass "built debug web binaries"
else
    fail "built debug web binaries"
    sed 's/^/  /' "$TMPDIR/build.log"
    exit 1
fi
echo ""

CONTROL="$BIN/zeroship-control"
GATEWAY="$BIN/zeroship-gate"
WORKER="$BIN/zeroship-worker"
AUTH="$BIN/zeroship-auth"

echo "=== Case 1: overlay-applied ==="
run_cmd control-overlay "$CONTROL" --check-config --config "$TMPDIR/shared.toml" --dev-insecure
show_last_output
expect_status 0 "control exits 0"
expect_stdout_contains "hydra_admin_url = http://hydra-from-file:4445" "control uses file hydra admin URL"
expect_stdout_contains "trusted_oauth_clients_count = 2" "control reports trusted client count 2"
echo ""

run_cmd gateway-overlay "$GATEWAY" --check-config --config "$TMPDIR/shared.toml" --dev-insecure
show_last_output
expect_status 0 "gateway exits 0"
expect_stdout_contains "hydra_public_url = http://hydra-pub-from-file:4444" "gateway uses file hydra public URL"
echo ""

run_cmd worker-overlay "$WORKER" --check-config --config "$TMPDIR/shared.toml" --dev-insecure
show_last_output
expect_status 0 "worker exits 0"
expect_stdout_contains "check-config: bind = 127.0.0.1" "worker prints dry-run summary"
echo ""

run_cmd auth-overlay "$AUTH" --check-config --config "$TMPDIR/shared.toml" --db-url postgres://check-config --dev-insecure --allow-remote-hydra-admin
show_last_output
expect_status 0 "auth exits 0"
expect_stdout_contains "hydra_admin_url = http://hydra-from-file:4445" "auth uses file hydra admin URL"
expect_stdout_contains "hydra_public_url = http://hydra-pub-from-file:4444" "auth uses file hydra public URL"
echo ""

echo "=== Case 2: CLI-overrides-file ==="
run_cmd control-cli-override "$CONTROL" --check-config --config "$TMPDIR/shared.toml" --dev-insecure --hydra-admin-url http://cli-override:9999
show_last_output
expect_status 0 "control CLI override exits 0"
expect_stdout_contains "hydra_admin_url = http://cli-override:9999" "control CLI hydra admin overrides file"
expect_stdout_not_contains "hydra_admin_url = http://hydra-from-file:4445" "control stdout omits file hydra admin after CLI override"
echo ""

echo "=== Case 3: guard-fires-from-file ==="
run_cmd auth-remote-guard "$AUTH" --check-config --config "$TMPDIR/remote-hydra.toml" --db-url postgres://check-config --dev-insecure
show_last_output
expect_nonzero "auth rejects file-supplied remote Hydra admin without allow flag"
echo ""

echo "=== Case 4: bad-filter-tolerant ==="
run_cmd control-bad-filter "$CONTROL" --check-config --config "$TMPDIR/bad-filter.toml" --dev-insecure
show_last_output
expect_status 0 "control tolerates invalid observability filter"
expect_stderr_contains "invalid tracing filter" "control warns about invalid observability filter"
echo ""

echo "=== Case 5: config-source ==="
# $TMPDIR is an absolute path (mktemp -d), so shared.toml is an absolute path.
SHARED_ABS="$TMPDIR/shared.toml"
run_cmd control-source "$CONTROL" --check-config --config "$SHARED_ABS" --dev-insecure
show_last_output
expect_status 0 "control config-source exits 0"
expect_stdout_contains "config_source = $SHARED_ABS" "control reports explicit config_source path"
expect_stdout_not_contains "(auto-discovered)" "control explicit source is not marked auto-discovered"
echo ""

run_cmd gateway-source "$GATEWAY" --check-config --config "$SHARED_ABS" --dev-insecure
show_last_output
expect_status 0 "gateway config-source exits 0"
expect_stdout_contains "config_source = $SHARED_ABS" "gateway reports explicit config_source path"
expect_stdout_not_contains "(auto-discovered)" "gateway explicit source is not marked auto-discovered"
echo ""

echo "=== Case 6: discovery-absent (guarded; never writes to /etc) ==="
if [ ! -e /etc/zeroship/zeroship.toml ]; then
    run_cmd control-no-config "$CONTROL" --check-config --dev-insecure
    show_last_output
    expect_status 0 "control with no --config exits 0"
    expect_stdout_contains "config_source = (none)" "control reports no overlay when well-known path absent"
else
    echo "SKIP: /etc/zeroship/zeroship.toml exists; cannot assert discovery-absent without touching /etc"
fi
echo ""

echo "============================================"
echo "Summary: $PASS passed, $FAIL failed"
echo "============================================"

if [ "$FAIL" -ne 0 ]; then
    exit 1
fi
