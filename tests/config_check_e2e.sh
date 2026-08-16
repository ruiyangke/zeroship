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
# `control_url` is a SHARED identity (auth, gateway, worker), so it sits at the
# overlay root rather than inside any one binary's table.
control_url = "http://control-from-file:9090"

[auth]
platform_issuer = "http://platform-from-file.test/oauth2"
trusted_oauth_clients = ["zeroship-builder", "zeroship-console"]

[observability]
log_filter = "info,zeroship_=debug"
log_format = "json"
TOML

cat >"$TMPDIR/bad-filter.toml" <<'TOML'
[observability]
log_filter = '!!!not a valid filter!!!'
TOML

# A secret at its CANONICAL PATH beside its siblings, as a file reference whose
# path deliberately does not exist. --check-config validates source policy and
# format and must NOT open it, so exit 0 is only reachable if nothing was read.
cat >"$TMPDIR/secret-file-ref.toml" <<'TOML'
[control]
master_key = "urn:zeroship:file:/no/such/zeroship/e2e/master.key"
TOML

# The same key as a LITERAL, which is now PERMITTED: the overlay may itself be a
# mounted Kubernetes Secret, and forbidding a literal by file while permitting
# one by environment had no principled basis. The prohibition moved to TRACKED
# files. Strong material, because a literal IS still strength-checked.
cat >"$TMPDIR/secret-literal.toml" <<'TOML'
[control]
master_key = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
TOML

# The same key as a DELETED reference scheme. An env-to-env alias is outside the
# supply set, so --check-config must REJECT it - source-policy validation, not
# format validation, and the one-variable partner to the file-reference case
# above: same key, same table, only the scheme differs.
cat >"$TMPDIR/secret-env-ref.toml" <<'TOML'
[control]
master_key = "urn:zeroship:env:E2E_MASTER"
TOML

# The DELETED [secrets] table itself. An overlay that still carries it must be
# told so at load, not have its credentials silently ignored.
cat >"$TMPDIR/secrets-table.toml" <<'TOML'
[secrets]
master_key = "urn:zeroship:file:/etc/zeroship/master.key"
TOML

ZEROSHIP_GATEWAY_BROKER_SECRET_FILE="$TMPDIR/gateway-broker-secret"
printf '%s' "gateway-broker-secret-32-bytes-minimum-ok" >"$ZEROSHIP_GATEWAY_BROKER_SECRET_FILE"
chmod 0600 "$ZEROSHIP_GATEWAY_BROKER_SECRET_FILE"

echo "============================================"
echo "  zeroship config --check-config E2E"
echo "============================================"
echo ""

echo "=== Build ==="
if cargo build -p zeroship-control -p zeroship-gateway -p zeroship-auth \
    -p zeroship-worker -p zeroship-migrated >"$TMPDIR/build.log" 2>&1; then
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
WORKER="$BIN/zeroship-worker"
MIGRATED="$BIN/zeroship-migrated"

# --check-config exercises the same mandatory guards as real startup. Supply
# real, strong inputs so these cases vary only the setting each one names.
STRONG_HEX="0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
CONTROL_KEY_HEX="1111111111111111111111111111111111111111111111111111111111111111"
WORKER_KEY_HEX="2222222222222222222222222222222222222222222222222222222222222222"
MINT_KEY_HEX="3333333333333333333333333333333333333333333333333333333333333333"

# Secrets are supplied by their CANONICAL ENVIRONMENT NAME, not by a value flag:
# a secret's only generated flag is `--<name>-file PATH`, so there is no value
# spelling left to pass. The environment tier is used rather than the path flag
# on purpose - an in-memory literal keeps its material under --check-config, so
# every strength guard below is still exercised by a dry run. A path-flag case
# further down covers the arm that deliberately reads nothing.
# The mint destination is mandatory whenever a platform issuer is configured,
# and every control case below configures one (the native provider already
# requires it). It is set here rather than per case for the same reason the
# four secrets are: it is a precondition of the runs, not the subject of any
# one of them. Case 18 is where it IS the subject.
CONTROL_MINT_URL="http://auth.internal.check-config.test:9092"
CONTROL_ENV=(
    env
    ZEROSHIP_AUTH_PLATFORM_MINT_KEY="$MINT_KEY_HEX"
    ZEROSHIP_CONTROL_KEY="$CONTROL_KEY_HEX"
    ZEROSHIP_WORKER_KEY="$WORKER_KEY_HEX"
    ZEROSHIP_PAIRWISE_SALT="$STRONG_HEX"
    ZEROSHIP_AUTH_PLATFORM_MINT_URL="$CONTROL_MINT_URL"
)
CONTROL_COMMON=(--signing-key-file "$TMPDIR/control-signing.pem")
# Split out so a case can vary the master key alone.
CONTROL_MASTER=(ZEROSHIP_CONTROL_MASTER_KEY="$STRONG_HEX")
CONTROL_RUN=("${CONTROL_ENV[@]}" "${CONTROL_MASTER[@]}" "$CONTROL")
CONTROL_RUN_NO_MASTER=("${CONTROL_ENV[@]}" "$CONTROL")

