#!/usr/bin/env bash
# Real-browser end-to-end gate for the native zeroship auth UI.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"
PROJECT="$ROOT/tests/e2e_auth_ui"
BIN="$ROOT/target/release"
TEST_DB="zeroship_authui_test"
PG_HOST="127.0.0.1"
PG_PORT="5440"
PG_USER="postgres"
PG_PASS="zeroship"
DSN="postgres://${PG_USER}:${PG_PASS}@${PG_HOST}:${PG_PORT}/${TEST_DB}"

# A setup failure is not a test result. This library gives observed ENOSPC
# failures their own loud diagnosis instead of misreporting them as defects.
# shellcheck source=tests/lib/measurement_integrity.sh
. "$ROOT/tests/lib/measurement_integrity.sh"

WORK=""
AUTH_PID=""
PSQL="${PSQL:-}"
DB_CREATED=0
RESULT_KIND="FAIL"
RESULT_DETAIL="test run did not happen: unexpected harness exit"

step() {
  printf '\n==> %s\n' "$1"
}

die() {
  RESULT_KIND="FAIL"
  RESULT_DETAIL="test run did not happen: $1"
  printf 'FATAL: %s\n' "$1" >&2
  exit "${2:-1}"
}

fail_from_log() {
  local log="$1" what="$2"
  if log_shows_disk_full "$log"; then
    report_measurement_did_not_run "$log" "$what"
    RESULT_KIND="FAIL"
    RESULT_DETAIL="test run did not happen: out of disk space during $what"
    exit 90
  fi
  tail -80 "$log" >&2 || true
  die "$what failed; the log tail is shown above"
}

run_psql() {
  PGPASSWORD="$PG_PASS" "$PSQL" -h "$PG_HOST" -p "$PG_PORT" -U "$PG_USER" "$@"
}

cleanup() {
  local status=$? cleanup_failed=0
  trap - EXIT INT TERM
  set +e

  if [ -n "$AUTH_PID" ] && kill -0 "$AUTH_PID" 2>/dev/null; then
    kill "$AUTH_PID" 2>/dev/null || true
    for _ in $(seq 1 30); do
      kill -0 "$AUTH_PID" 2>/dev/null || break
      sleep 0.1
    done
    if kill -0 "$AUTH_PID" 2>/dev/null; then
      kill -KILL "$AUTH_PID" 2>/dev/null || true
    fi
  fi
  if [ -n "$AUTH_PID" ]; then
    wait "$AUTH_PID" 2>/dev/null || true
  fi

  if [ "$DB_CREATED" -eq 1 ] && [ -n "$PSQL" ]; then
    if ! run_psql -d postgres -v ON_ERROR_STOP=1 \
      -c "DROP DATABASE IF EXISTS ${TEST_DB} WITH (FORCE);" >/dev/null 2>&1; then
      printf 'FATAL: cleanup could not drop dedicated database %s\n' "$TEST_DB" >&2
      cleanup_failed=1
    fi
  fi

  if [ -n "$WORK" ] && [ -d "$WORK" ]; then
    case "$WORK" in
      "${TMPDIR:-/tmp}"/zeroship-auth-ui.*)
        if ! rm -rf -- "$WORK"; then
          printf 'FATAL: cleanup could not remove work directory %s\n' "$WORK" >&2
          cleanup_failed=1
        fi
        ;;
      *)
        printf 'FATAL: refusing to remove unexpected work directory %s\n' "$WORK" >&2
        cleanup_failed=1
        ;;
    esac
  fi

  if [ "$cleanup_failed" -ne 0 ]; then
    status=1
    RESULT_KIND="FAIL"
    RESULT_DETAIL="${RESULT_DETAIL}; cleanup failed"
  fi

  if [ "$status" -eq 0 ]; then
    printf '\nAUTH UI E2E PASS: %s\n' "$RESULT_DETAIL"
  else
    printf '\nAUTH UI E2E FAIL: %s\n' "$RESULT_DETAIL"
  fi
  exit "$status"
}

