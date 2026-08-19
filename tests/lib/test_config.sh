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
}