GATEWAY_RUN=(
    env
    ZEROSHIP_CONTROL_KEY="$CONTROL_KEY_HEX"
    ZEROSHIP_WORKER_KEY="$WORKER_KEY_HEX"
    ZEROSHIP_GATEWAY_STASH_SIGNING_KEY="$STRONG_HEX"
    ZEROSHIP_PAIRWISE_SALT="$STRONG_HEX"
    "$GATEWAY"
)
GATEWAY_COMMON=(--broker-secret-file "$ZEROSHIP_GATEWAY_BROKER_SECRET_FILE")

AUTH_RUN=(
    env
    ZEROSHIP_AUTH_DATABASE_URL=postgres://check-config
    ZEROSHIP_AUTH_PLATFORM_MINT_KEY="$MINT_KEY_HEX"
    ZEROSHIP_AUTH_STASH_SIGNING_KEY="$STRONG_HEX"
    ZEROSHIP_AUTH_TOTP_ENC_KEY="$STRONG_HEX"
    "$AUTH"
)
AUTH_COMMON=()

WORKER_RUN=(
    env
    ZEROSHIP_CONTROL_KEY="$CONTROL_KEY_HEX"
    ZEROSHIP_WORKER_KEY="$WORKER_KEY_HEX"
    "$WORKER"
)
WORKER_COMMON=()

MIGRATED_RUN=(
    env
    ZEROSHIP_CONTROL_KEY="$CONTROL_KEY_HEX"
    ZEROSHIP_MIGRATED_POLICY_SEAL_KEY="$STRONG_HEX"
    "$MIGRATED"
)
MIGRATED_COMMON=()

# Launch one named binary with its own environment prefix and its own remaining
# flags. Exists because a secret is supplied by an ENVIRONMENT NAME, which has
# to sit before the binary, so the per-binary invocation cannot be reduced to a
# string of arguments appended after it.
run_one() {
    local target="$1" label="$2"
    shift 2
    case "$target" in
        control) run_cmd "$label" "${CONTROL_RUN[@]}" "$@" "${CONTROL_COMMON[@]}" ;;
        gateway) run_cmd "$label" "${GATEWAY_RUN[@]}" "$@" "${GATEWAY_COMMON[@]}" ;;
        auth) run_cmd "$label" "${AUTH_RUN[@]}" "$@" ;;
        worker) run_cmd "$label" "${WORKER_RUN[@]}" "$@" ;;
        migrated) run_cmd "$label" "${MIGRATED_RUN[@]}" "$@" ;;
        *) fail "run_one: unknown target $target"; return 1 ;;
    esac
}

echo "=== Case 1: overlay-applied ==="
run_cmd control-overlay "${CONTROL_RUN[@]}" --check-config --config "$TMPDIR/shared.toml" \
    "${CONTROL_COMMON[@]}"
show_last_output
expect_status 0 "control exits 0"
expect_stdout_contains "auth_platform_issuer = http://platform-from-file.test/oauth2" "control uses file platform issuer"
expect_stdout_contains "trusted_oauth_clients_count = 2" "control reports trusted client count 2"
echo ""

run_cmd gateway-overlay "${GATEWAY_RUN[@]}" --check-config --config "$TMPDIR/shared.toml" \
    "${GATEWAY_COMMON[@]}"
show_last_output
expect_status 0 "gateway exits 0"
expect_stdout_contains "log_format = json" "gateway uses file observability log format"
echo ""

run_cmd auth-overlay "${AUTH_RUN[@]}" --check-config --config "$TMPDIR/shared.toml" \
    "${AUTH_COMMON[@]}"
show_last_output
expect_status 0 "auth exits 0"
expect_stdout_contains "control_url = http://control-from-file:9090" "auth uses file control URL"
echo ""