trap cleanup EXIT
trap 'exit 130' INT TERM

for command_name in cargo curl mktemp node pnpm playwright readlink seq; do
  command -v "$command_name" >/dev/null 2>&1 \
    || die "missing required command '$command_name'; run with nix develop"
done

PW_VERSION="$(playwright --version 2>&1 || true)"
[ "$PW_VERSION" = "Version 1.58.2" ] \
  || die "expected Playwright 1.58.2 from nix, got '${PW_VERSION:-no version}'"
[ -n "${PLAYWRIGHT_BROWSERS_PATH:-}" ] \
  || die "PLAYWRIGHT_BROWSERS_PATH is unset; run with nix develop"
[ -d "$PLAYWRIGHT_BROWSERS_PATH" ] \
  || die "PLAYWRIGHT_BROWSERS_PATH is not a directory: $PLAYWRIGHT_BROWSERS_PATH"

if [ -z "$PSQL" ]; then
  if command -v psql >/dev/null 2>&1; then
    PSQL="$(command -v psql)"
  else
    PSQL="$(ls -d /nix/store/*postgresql*/bin/psql 2>/dev/null | head -1 || true)"
  fi
fi
[ -n "$PSQL" ] && [ -x "$PSQL" ] \
  || die "no psql found; set PSQL to the Postgres client binary"

WORK="$(mktemp -d "${TMPDIR:-/tmp}/zeroship-auth-ui.XXXXXX")"
BUILD_LOG="$WORK/release-build.log"
MIGRATE_LOG="$WORK/migrate.log"
DEV_INIT_LOG="$WORK/dev-init.log"
AUTH_LOG="$WORK/auth.log"
PLAYWRIGHT_LOG="$WORK/playwright.log"
TEE_LOG="$WORK/tee.log"
RESULTS_JSON="$WORK/playwright-results.json"
SECRETS_DIR="$WORK/secrets"
ENV_FILE="$WORK/auth.env"

step "Verify the existing Postgres 16 test service"
run_psql -d postgres -v ON_ERROR_STOP=1 -tAc "select 1" >/dev/null \
  || die "Postgres is unreachable at ${PG_HOST}:${PG_PORT}"

step "Link Playwright to the Nix 1.58.2 runner"
bash "$PROJECT/scripts/link-playwright.sh" \
  || die "could not link the Nix Playwright test package"

step "Build release binaries"
# These are the three JS files embedded by the runtime dependency graph. Build
# their two owning packages every time so a present but stale dist file cannot
# enter a freshly compiled release binary. The scoped build does not touch the
# vendored N-API package under third_party.
pnpm --filter @zeroship/db build >"$BUILD_LOG" 2>&1 \
  || fail_from_log "$BUILD_LOG" "database SDK prerequisite build"
pnpm --filter @zeroship/bootstrap build >>"$BUILD_LOG" 2>&1 \
  || fail_from_log "$BUILD_LOG" "bootstrap SDK prerequisite build"
for path in \
  "$ROOT/sdks/bootstrap/dist/runtime-entry.js" \
  "$ROOT/sdks/bootstrap/dist/install-schema.js" \
  "$ROOT/sdks/db/dist/internal.js"; do
  [ -f "$path" ] || die "SDK build did not produce $path"
done
cargo build --release -p zeroship -p zeroship-auth \
  >>"$BUILD_LOG" 2>&1 \
  || fail_from_log "$BUILD_LOG" "zeroship auth release build"
cargo build --release -p zeroship-migrate-adapter \
  --features platform-cli --bin zeroship-platform-migrate \
  >>"$BUILD_LOG" 2>&1 \
  || fail_from_log "$BUILD_LOG" "platform migration release build"

for binary in zeroship zeroship-auth zeroship-platform-migrate; do
  [ -x "$BIN/$binary" ] || die "release build did not produce $BIN/$binary"
done

