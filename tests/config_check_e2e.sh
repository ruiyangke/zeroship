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

# A [secrets] overlay whose master_key is a well-formed env REFERENCE. The
# referenced var is deliberately left UNSET: --check-config validates the
# reference FORMAT only and must NOT read the env, so exit 0 proves no fetch.
cat >"$TMPDIR/secrets-ref.toml" <<'TOML'
[auth]
hydra_admin_url = "http://hydra-from-file:4445"

[secrets]
master_key = "urn:zeroship:env:E2E_MASTER"
TOML

# A [secrets] overlay whose master_key is a LITERAL. The config file must never
# carry a plaintext secret, so obtain_secret must reject this (file-must-be-a-
# reference) — exit non-zero even under --check-config.
cat >"$TMPDIR/secrets-literal.toml" <<'TOML'
[auth]
hydra_admin_url = "http://hydra-from-file:4445"

[secrets]
master_key = "plainsecret"
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
run_cmd control-overlay "$CONTROL" --check-config --config "$TMPDIR/shared.toml" --dev-insecure --allow-remote-hydra-admin
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
run_cmd control-cli-override "$CONTROL" --check-config --config "$TMPDIR/shared.toml" --dev-insecure --allow-remote-hydra-admin --hydra-admin-url http://cli-override:9999
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

# Symmetric guard on control: dev mode does NOT bypass the remote-admin opt-in.
run_cmd control-remote-guard "$CONTROL" --check-config --config "$TMPDIR/remote-hydra.toml" --dev-insecure
show_last_output
expect_nonzero "control rejects file-supplied remote Hydra admin without allow flag"
echo ""

echo "=== Case 4: bad-filter-tolerant + STRUCTURED warning (O3) ==="
run_cmd control-bad-filter "$CONTROL" --check-config --config "$TMPDIR/bad-filter.toml" --dev-insecure
show_last_output
expect_status 0 "control tolerates invalid observability filter"
# O3: the invalid-filter fallback is now a STRUCTURED tracing event on stdout (the
# platform's log sink), not a pre-tracing plaintext eprintln on stderr. Assert it is
# on stdout AND that the warning line is valid JSON (non-TTY default format).
expect_stdout_contains "invalid tracing filter" "control warns (structured) about invalid observability filter"
if grep -F 'invalid tracing filter' "$LAST_STDOUT" | head -1 \
    | python3 -c "import json,sys; json.loads(sys.stdin.readline()); " 2>/dev/null; then
    pass "invalid-filter warning is a structured JSON log line (O3), not a plaintext eprintln"
else
    fail "invalid-filter warning should be a structured JSON log line on stdout (O3)"
fi
echo ""

echo "=== Case 5: config-source ==="
# $TMPDIR is an absolute path (mktemp -d), so shared.toml is an absolute path.
SHARED_ABS="$TMPDIR/shared.toml"
run_cmd control-source "$CONTROL" --check-config --config "$SHARED_ABS" --dev-insecure --allow-remote-hydra-admin
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

echo "=== Case 7: DSN secrets not leaked in --help (S2 / NEW-1) ==="
HELP_OUT="$TMPDIR/control-help.txt"
env -i PATH="$PATH" HOME="${HOME:-}" \
    DATABASE_URL="postgres://u:SUPERSECRETPW1@h/d" \
    AUTH_DB_URL="postgres://u:SUPERSECRETPW2@h/d" \
    "$CONTROL" --help >"$HELP_OUT" 2>&1 || true
if grep -Fq "SUPERSECRETPW" "$HELP_OUT"; then
    fail "control --help must not print DSN env secrets (hide_env_values on --db AND --auth-db)"
else
    pass "control --help hides DSN env secrets"
fi
echo ""

echo "=== Case 8: invalid --check-config-format rejected (NEW-2) ==="
run_cmd control-bad-format "$CONTROL" --check-config --check-config-format xml --dev-insecure
show_last_output
expect_nonzero "control rejects an unknown --check-config-format value (no silent text fallback)"
echo ""