echo "=== Case 2: CLI-overrides-file ==="
run_cmd control-cli-override "${CONTROL_RUN[@]}" --check-config --config "$TMPDIR/shared.toml" \
    "${CONTROL_COMMON[@]}" \
    --auth-platform-issuer http://platform-cli-override.test/oauth2
show_last_output
expect_status 0 "control CLI override exits 0"
expect_stdout_contains "auth_platform_issuer = http://platform-cli-override.test/oauth2" "control CLI platform issuer overrides file"
expect_stdout_not_contains "auth_platform_issuer = http://platform-from-file.test/oauth2" "control stdout omits file platform issuer after CLI override"
echo ""

echo "=== Case 3: bad-filter-tolerant + STRUCTURED warning (O3) ==="
run_cmd control-bad-filter "${CONTROL_RUN[@]}" --check-config --config "$TMPDIR/bad-filter.toml" \
    "${CONTROL_COMMON[@]}" \
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
run_cmd control-source "${CONTROL_RUN[@]}" --check-config --config "$SHARED_ABS" \
    "${CONTROL_COMMON[@]}"
show_last_output
expect_status 0 "control config-source exits 0"
expect_stdout_contains "config_source = $SHARED_ABS" "control reports explicit config_source path"
expect_stdout_not_contains "(auto-discovered)" "control explicit source is not marked auto-discovered"
echo ""

run_cmd gateway-source "${GATEWAY_RUN[@]}" --check-config --config "$SHARED_ABS" \
    "${GATEWAY_COMMON[@]}"
show_last_output
expect_status 0 "gateway config-source exits 0"
expect_stdout_contains "config_source = $SHARED_ABS" "gateway reports explicit config_source path"
expect_stdout_not_contains "(auto-discovered)" "gateway explicit source is not marked auto-discovered"
echo ""

echo "=== Case 5: discovery-absent (guarded; never writes to /etc) ==="
if [ ! -e /etc/zeroship/zeroship.toml ]; then
    run_cmd control-no-config "${CONTROL_RUN[@]}" --check-config \
        "${CONTROL_COMMON[@]}" \
        --auth-platform-issuer http://platform.test/oauth2
    show_last_output
    expect_status 0 "control with no --config exits 0"
    expect_stdout_contains "config_source = (none)" "control reports no overlay when well-known path absent"
else
    echo "SKIP: /etc/zeroship/zeroship.toml exists; cannot assert discovery-absent without touching /etc"
fi
echo ""

echo "=== Case 6: DSN secrets not leaked in --help ==="
# The mechanism changed and the property did not. `--db` used to be a VALUE flag
# with `env = "DATABASE_URL"`, and clap prints an env var's current value in
# --help unless told not to, so this asserted `hide_env_values`. A secret now has
# no value flag and no clap env binding at all, so there is nothing for clap to
# print - which is a stronger guarantee reached by deletion rather than by an
# attribute somebody has to remember. Both spellings are supplied anyway, so a
# regression that reintroduced either surface would show here.
HELP_OUT="$TMPDIR/control-help.txt"
env -i PATH="$PATH" HOME="${HOME:-}" \
    ZEROSHIP_CONTROL_DATABASE_URL="postgres://u:SUPERSECRETPW1@h/d" \
    DATABASE_URL="postgres://u:SUPERSECRETPW2@h/d" \
    "$CONTROL" --help >"$HELP_OUT" 2>&1 || true
# Same vacuous-absence trap as expect_stdout_not_contains: if `--help` produced
# nothing at all (missing binary, renamed flag, panic), "SUPERSECRETPW is not in
# the output" is trivially true. Require the help text to actually BE help text
# before believing the secret is absent from it.
if [ ! -s "$HELP_OUT" ]; then
    fail "control --help produced NO output - cannot conclude anything about secret redaction"
elif ! grep -Fq -- "--database-url-file" "$HELP_OUT"; then
    fail "control --help does not mention --database-url-file - not the help text this assertion assumes"
elif grep -Fq "SUPERSECRETPW" "$HELP_OUT"; then
    fail "control --help must not print DSN env secrets"
else
    pass "control --help hides DSN env secrets"
fi
# The flag surface itself: a secret gets a PATH flag and no value flag, because
# a value flag would put the material in a world-readable argument list.
if grep -Eq -- '--db[ ,=]|--db$' "$HELP_OUT"; then
    fail "control --help still offers a flag for a secret"