step "Recreate and migrate dedicated database $TEST_DB"
DB_CREATED=1
run_psql -d postgres -v ON_ERROR_STOP=1 \
  -c "DROP DATABASE IF EXISTS ${TEST_DB} WITH (FORCE);" \
  -c "CREATE DATABASE ${TEST_DB};" >"$MIGRATE_LOG" 2>&1 \
  || fail_from_log "$MIGRATE_LOG" "dedicated database provisioning"
ZEROSHIP_MIGRATE_DSN="$DSN" \
ZEROSHIP_MIGRATE_BIN="$BIN/zeroship-platform-migrate" \
  "$ROOT/deploy/ops/db-migrate.sh" >>"$MIGRATE_LOG" 2>&1 \
  || fail_from_log "$MIGRATE_LOG" "platform schema migration"
run_psql -d "$TEST_DB" -v ON_ERROR_STOP=1 -tAc "select 1" >/dev/null \
  || die "migrated database $TEST_DB is unreachable"

step "Generate isolated auth secrets"
"$BIN/zeroship" dev init \
  --secrets-dir="$SECRETS_DIR" \
  --env-file="$ENV_FILE" >"$DEV_INIT_LOG" 2>&1 \
  || fail_from_log "$DEV_INIT_LOG" "zeroship dev init"
set -a
# The generated file contains only validated NAME=hex assignments.
. "$ENV_FILE"
set +a
: "${ZEROSHIP_CONTROL_KEY:?zeroship dev init omitted ZEROSHIP_CONTROL_KEY}"
: "${ZEROSHIP_AUTH_PLATFORM_MINT_KEY:?zeroship dev init omitted the platform mint key}"
: "${ZEROSHIP_AUTH_STASH_SIGNING_KEY:?zeroship dev init omitted the auth stash key}"
: "${ZEROSHIP_AUTH_TOTP_ENC_KEY:?zeroship dev init omitted the auth TOTP key}"
# Keep the mint credential out of the Node and Playwright children below. Auth
# receives it explicitly at its launch boundary.
export -n ZEROSHIP_AUTH_PLATFORM_MINT_KEY

AUTH_PORT="$(node -e '
const net = require("node:net");
const server = net.createServer();
server.listen(0, "127.0.0.1", () => {
  process.stdout.write(String(server.address().port));
  server.close();
});
')"
[ -n "$AUTH_PORT" ] || die "could not select a free auth port"
BASE_URL="http://127.0.0.1:$AUTH_PORT"

step "Boot the real native zeroship-auth binary on $BASE_URL"
unset ZEROSHIP_CONFIG || true
export ZEROSHIP_AUTH_DATABASE_URL="$DSN"
(
  export ZEROSHIP_AUTH_PLATFORM_MINT_KEY
  exec "$BIN/zeroship-auth" \
    --no-config \
    --provider native \
    --addr "127.0.0.1:$AUTH_PORT" \
    --public-url "$BASE_URL" \
    --signing-key-file "$SECRETS_DIR/auth-signing.pem" \
    --pairwise-salt-file "$SECRETS_DIR/pairwise-salt" \
    --broker-secret-file "$SECRETS_DIR/broker-secret" \
    --refresh-hash-key-file "$SECRETS_DIR/refresh-hash-key" \
    --refresh-idem-key-file "$SECRETS_DIR/refresh-idem-key" \
    --mailer stdout \
    --relay-forward-mailer stdout
) >"$AUTH_LOG" 2>&1 &
AUTH_PID=$!

ready=0
for _ in $(seq 1 60); do
  if ! kill -0 "$AUTH_PID" 2>/dev/null; then
    tail -80 "$AUTH_LOG" >&2 || true
    die "zeroship-auth exited before becoming ready"
  fi
  if curl -fsS --connect-timeout 1 --max-time 1 "$BASE_URL/readyz" >/dev/null 2>&1; then
    ready=1
    break
  fi
  sleep 0.5
