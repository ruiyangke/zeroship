# shellcheck shell=bash
# ============================================================================
# test_config.sh - the shell half of the test overlay.
#
# WHAT THIS REPLACES
# ------------------
# Every suite carried its own copy of the test backends' coordinates:
#
#   tests/run_auth_suite.sh:65-68     PG_HOST/PG_PORT/PG_USER/PG_PASS
#   tests/run_billing_suite.sh:124-127  the same four lines again
#
# and then exported the resulting DSN under whichever names the crates it ran
# happened to read - AUTH_DB_URL, CONTROL_TEST_DB, MIGRATED_TEST_DB,
# GATEWAY_ANCHORS_DB_URL, GATEWAY_POOL_SMOKE_URL. Two copies of the address
# drift; five names for one value means a crate whose name nobody exported runs
# against nothing and reports passes.
#
# `tests/provision_test_backends.sh` now writes `deploy/ops/zeroship.test.toml`
# in the platform's own config schema, and this file reads it. Rust test code
# reads the same document through `zeroship_core::config::test_overlay`, which
# parses it with `FileConfig` under `deny_unknown_fields`.
#
# WHY AWK AND NOT THE REAL PARSER. A harness cannot afford a cargo build to
# learn a hostname - `tests/lib_scratch_db_selftest.sh` runs in under a second
# and must keep doing so. So the READ is awk and the VALIDATION is Rust:
# `crates/core/src/config/test_overlay.rs` fails on an unknown key, so a typo
# cannot survive `cargo test -p zeroship-core`, and this reader would only ever
# return an empty value for one. The subset parsed here (a `[section]` header
# and `key = "value"`) is the whole of what the generator writes.
#
# WHAT IT DOES NOT DO. It does not invent a DSN when the file is missing. A
# suite that silently falls back to a compiled default when its configuration
# is absent is the deleted ZEROSHIP_REQUIRE_LIVE_BACKENDS flag in another
# costume: it converts "there is no configuration" into "the run passed".
# ============================================================================