else
    pass "control offers no flag"
fi
echo ""

echo "=== Case 7: invalid --check-config-format rejected (NEW-2) ==="
run_cmd control-bad-format "${CONTROL_RUN[@]}" --check-config --check-config-format xml \
    "${CONTROL_COMMON[@]}" \
    --auth-platform-issuer http://platform.test/oauth2
show_last_output
expect_rejected "invalid value 'xml' for '--check-config-format" \
    "control rejects an unknown --check-config-format value (no silent text fallback)"
echo ""

echo "=== Case 8: a secret FILE that does not exist still passes --check-config ==="
# THE PAIR. Both halves name a secret file that is absent, and they differ in
# exactly one variable: WHICH SUPPLY TIER names it. A dry run establishes the
# SOURCE of a secret and stops; it opens nothing. If --check-config ever starts
# stat-ing or reading, both halves fail, and a check that passed only when the
# file happened to exist would be doing no source-policy validation at all.
MISSING_KEYFILE="$TMPDIR/does-not-exist-master.key"
run_cmd control-missing-key-env "${CONTROL_ENV[@]}" \
    ZEROSHIP_CONTROL_MASTER_KEY="urn:zeroship:file:$MISSING_KEYFILE" \
    "$CONTROL" --check-config --config "$TMPDIR/shared.toml" \
    "${CONTROL_COMMON[@]}"
show_last_output
expect_status 0 "control accepts an absent master-key FILE REFERENCE under --check-config"
echo ""

run_cmd control-missing-key-flag "${CONTROL_RUN_NO_MASTER[@]}" --check-config \
    --config "$TMPDIR/shared.toml" "${CONTROL_COMMON[@]}" \
    --master-key-file "$MISSING_KEYFILE"
show_last_output
expect_status 0 "control accepts an absent master-key PATH FLAG under --check-config"
echo ""

# The half that proves the instrument discriminates: the SAME absent path,
# reached through the SAME tier, but on a real boot. If this also exited 0 the
# two cases above would be evidence of nothing.
run_cmd control-missing-key-boot "${CONTROL_RUN_NO_MASTER[@]}" \
    --config "$TMPDIR/shared.toml" "${CONTROL_COMMON[@]}" \
    --master-key-file "$MISSING_KEYFILE"
show_last_output
expect_nonzero "control refuses to BOOT with an absent master-key file"
echo ""

echo "=== Case 9: a secret source outside the supply set is rejected ==="
# Source-policy validation, which is the other half of what --check-config owes.
# An env-to-env alias, a Vault URN and an AWS ARN all used to PARSE: the first
# resolved a second variable, the other two returned BackendUnavailable at boot
# while passing the dry run. All three are outside the supply set now, so the
# dry run refuses them.
for bad in "urn:zeroship:env:E2E_MASTER" "urn:zeroship:vault:secret/x" \
    "arn:aws:secretsmanager:us-east-1:123:secret:x" "urn:zeroship:bogus:x"; do
    run_cmd "control-bad-source" "${CONTROL_ENV[@]}" \
        ZEROSHIP_CONTROL_MASTER_KEY="$bad" \
        "$CONTROL" --check-config --config "$TMPDIR/shared.toml" \
        "${CONTROL_COMMON[@]}"
    expect_rejected "unsupported secret source" \
        "control rejects the deleted secret source $bad under --check-config"
done
echo ""

echo "=== Case 10: a secret at its canonical overlay path, as a file reference ==="
# The secret sits under [control] beside its operational siblings; there is no
# [secrets] table. The referenced path does not exist, so exit 0 again proves
# the overlay tier is not read either.
run_cmd control-overlay-file-ref "${CONTROL_RUN_NO_MASTER[@]}" --check-config \
    --config "$TMPDIR/secret-file-ref.toml" \
    "${CONTROL_COMMON[@]}" --auth-platform-issuer http://platform.test/oauth2
show_last_output
expect_status 0 "control accepts a canonical-path file reference without resolving it"
echo ""

run_cmd control-overlay-env-ref "${CONTROL_RUN_NO_MASTER[@]}" --check-config \
    --config "$TMPDIR/secret-env-ref.toml" \
    "${CONTROL_COMMON[@]}" --auth-platform-issuer http://platform.test/oauth2
show_last_output
expect_rejected "unsupported secret source" \
    "control rejects an env-to-env reference at the canonical overlay path"
echo ""