done
if [ "$ready" -ne 1 ]; then
  tail -80 "$AUTH_LOG" >&2 || true
  die "zeroship-auth did not become ready within the bounded readiness window"
fi

step "Run Chromium auth UI specs"
export ZEROSHIP_AUTH_UI_BASE_URL="$BASE_URL"
export ZEROSHIP_AUTH_UI_AUTH_LOG="$AUTH_LOG"
export ZEROSHIP_AUTH_UI_RESULTS_JSON="$RESULTS_JSON"
set +e
(
  cd "$PROJECT"
  playwright test
) 2>&1 | tee "$PLAYWRIGHT_LOG" 2>"$TEE_LOG"
pipeline_status=("${PIPESTATUS[@]}")
playwright_status=${pipeline_status[0]}
tee_status=${pipeline_status[1]}
set -e

if [ "$tee_status" -ne 0 ]; then
  cat "$TEE_LOG" >&2 || true
  if log_shows_disk_full "$TEE_LOG"; then
    report_measurement_did_not_run "$TEE_LOG" "Playwright output capture"
    RESULT_DETAIL="test run did not happen: out of disk space during Playwright output capture"
    exit 90
  fi
  die "could not capture Playwright output (tee exited $tee_status)"
fi
if log_shows_disk_full "$PLAYWRIGHT_LOG"; then
  report_measurement_did_not_run "$PLAYWRIGHT_LOG" "the auth UI browser E2E"
  RESULT_KIND="FAIL"
  RESULT_DETAIL="test run did not happen: out of disk space during Playwright"
  exit 90
fi
[ -s "$RESULTS_JSON" ] \
  || die "Playwright produced no JSON results; browser measurement did not complete"

counts="$(node -e '
const fs = require("node:fs");
const report = JSON.parse(fs.readFileSync(process.argv[1], "utf8"));
const stats = report.stats || {};
for (const key of ["expected", "unexpected", "skipped"]) {
  if (!Number.isInteger(stats[key])) throw new Error(`missing integer stats.${key}`);
}
let expectedFailures = 0;
const discoveredTitles = new Set();
const visitSuite = (suite) => {
  for (const spec of suite.specs || []) {
    discoveredTitles.add(spec.title);
    for (const test of spec.tests || []) {
      const result = test.results?.[test.results.length - 1];
      if (test.expectedStatus === "failed" && result?.status === "failed") {
        expectedFailures += 1;
      }
    }
  }
  for (const child of suite.suites || []) visitSuite(child);
};
for (const suite of report.suites || []) visitSuite(suite);
for (const requiredTitle of [
  "login applies its layout without style-src violations",
  "failed login error has screen-reader announcement semantics",
  "failed login fields identify and describe their errors",
  "public auth pages name controls and use a sane heading order",
  "public auth pages show a focus indicator on every interactive element",
  "creator journey covers signup, verification, login, profile, logout, and defenses",
]) {
  if (!discoveredTitles.has(requiredTitle)) {
    throw new Error(`required browser test was not discovered: ${requiredTitle}`);
  }
}
process.stdout.write(`${stats.expected - expectedFailures} ${stats.unexpected} ${stats.skipped} ${expectedFailures}`);
' "$RESULTS_JSON")" || die "could not parse Playwright result counts"
read -r passed failed skipped expected_failures <<<"$counts"
total=$((passed + failed + skipped + expected_failures))
[ "$total" -gt 0 ] || die "Playwright executed zero tests"

RESULT_DETAIL="$passed passed, $failed failed, $skipped skipped, $expected_failures expected failures"
if [ "$skipped" -ne 0 ]; then
  RESULT_KIND="FAIL"
  RESULT_DETAIL="$RESULT_DETAIL; skipped browser tests are forbidden"
  exit 1
fi
if [ "$playwright_status" -ne 0 ] || [ "$failed" -ne 0 ]; then
  RESULT_KIND="FAIL"
  exit 1
fi

RESULT_KIND="PASS"
exit 0
