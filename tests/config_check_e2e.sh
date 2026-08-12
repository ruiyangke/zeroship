#!/usr/bin/env bash
# End-to-end config dry-run test for real web binaries.
#
# This intentionally runs the compiled binaries against real --config TOML
# files. --check-config exits before DB/server startup, so no services
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

# An ABSENCE assertion over EMPTY output proves nothing: every needle is absent
# from an empty file. Measured 2026-08-12 -- with `$BIN` pointing at a directory
# with no binaries, all three call sites below passed, because the command never
# produced a byte of stdout. Requiring stdout to be non-empty is what makes the
# absence meaningful; it is the cheapest possible positive pair.
expect_stdout_not_contains() {
    local needle="$1"
    local label="$2"

    if [ ! -s "$LAST_STDOUT" ]; then
        fail "$label (stdout was EMPTY - an absence assertion over no output proves nothing)"
    elif grep -Fq "$needle" "$LAST_STDOUT"; then
        fail "$label (unexpected $needle)"
    else
        pass "$label"
    fi
}

# The strong form of expect_nonzero: the command must fail AND must say WHY.
#
# WHY THIS EXISTS. `expect_nonzero` alone cannot tell "the binary refused this
# config" from "the binary never ran". A missing executable, a renamed flag and
# a startup panic all exit non-zero, so every "control rejects <bad config>"
# case stayed green while testing nothing -- measured, four of them, with `$BIN`
# pointing nowhere. Each of these cases already prints a precise diagnostic, so
# asserting on it costs nothing and closes the gap. Same class as #275/#297.
expect_rejected() {
    local needle="$1"
    local label="$2"

    if [ "$LAST_STATUS" -eq 0 ]; then
        fail "$label (expected non-zero status)"
    elif ! grep -Fq "$needle" "$LAST_STDERR" && ! grep -Fq "$needle" "$LAST_STDOUT"; then
        fail "$label (exited $LAST_STATUS but never said '$needle' - did it reject the config, or just fail to run?)"
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
platform_issuer = "http://platform-from-file.test/oauth2"
control_url = "http://control-from-file:9090"
trusted_oauth_clients = ["zeroship-builder", "zeroship-console"]

[observability]
rust_log = "info,zeroship_=debug"
log_format = "json"
TOML

cat >"$TMPDIR/bad-filter.toml" <<'TOML'
[observability]
rust_log = '!!!not a valid filter!!!'
TOML

# A [secrets] overlay whose master_key is a well-formed env REFERENCE. The
# referenced var is deliberately left UNSET: --check-config validates the
# reference FORMAT only and must NOT read the env, so exit 0 proves no fetch.
cat >"$TMPDIR/secrets-ref.toml" <<'TOML'
[secrets]
master_key = "urn:zeroship:env:E2E_MASTER"
TOML

# A [secrets] overlay whose master_key is a LITERAL. The config file must never
# carry a plaintext secret, so obtain_secret must reject this (file-must-be-a-
# reference) — exit non-zero even under --check-config.
cat >"$TMPDIR/secrets-literal.toml" <<'TOML'
[secrets]
master_key = "plainsecret"
TOML

GATEWAY_BROKER_SECRET_FILE="$TMPDIR/gateway-broker-secret"
printf '%s' "gateway-broker-secret-32-bytes-minimum-ok" >"$GATEWAY_BROKER_SECRET_FILE"
chmod 0600 "$GATEWAY_BROKER_SECRET_FILE"

echo "============================================"
echo "  zeroship config --check-config E2E"
echo "============================================"
echo ""

echo "=== Build ==="
if cargo build -p zeroship-control -p zeroship-gateway -p zeroship-auth >"$TMPDIR/build.log" 2>&1; then
    pass "built debug web binaries"
else
    fail "built debug web binaries"
    sed 's/^/  /' "$TMPDIR/build.log"
    exit 1
fi
echo ""

CONTROL="$BIN/zeroship-control"
GATEWAY="$BIN/zeroship-gate"
AUTH="$BIN/zeroship-auth"

# --check-config exercises the same mandatory guards as real startup. Supply
# real, strong inputs so these cases vary only the setting each one names.
STRONG_HEX="0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
CONTROL_COMMON=(
    --control-key "$STRONG_HEX"
    --worker-key "$STRONG_HEX"
    --signing-key-file "$TMPDIR/control-signing.pem"
    --pairwise-salt "$STRONG_HEX"
)
CONTROL_MASTER=(--master-key "$STRONG_HEX")
GATEWAY_COMMON=(
    --control-key "$STRONG_HEX"
    --worker-key "$STRONG_HEX"
    --stash-signing-key "$STRONG_HEX"
    --pairwise-salt "$STRONG_HEX"
    --gateway-broker-secret-file "$GATEWAY_BROKER_SECRET_FILE"
)
AUTH_COMMON=(
    --db-url postgres://check-config
    --stash-signing-key "$STRONG_HEX"
    --totp-enc-key "$STRONG_HEX"
)

echo "=== Case 1: overlay-applied ==="
run_cmd control-overlay "$CONTROL" --check-config --config "$TMPDIR/shared.toml" \
    "${CONTROL_COMMON[@]}" "${CONTROL_MASTER[@]}"
show_last_output
expect_status 0 "control exits 0"
expect_stdout_contains "auth_platform_issuer = http://platform-from-file.test/oauth2" "control uses file platform issuer"
expect_stdout_contains "trusted_oauth_clients_count = 2" "control reports trusted client count 2"
echo ""

run_cmd gateway-overlay "$GATEWAY" --check-config --config "$TMPDIR/shared.toml" \
    "${GATEWAY_COMMON[@]}"
show_last_output
expect_status 0 "gateway exits 0"
expect_stdout_contains "log_format = json" "gateway uses file observability log format"
echo ""

run_cmd auth-overlay "$AUTH" --check-config --config "$TMPDIR/shared.toml" \
    "${AUTH_COMMON[@]}"
show_last_output
expect_status 0 "auth exits 0"
expect_stdout_contains "control_url = http://control-from-file:9090" "auth uses file control URL"
echo ""

echo "=== Case 2: CLI-overrides-file ==="
run_cmd control-cli-override "$CONTROL" --check-config --config "$TMPDIR/shared.toml" \
    "${CONTROL_COMMON[@]}" "${CONTROL_MASTER[@]}" \
    --auth-platform-issuer http://platform-cli-override.test/oauth2
show_last_output
expect_status 0 "control CLI override exits 0"
expect_stdout_contains "auth_platform_issuer = http://platform-cli-override.test/oauth2" "control CLI platform issuer overrides file"
expect_stdout_not_contains "auth_platform_issuer = http://platform-from-file.test/oauth2" "control stdout omits file platform issuer after CLI override"
echo ""

echo "=== Case 3: bad-filter-tolerant + STRUCTURED warning (O3) ==="
run_cmd control-bad-filter "$CONTROL" --check-config --config "$TMPDIR/bad-filter.toml" \
    "${CONTROL_COMMON[@]}" "${CONTROL_MASTER[@]}" \
    --auth-platform-issuer http://platform.test/oauth2
show_last_output
expect_status 0 "control tolerates invalid observability filter"
# O3: the invalid-filter fallback is now a STRUCTURED tracing event on stdout (the
# platform's log sink), not a pre-tracing plaintext eprintln on stderr. Assert it is
# on stdout AND that the warning line is valid JSON (non-TTY default format).
expect_stdout_contains "invalid tracing filter" "control warns (structured) about invalid observability filter"
if awk '/invalid tracing filter/ { print; exit }' "$LAST_STDOUT" \
    | jq -e . >/dev/null 2>&1; then
    pass "invalid-filter warning is a structured JSON log line (O3), not a plaintext eprintln"
else
    fail "invalid-filter warning should be a structured JSON log line on stdout (O3)"
fi
echo ""

echo "=== Case 4: config-source ==="
# $TMPDIR is an absolute path (mktemp -d), so shared.toml is an absolute path.
SHARED_ABS="$TMPDIR/shared.toml"
run_cmd control-source "$CONTROL" --check-config --config "$SHARED_ABS" \
    "${CONTROL_COMMON[@]}" "${CONTROL_MASTER[@]}"
show_last_output
expect_status 0 "control config-source exits 0"
expect_stdout_contains "config_source = $SHARED_ABS" "control reports explicit config_source path"
expect_stdout_not_contains "(auto-discovered)" "control explicit source is not marked auto-discovered"
echo ""

run_cmd gateway-source "$GATEWAY" --check-config --config "$SHARED_ABS" \
    "${GATEWAY_COMMON[@]}"
show_last_output
expect_status 0 "gateway config-source exits 0"
expect_stdout_contains "config_source = $SHARED_ABS" "gateway reports explicit config_source path"
expect_stdout_not_contains "(auto-discovered)" "gateway explicit source is not marked auto-discovered"
echo ""

echo "=== Case 5: discovery-absent (guarded; never writes to /etc) ==="
if [ ! -e /etc/zeroship/zeroship.toml ]; then
    run_cmd control-no-config "$CONTROL" --check-config \
        "${CONTROL_COMMON[@]}" "${CONTROL_MASTER[@]}" \
        --auth-platform-issuer http://platform.test/oauth2
    show_last_output
    expect_status 0 "control with no --config exits 0"
    expect_stdout_contains "config_source = (none)" "control reports no overlay when well-known path absent"
else
    echo "SKIP: /etc/zeroship/zeroship.toml exists; cannot assert discovery-absent without touching /etc"
fi
echo ""

echo "=== Case 6: DSN secrets not leaked in --help (S2 / NEW-1) ==="
HELP_OUT="$TMPDIR/control-help.txt"
env -i PATH="$PATH" HOME="${HOME:-}" \
    DATABASE_URL="postgres://u:SUPERSECRETPW1@h/d" \
    AUTH_DB_URL="postgres://u:SUPERSECRETPW2@h/d" \
    "$CONTROL" --help >"$HELP_OUT" 2>&1 || true
# Same vacuous-absence trap as expect_stdout_not_contains: if `--help` produced
# nothing at all (missing binary, renamed flag, panic), "SUPERSECRETPW is not in
# the output" is trivially true. Require the help text to actually BE help text
# before believing the secret is absent from it.
if [ ! -s "$HELP_OUT" ]; then
    fail "control --help produced NO output - cannot conclude anything about secret redaction"
elif ! grep -Fq -- "--db" "$HELP_OUT"; then
    fail "control --help output does not mention --db - not the help text this assertion assumes"
elif grep -Fq "SUPERSECRETPW" "$HELP_OUT"; then
    fail "control --help must not print DSN env secrets (hide_env_values on --db AND --auth-db)"
else
    pass "control --help hides DSN env secrets"
fi
echo ""

echo "=== Case 7: invalid --check-config-format rejected (NEW-2) ==="
run_cmd control-bad-format "$CONTROL" --check-config --check-config-format xml \
    "${CONTROL_COMMON[@]}" "${CONTROL_MASTER[@]}" \
    --auth-platform-issuer http://platform.test/oauth2
show_last_output
expect_rejected "invalid value 'xml' for '--check-config-format" \
    "control rejects an unknown --check-config-format value (no silent text fallback)"
echo ""

echo "=== Case 8: MASTER_KEY secret-ref FORMAT validated, NOT fetched ==="
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
    "$CONTROL" --check-config --config "$TMPDIR/shared.toml" \
    "${CONTROL_COMMON[@]}" \
    >"$LAST_STDOUT" 2>"$LAST_STDERR"
LAST_STATUS=$?
set -e
show_last_output
expect_status 0 "control accepts a well-formed MASTER_KEY file ref under --check-config without reading the (nonexistent) file"
echo ""

echo "=== Case 9: malformed MASTER_KEY secret-ref rejected ==="
LAST_STDOUT="$TMPDIR/control-bad-ref.stdout"
LAST_STDERR="$TMPDIR/control-bad-ref.stderr"
set +e
env -i PATH="$PATH" HOME="${HOME:-}" \
    MASTER_KEY="urn:zeroship:bogus:x" \
    "$CONTROL" --check-config --config "$TMPDIR/shared.toml" \
    "${CONTROL_COMMON[@]}" \
    >"$LAST_STDOUT" 2>"$LAST_STDERR"
LAST_STATUS=$?
set -e
show_last_output
expect_rejected "malformed secret reference" \
    "control rejects a malformed MASTER_KEY secret reference under --check-config"
echo ""

echo "=== Case 10: [secrets] file tier with a REFERENCE validates (no fetch) ==="
# master_key comes from the [secrets] overlay as urn:zeroship:env:E2E_MASTER.
# The env var is intentionally absent (env -i wipes the environment), so exit 0
# can only be reached if --check-config validates the FORMAT and never reads it.
run_cmd control-secrets-file-ref "$CONTROL" --check-config --config "$TMPDIR/secrets-ref.toml" \
    "${CONTROL_COMMON[@]}" --auth-platform-issuer http://platform.test/oauth2
show_last_output
expect_status 0 "control accepts a [secrets] master_key env reference under --check-config without resolving it"
echo ""

echo "=== Case 11: [secrets] file tier with a LITERAL rejected (file must be a reference) ==="
# A plaintext literal in [secrets] is a configuration error: the file must never
# carry a secret value, only a urn:/arn: reference. obtain_secret rejects it.
run_cmd control-secrets-file-literal "$CONTROL" --check-config --config "$TMPDIR/secrets-literal.toml" \
    "${CONTROL_COMMON[@]}" --auth-platform-issuer http://platform.test/oauth2
show_last_output
expect_rejected "must be a urn:/arn: reference, not a literal value" \
    "control rejects a literal master_key in the [secrets] file (must be a urn:/arn: reference)"
echo ""

echo "=== Case 12: LEGACY_MASTER_KEYS comma-list is resolved PER ENTRY ==="
# A comma-list where the 2nd entry is a malformed reference must be rejected. This
# proves per-entry handling: the list is split FIRST, then each entry validated.
# (A whole-CSV-as-one-reference bug would parse the 1st scheme and wrongly accept.)
LAST_STDOUT="$TMPDIR/control-legacy.stdout"
LAST_STDERR="$TMPDIR/control-legacy.stderr"
set +e
env -i PATH="$PATH" HOME="${HOME:-}" \
    LEGACY_MASTER_KEYS="urn:zeroship:env:LEGACY_A,urn:zeroship:bogus:x" \
    "$CONTROL" --check-config --config "$TMPDIR/shared.toml" \
    "${CONTROL_COMMON[@]}" "${CONTROL_MASTER[@]}" \
    >"$LAST_STDOUT" 2>"$LAST_STDERR"
LAST_STATUS=$?
set -e
show_last_output
expect_rejected "LEGACY_MASTER_KEYS / --legacy-master-keys: malformed secret reference" \
    "control rejects a malformed per-entry reference in a LEGACY_MASTER_KEYS comma-list"
echo ""

echo "============================================"
echo "Summary: $PASS passed, $FAIL failed"
echo "============================================"

if [ "$FAIL" -ne 0 ]; then
    exit 1
fi

# MINIMUM-PASSED FLOOR. Without this the verdict is only "nothing failed", so a
# run that asserted NOTHING would print "0 passed, 0 failed" and exit 0 -- the
# shape already fixed in #279, #285, #294 and #316. The number is MEASURED, not
# chosen: a full run on 2026-08-12 reported exactly 29.
#
# Raise it when you add cases. If it trips after you deleted a case on purpose,
# lower it deliberately and say so in the commit -- do not delete the check.
CONFIG_CHECK_MIN_PASSED="${CONFIG_CHECK_MIN_PASSED:-29}"
if [ "$PASS" -lt "$CONFIG_CHECK_MIN_PASSED" ]; then
    echo "" >&2
    echo "FLOOR: only $PASS assertions passed, expected at least $CONFIG_CHECK_MIN_PASSED." >&2
    echo "  Nothing FAILED, so this is not a broken assertion - it is MISSING ones." >&2
    echo "  Something skipped a whole section, or a case stopped running silently." >&2
    exit 1
fi