echo "=== Case 11: a secret LITERAL in the overlay is now permitted ==="
# INVERTED on purpose. The old rule rejected a literal because the TRACKED
# deploy/ops/zeroship.toml was the only overlay anyone imagined. An overlay may
# be a mounted Kubernetes Secret, where the value never enters git and is
# RBAC-controlled, and permitting a literal by environment while forbidding one
# by mounted file had no principled basis. The prohibition moved to TRACKED
# files (Section 4.7).
run_cmd control-overlay-literal "${CONTROL_RUN_NO_MASTER[@]}" --check-config \
    --config "$TMPDIR/secret-literal.toml" \
    "${CONTROL_COMMON[@]}" --auth-platform-issuer http://platform.test/oauth2
show_last_output
expect_status 0 "control accepts a strong secret literal at its canonical overlay path"
echo ""

# The one-variable partner: same file, same key, same tier, WEAK material. A
# literal is in memory already, so a dry run still strength-checks it - which is
# what stops "literals are permitted" from meaning "literals are unchecked".
cat >"$TMPDIR/secret-weak-literal.toml" <<'TOML'
[control]
master_key = "short"
TOML
run_cmd control-overlay-weak-literal "${CONTROL_RUN_NO_MASTER[@]}" --check-config \
    --config "$TMPDIR/secret-weak-literal.toml" \
    "${CONTROL_COMMON[@]}" --auth-platform-issuer http://platform.test/oauth2
show_last_output
expect_rejected "ZEROSHIP_CONTROL_MASTER_KEY" "control still strength-checks a secret literal under --check-config"
echo ""

echo "=== Case 11b: the deleted [secrets] table is rejected, not ignored ==="
run_cmd control-secrets-table "${CONTROL_RUN[@]}" --check-config \
    --config "$TMPDIR/secrets-table.toml" \
    "${CONTROL_COMMON[@]}" --auth-platform-issuer http://platform.test/oauth2
show_last_output
expect_rejected "secrets" "control rejects an overlay that still carries the deleted [secrets] table"
echo ""

echo "=== Case 12: the legacy master keys are one secret holding a comma-list ==="
# Each entry used to be resolvable as its own reference, so the parse depended
# on whether a resolved value contained a comma. It is one secret now, split
# once - and every entry is still strength-checked, which is the property that
# would silently vanish if the split moved without the guard.
run_cmd control-legacy-strong "${CONTROL_ENV[@]}" "${CONTROL_MASTER[@]}" \
    ZEROSHIP_CONTROL_LEGACY_MASTER_KEYS="$STRONG_HEX,$STRONG_HEX" \
    "$CONTROL" --check-config --config "$TMPDIR/shared.toml" \
    "${CONTROL_COMMON[@]}"
show_last_output
expect_status 0 "control accepts a comma-list of strong legacy master keys"
echo ""

run_cmd control-legacy-weak "${CONTROL_ENV[@]}" "${CONTROL_MASTER[@]}" \
    ZEROSHIP_CONTROL_LEGACY_MASTER_KEYS="$STRONG_HEX,short" \
    "$CONTROL" --check-config --config "$TMPDIR/shared.toml" \
    "${CONTROL_COMMON[@]}"
show_last_output
expect_rejected "ZEROSHIP_CONTROL_LEGACY_MASTER_KEYS[1]" \
    "control rejects a weak entry inside the legacy master-key list"
echo ""

echo "=== Case 12b: a report prints presence, never material ==="
# A KNOWN SENTINEL is supplied as the master key, then the ENTIRE report is
# searched for it and for every prefix of it down to 8 characters. A report that
# printed a value, a prefix of one, or a redacted-but-length-preserving form
# would fail. The length itself is checked separately: "64 characters" is a leak.
SENTINEL="a1b2c3d4e5f60718293a4b5c6d7e8f90a1b2c3d4e5f60718293a4b5c6d7e8f90"
run_cmd control-sentinel "${CONTROL_ENV[@]}" \
    ZEROSHIP_CONTROL_MASTER_KEY="$SENTINEL" \
    "$CONTROL" --check-config --config "$TMPDIR/shared.toml" \
    "${CONTROL_COMMON[@]}"
expect_status 0 "control exits 0 with the sentinel master key"
if [ ! -s "$LAST_STDOUT" ]; then
    fail "control printed NO report - an absence assertion over no output proves nothing"