echo "=== Case 9: MASTER_KEY secret-ref FORMAT validated, NOT fetched ==="
# A well-formed urn:zeroship:file:<path> ref must pass --check-config with exit 0
# because validate is FORMAT-only: it must NOT read the file. To prove no-fetch
# we point the ref at a NONEXISTENT path AND seed the file with strong material
# so that, were check-config to dereference it, the strength check would still
# pass — but the path does not exist, so the only way exit 0 is reachable is if
# the file is never opened. (The file content is incidental; the path is dead.)
KEYFILE="$TMPDIR/master.key"
# 48 chars of fake-but-strong key material (well over the 32-char minimum).
printf '%s' "this-is-a-fake-but-strong-master-key-1234567890" >"$KEYFILE"
MISSING_KEYFILE="$TMPDIR/does-not-exist-master.key"
LAST_STDOUT="$TMPDIR/control-secret-ref.stdout"
LAST_STDERR="$TMPDIR/control-secret-ref.stderr"
set +e
env -i PATH="$PATH" HOME="${HOME:-}" \
    MASTER_KEY="urn:zeroship:file:$MISSING_KEYFILE" \
    "$CONTROL" --check-config --config "$TMPDIR/shared.toml" --dev-insecure --allow-remote-hydra-admin \
    >"$LAST_STDOUT" 2>"$LAST_STDERR"
LAST_STATUS=$?
set -e
show_last_output
expect_status 0 "control accepts a well-formed MASTER_KEY file ref under --check-config without reading the (nonexistent) file"
echo ""

echo "=== Case 10: malformed MASTER_KEY secret-ref rejected ==="
LAST_STDOUT="$TMPDIR/control-bad-ref.stdout"
LAST_STDERR="$TMPDIR/control-bad-ref.stderr"
set +e
env -i PATH="$PATH" HOME="${HOME:-}" \
    MASTER_KEY="urn:zeroship:bogus:x" \
    "$CONTROL" --check-config --config "$TMPDIR/shared.toml" --dev-insecure --allow-remote-hydra-admin \
    >"$LAST_STDOUT" 2>"$LAST_STDERR"
LAST_STATUS=$?
set -e
show_last_output
expect_nonzero "control rejects a malformed MASTER_KEY secret reference under --check-config"
echo ""

echo "=== Case 11: [secrets] file tier with a REFERENCE validates (no fetch) ==="
# master_key comes from the [secrets] overlay as urn:zeroship:env:E2E_MASTER.
# The env var is intentionally absent (env -i wipes the environment), so exit 0
# can only be reached if --check-config validates the FORMAT and never reads it.
run_cmd control-secrets-file-ref "$CONTROL" --check-config --config "$TMPDIR/secrets-ref.toml" --dev-insecure --allow-remote-hydra-admin
show_last_output
expect_status 0 "control accepts a [secrets] master_key env reference under --check-config without resolving it"
echo ""

echo "=== Case 12: [secrets] file tier with a LITERAL rejected (file must be a reference) ==="
# A plaintext literal in [secrets] is a configuration error: the file must never
# carry a secret value, only a urn:/arn: reference. obtain_secret rejects it.
run_cmd control-secrets-file-literal "$CONTROL" --check-config --config "$TMPDIR/secrets-literal.toml" --dev-insecure --allow-remote-hydra-admin
show_last_output
expect_nonzero "control rejects a literal master_key in the [secrets] file (must be a urn:/arn: reference)"
echo ""

echo "=== Case 13: LEGACY_MASTER_KEYS comma-list is resolved PER ENTRY ==="
# A comma-list where the 2nd entry is a malformed reference must be rejected. This
# proves per-entry handling: the list is split FIRST, then each entry validated.
# (A whole-CSV-as-one-reference bug would parse the 1st scheme and wrongly accept.)
LAST_STDOUT="$TMPDIR/control-legacy.stdout"
LAST_STDERR="$TMPDIR/control-legacy.stderr"
set +e
env -i PATH="$PATH" HOME="${HOME:-}" \
    LEGACY_MASTER_KEYS="urn:zeroship:env:LEGACY_A,urn:zeroship:bogus:x" \
    "$CONTROL" --check-config --config "$TMPDIR/shared.toml" --dev-insecure --allow-remote-hydra-admin \
    >"$LAST_STDOUT" 2>"$LAST_STDERR"
LAST_STATUS=$?
set -e
show_last_output
expect_nonzero "control rejects a malformed per-entry reference in a LEGACY_MASTER_KEYS comma-list"
echo ""

echo "============================================"
echo "Summary: $PASS passed, $FAIL failed"
echo "============================================"

if [ "$FAIL" -ne 0 ]; then
    exit 1
fi