# Read one `section.key` out of the generated test overlay.
#
# Usage: zs_test_config_get <section> <key>
# Prints the value, or nothing when the key is absent.
zs_test_config_get() {
  local section="$1" key="$2" file="${ZS_TEST_OVERLAY:?zs_test_config_get before zs_test_config_load}"

  awk -v want_section="$section" -v want_key="$key" '
    /^[[:space:]]*#/ { next }
    /^[[:space:]]*\[/ {
      section = $0
      sub(/^[[:space:]]*\[/, "", section)
      sub(/\].*$/, "", section)
      next
    }
    section == want_section {
      line = $0
      sub(/^[[:space:]]+/, "", line)
      eq = index(line, "=")
      if (eq == 0) next
      k = substr(line, 1, eq - 1)
      sub(/[[:space:]]+$/, "", k)
      if (k != want_key) next
      v = substr(line, eq + 1)
      sub(/^[[:space:]]+/, "", v)
      sub(/[[:space:]]+$/, "", v)
      gsub(/^"|"$/, "", v)
      print v
      exit
    }
  ' "$file"
}

# Locate and validate the generated test overlay, then export the values every
# harness needs.
#
# Sets, all exported:
#   ZS_TEST_OVERLAY  path to the overlay that was read
#   PG_HOST PG_PORT PG_USER PG_PASS PG_DB   parsed back out of the DSN
#   ZS_TEST_PG_DSN   the server DSN as written (database = PG_DB)
#   ZS_TEST_REDIS_URL
#
# The PG_* names are set for the harnesses' own psql invocations. They are also
# still honoured as INPUTS by the provisioner, which is the override tier: set
# one there and the overlay this reads is written to match.
zs_test_config_load() {
  local root dsn authority userinfo hostport
  local want_host="${PG_HOST:-}" want_port="${PG_PORT:-}"
  local want_user="${PG_USER:-}" want_pass="${PG_PASS:-}"

  root="${1:-$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)}"
  ZS_TEST_OVERLAY="$root/deploy/ops/zeroship.test.toml"

  if [ ! -f "$ZS_TEST_OVERLAY" ]; then
    echo "FATAL: no test overlay at $ZS_TEST_OVERLAY" >&2
    echo "       It names the PostgreSQL and Redis the suites dial, and is" >&2
    echo "       written alongside them by:" >&2
    echo "         tests/provision_test_backends.sh" >&2
    return 1
  fi
  export ZS_TEST_OVERLAY

  dsn="$(zs_test_config_get control database_url)"
  ZS_TEST_REDIS_URL="$(zs_test_config_get worker kv_url)"

  if [ -z "$dsn" ]; then
    echo "FATAL: $ZS_TEST_OVERLAY has no [control] database_url." >&2
    echo "       Re-run tests/provision_test_backends.sh to rewrite it." >&2
    return 1
  fi

  # Split postgres://user:pass@host:port/db without a URL parser. The password
  # is taken to the LAST '@' inside the authority, so a password containing '@'
  # cannot truncate the host - the same rule redact_dsn applies in
  # libs/compio-postgres/tests/common/mod.rs.
  authority="${dsn#*://}"
  PG_DB="${authority#*/}"
  PG_DB="${PG_DB%%\?*}"
  authority="${authority%%/*}"
  if [[ "$authority" == *@* ]]; then
    userinfo="${authority%@*}"
    hostport="${authority##*@}"
    PG_USER="${userinfo%%:*}"
    PG_PASS="${userinfo#*:}"
    [ "$PG_PASS" = "$userinfo" ] && PG_PASS=""
  else
    hostport="$authority"
    PG_USER=""
    PG_PASS=""
  fi
  PG_HOST="${hostport%%:*}"
  PG_PORT="${hostport##*:}"
  [ "$PG_PORT" = "$hostport" ] && PG_PORT=5432

  ZS_TEST_PG_DSN="$dsn"
  export PG_HOST PG_PORT PG_USER PG_PASS PG_DB ZS_TEST_PG_DSN ZS_TEST_REDIS_URL

  zs_test_config_assert_agrees "$want_host" "$want_port" "$want_user" "$want_pass" || return 1
}

# Refuse when the caller asked for one server and the overlay names another.
#
# WHY A REFUSAL AND NOT A WARNING. A harness reads PG_HOST/PG_PORT for its OWN
# psql calls; every SERVICE it spawns reads the overlay through
# `zeroship_core::config::test_overlay`. Point a suite at a second cluster with
# `PG_PORT=5444` and the two halves connect to two different servers - the
# provisioning and the probes on one, the code under test on the other. Nothing
# announces that. The run completes and reports a plausible number computed
# against two databases, which is the worst available outcome: not a failure, a
# WRONG MEASUREMENT that reads like a result.
#
# The values are also what `tests/provision_test_backends.sh` takes as INPUTS,
# so the fix a caller wants is always the same - regenerate the overlay for the
# server they meant - and the message says so.
#
# `localhost` and `127.0.0.1` are the same host and are treated as such. They
# differ as strings and CI writes one while the generator writes the other, so
# comparing them literally would refuse every CI run for no reason - a gate that
# cries wolf is a gate somebody deletes.
zs_test_config_assert_agrees() {
  local want_host="$1" want_port="$2" want_user="$3" want_pass="$4"
  local mismatch=""

  _zs_same_host() {
    local a="$1" b="$2"
    [ "$a" = "$b" ] && return 0
    case "$a" in localhost|127.0.0.1|::1) ;; *) return 1 ;; esac
    case "$b" in localhost|127.0.0.1|::1) return 0 ;; *) return 1 ;; esac
  }

  [ -n "$want_host" ] && ! _zs_same_host "$want_host" "$PG_HOST" \
    && mismatch="${mismatch}  PG_HOST: you asked for '${want_host}', the overlay says '${PG_HOST}'
"
  [ -n "$want_port" ] && [ "$want_port" != "$PG_PORT" ] \
    && mismatch="${mismatch}  PG_PORT: you asked for '${want_port}', the overlay says '${PG_PORT}'
"
  [ -n "$want_user" ] && [ "$want_user" != "$PG_USER" ] \
    && mismatch="${mismatch}  PG_USER: you asked for '${want_user}', the overlay says '${PG_USER}'
"
  # The password is compared but never printed.
  [ -n "$want_pass" ] && [ "$want_pass" != "$PG_PASS" ] \
    && mismatch="${mismatch}  PG_PASS: differs from the overlay's (values not shown)
"

  [ -z "$mismatch" ] && return 0

  echo "FATAL: the server you asked for is not the server the overlay names." >&2
  printf '%s' "$mismatch" >&2
  echo "       $ZS_TEST_OVERLAY" >&2
  echo "       This is not a preference the harness can honour halfway. Its own" >&2
  echo "       psql calls would go to your server while every service it starts" >&2
  echo "       read the overlay and went to the other one, and the run would" >&2
  echo "       report a number computed against two different databases." >&2
  echo "       Regenerate the overlay for the server you mean:" >&2
  echo "         PG_HOST=${want_host:-$PG_HOST} PG_PORT=${want_port:-$PG_PORT} \\" >&2
  echo "           tests/provision_test_backends.sh" >&2
  return 1
}