else
    LEAK=""
    for width in 8 16 32 48 64; do
        PREFIX="${SENTINEL:0:$width}"
        if grep -Fq "$PREFIX" "$LAST_STDOUT" || grep -Fq "$PREFIX" "$LAST_STDERR"; then
            LEAK="$PREFIX"
            break
        fi
    done
    if [ -n "$LEAK" ]; then
        fail "the report leaked a ${#LEAK}-character prefix of the master key"
    else
        pass "the report contains no prefix of the master key"
    fi
fi
if grep -Eq "master[_-]key[^=]*= *(configured|\(unset\))" "$LAST_STDOUT" \
    || grep -Fq "pairwise_salt_configured = configured" "$LAST_STDOUT"; then
    pass "the report states secret PRESENCE"
else
    fail "the report never states secret presence - the absence check above is vacuous"
fi

echo "=== Case 13: all five server binaries answer --check-config ==="
# The coverage gap this closes: the build list and the case list above named
# only control, gateway and auth, so worker's report and migrated's brand-new
# one were never exercised by a real process.
# Each binary is launched through its own RUN array (an `env` prefix carrying
# that binary's canonical secret names, then the binary). A packed argument
# STRING cannot express that, because the environment assignments have to
# precede the binary rather than follow it.
for name in control gateway auth worker migrated; do
    run_one "$name" "$name-all-five" --check-config --config "$TMPDIR/shared.toml"
    show_last_output
    expect_status 0 "$name exits 0 under --check-config"
    expect_stdout_contains "config_source = $TMPDIR/shared.toml" \
        "$name reports the explicit overlay it was given"
    expect_stdout_contains "log_format = json" \
        "$name resolves observability.log_format from the overlay"
done
echo ""

echo "=== Case 14: --check-config-format json is machine-readable everywhere ==="
# Also the negative half of the ValueEnum conversion: an unknown format is now
# rejected by clap rather than silently falling back to text.
for name in control worker migrated; do
    run_one "$name" "$name-json" --check-config --check-config-format json \
        --config "$TMPDIR/shared.toml"
    show_last_output
    expect_status 0 "$name exits 0 with --check-config-format json"
    expect_stdout_contains '{"' "$name emits a JSON object"

    run_one "$name" "$name-bad-format" --check-config --check-config-format yaml \
        --config "$TMPDIR/shared.toml"
    show_last_output
    expect_status 2 "$name rejects an unknown --check-config-format"
done
echo ""

echo "=== Case 15: migrated's --check-config has no side effects ==="
# migrated used to create its tmp dir, dial the control DSN and bind a listener
# unconditionally. The dry run must do none of that: point --tmp-dir at a path
# that does not exist and require it STILL does not exist afterwards.
MIGRATED_TMP="$TMPDIR/migrated-must-not-exist"
run_cmd migrated-no-side-effects env \
    ZEROSHIP_CONTROL_KEY="$STRONG_HEX" \
    ZEROSHIP_MIGRATED_POLICY_SEAL_KEY="$STRONG_HEX" \
    ZEROSHIP_MIGRATED_DATABASE_URL="postgres://127.0.0.1:1/nonexistent" \
    "$MIGRATED" --check-config \
    --config "$TMPDIR/shared.toml" --tmp-dir "$MIGRATED_TMP"
show_last_output
expect_status 0 "migrated exits 0 without a reachable database"
if [ -e "$MIGRATED_TMP" ]; then
    fail "migrated --check-config created $MIGRATED_TMP"
else
    pass "migrated --check-config created no directory"
fi
echo ""

echo "=== Case 16: ONE auth-provider variable reaches BOTH auth and control ==="
# The reason the merge exists. `ZEROSHIP_AUTH_PROVIDER=native` used to be
# settable alongside `ZEROSHIP_CONTROL_AUTH_PROVIDER=supabase`: with a platform
# issuer configured control silently widened its trust to accept both issuers,
# and without one every authenticated request failed at REQUEST time. Neither
# state was reported at boot. Two processes are the only vector that can
# observe the agreement, so it is asserted here rather than in a unit test.
run_cmd control-shared-provider env ZEROSHIP_AUTH_PROVIDER=supabase \
    "${CONTROL_RUN[@]}" --check-config "${CONTROL_COMMON[@]}" \
    --auth-platform-issuer http://platform.test/oauth2
show_last_output
expect_status 0 "control exits 0 with the shared provider variable"
expect_stdout_contains "auth_provider = supabase" "control reads ZEROSHIP_AUTH_PROVIDER"
echo ""

run_cmd auth-shared-provider env ZEROSHIP_AUTH_PROVIDER=supabase \
    "${AUTH_RUN[@]}" --check-config "${AUTH_COMMON[@]}" \
    --supabase-url https://project.supabase.co --supabase-anon-key anon
show_last_output
expect_status 0 "auth exits 0 with the shared provider variable"
expect_stdout_contains "auth_provider = supabase" "auth reads the SAME ZEROSHIP_AUTH_PROVIDER"
echo ""

# The one-variable control for the pair above: with the variable UNSET both
# binaries must report the same compiled default. Without this, two binaries
# that ignored the variable and happened to default to `supabase` would pass.
run_cmd control-default-provider "${CONTROL_RUN[@]}" --check-config \
    "${CONTROL_COMMON[@]}" \
    --auth-platform-issuer http://platform.test/oauth2
show_last_output
expect_stdout_contains "auth_provider = native" "control defaults to native, not supabase"
echo ""

run_cmd auth-default-provider "${AUTH_RUN[@]}" --check-config "${AUTH_COMMON[@]}"
show_last_output
expect_stdout_contains "auth_provider = native" "auth defaults to native, not supabase"
echo ""

echo "=== Case 17: the retired control-scoped provider spellings are refused ==="
# `platform` was control's word for the state now spelled `native`, and
# `[control] auth_provider` was its overlay key. Both must be gone, not
# tolerated: a deployment that still carries either has to be told so at boot.
run_cmd control-retired-provider-value env ZEROSHIP_AUTH_PROVIDER=platform \
    "${CONTROL_RUN[@]}" --check-config "${CONTROL_COMMON[@]}" \
    --auth-platform-issuer http://platform.test/oauth2
show_last_output
expect_rejected "platform" "control rejects the retired provider value"
echo ""

cat >"$TMPDIR/retired-control-provider.toml" <<'TOML'
[control]
auth_provider = "platform"
TOML
run_cmd control-retired-provider-key "${CONTROL_RUN[@]}" --check-config \
    --config "$TMPDIR/retired-control-provider.toml" \
    "${CONTROL_COMMON[@]}" \
    --auth-platform-issuer http://platform.test/oauth2
show_last_output
expect_rejected "auth_provider" "control rejects the retired [control] auth_provider key"
echo ""

echo "=== Case 18: the platform mint destination is configured, not derived ==="
# One string used to do two jobs: the issuer was both the trust anchor a token's
# `iss` must equal AND the address control POSTed `control_key` to. On the live
# deployment those cannot be the same value - the public name resolves to a CDN
# with no route back to the host - so every `zeroship login` approval ended in
# `{"error":"internal error"}`. --check-config must now show BOTH, separately,
# because "which of these two is wrong" is the question an operator is asking
# when they read this output.
run_cmd control-mint-url "${CONTROL_RUN[@]}" --check-config "${CONTROL_COMMON[@]}" \
    --auth-platform-issuer https://auth.public.test/oauth2
show_last_output
expect_status 0 "control exits 0 with a public issuer and a separate mint URL"
expect_stdout_contains "auth_platform_issuer = https://auth.public.test/oauth2" \
    "control reports the PUBLIC issuer as the trust anchor"
expect_stdout_contains "auth_platform_mint_url = $CONTROL_MINT_URL" \
    "control reports the INTERNAL mint destination, which is not derived from the issuer"
expect_stdout_not_contains "auth_platform_mint_url = https://auth.public.test" \
    "the mint destination did not follow the issuer"
echo ""

# The refusal. An issuer with no mint destination must NOT fall back to the
# issuer's own origin - that fallback is bit-for-bit the shipped bug, and it
# would reappear on exactly the deployments that never set the new value.
run_cmd control-mint-url-missing env \
    ZEROSHIP_AUTH_PLATFORM_MINT_KEY="$MINT_KEY_HEX" \
    ZEROSHIP_CONTROL_KEY="$CONTROL_KEY_HEX" ZEROSHIP_WORKER_KEY="$WORKER_KEY_HEX" \
    ZEROSHIP_PAIRWISE_SALT="$STRONG_HEX" "${CONTROL_MASTER[@]}" \
    "$CONTROL" --check-config "${CONTROL_COMMON[@]}" \
    --auth-platform-issuer https://auth.public.test/oauth2
show_last_output
expect_rejected "ZEROSHIP_AUTH_PLATFORM_MINT_URL" \
    "control refuses an issuer with no mint destination rather than deriving one"
echo ""

# A credential destination, so the value is parsed rather than trusted. Each
# spelling below resolves somewhere other than the host it reads as.
for bad_mint in "auth:9092" "https://auth.internal@evil.example" \
    "http://auth:9092/oauth2"; do
    run_cmd control-mint-url-bad env \
        ZEROSHIP_AUTH_PLATFORM_MINT_KEY="$MINT_KEY_HEX" \
        ZEROSHIP_CONTROL_KEY="$CONTROL_KEY_HEX" ZEROSHIP_WORKER_KEY="$WORKER_KEY_HEX" \
        ZEROSHIP_PAIRWISE_SALT="$STRONG_HEX" "${CONTROL_MASTER[@]}" \
        ZEROSHIP_AUTH_PLATFORM_MINT_URL="$bad_mint" \
        "$CONTROL" --check-config "${CONTROL_COMMON[@]}" \
        --auth-platform-issuer https://auth.public.test/oauth2
    expect_rejected "ZEROSHIP_AUTH_PLATFORM_MINT_URL" \
        "control rejects the mint destination $bad_mint"
done
echo ""

echo "=== Case 18b: the platform mint credential is distinct from worker-held keys ==="
run_cmd control-mint-equals-control env \
    ZEROSHIP_AUTH_PLATFORM_MINT_KEY="$CONTROL_KEY_HEX" \
    ZEROSHIP_CONTROL_KEY="$CONTROL_KEY_HEX" ZEROSHIP_WORKER_KEY="$WORKER_KEY_HEX" \
    ZEROSHIP_PAIRWISE_SALT="$STRONG_HEX" "${CONTROL_MASTER[@]}" \
    ZEROSHIP_AUTH_PLATFORM_MINT_URL="$CONTROL_MINT_URL" \
    "$CONTROL" --check-config "${CONTROL_COMMON[@]}" \
    --auth-platform-issuer https://auth.public.test/oauth2
expect_rejected "ZEROSHIP_CONTROL_KEY" \
    "control rejects a platform mint key equal to the worker-held control key"

run_cmd control-mint-equals-worker env \
    ZEROSHIP_AUTH_PLATFORM_MINT_KEY="$WORKER_KEY_HEX" \
    ZEROSHIP_CONTROL_KEY="$CONTROL_KEY_HEX" ZEROSHIP_WORKER_KEY="$WORKER_KEY_HEX" \
    ZEROSHIP_PAIRWISE_SALT="$STRONG_HEX" "${CONTROL_MASTER[@]}" \
    ZEROSHIP_AUTH_PLATFORM_MINT_URL="$CONTROL_MINT_URL" \
    "$CONTROL" --check-config "${CONTROL_COMMON[@]}" \
    --auth-platform-issuer https://auth.public.test/oauth2
expect_rejected "ZEROSHIP_WORKER_KEY" \
    "control rejects a platform mint key equal to the worker dispatch key"
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
# chosen: a full run on 2026-08-12 reported exactly 29, and 55 after cases
# 13-15 extended coverage to worker and migrated on the same day. Cases 16-17
# (the shared auth-provider variable) took a measured run to 63.
#
# Cases 16-17 (the shared auth-provider variable) took a measured run to 63, and
# the secret conversion took it to 76: cases 8-12 became the absent-file pair,
# the deleted-source set, the canonical-path overlay tiers, the inverted literal
# rule and the presence-only sentinel.
#
# Case 18 (the platform mint destination, split from the issuer) added eight:
# four on the reported issuer/mint pair, one on the refusal when only the
# issuer is set, three on the rejected spellings. MEASURED after it landed: 84.
# Case 18b adds two process-level refusals for mint-key equality with each key a
# worker process holds, bringing the assertion floor to 86.
#
# Raise it when you add cases. If it trips after you deleted a case on purpose,
# lower it deliberately and say so in the commit -- do not delete the check.
CONFIG_CHECK_MIN_PASSED="${CONFIG_CHECK_MIN_PASSED:-86}"
if [ "$PASS" -lt "$CONFIG_CHECK_MIN_PASSED" ]; then
    echo "" >&2
    echo "FLOOR: only $PASS assertions passed, expected at least $CONFIG_CHECK_MIN_PASSED." >&2
    echo "  Nothing FAILED, so this is not a broken assertion - it is MISSING ones." >&2
    echo "  Something skipped a whole section, or a case stopped running silently." >&2
    exit 1
fi
