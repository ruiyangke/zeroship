#!/usr/bin/env bash
# ============================================================================
# THE BOOT GATE AGAINST A SILENT WEAK SERVICE CREDENTIAL, driven against the
# REAL BINARIES.
#
# The unit tests in crates/core, crates/gateway, crates/worker and
# crates/zeroship-migrate-server all rule on the same functions `main` calls. Not one of them
# rules on whether `main` CALLS them, and each says so in its own "does NOT
# cover" note. This gate closes that: it launches the compiled binary with a
# credential file and reads the exit code and the banner.
#
# WHAT EACH ARM RULES ON
#
#   sentinel      one launch per binary with `CHANGE_ME_ZEROSHIP_SERVICE_KEY`
#                 in a credential file. Must exit non-zero and the banner must
#                 name the key, the config file and the remediation command.
#   empty         the SAME launch with an empty credential file. Must produce a
#                 BYTE-IDENTICAL refusal. Treating the two differently is
#                 Gitaly's `if len(token) == 0 { return ctx, nil }`, and a gate
#                 that only checked "both fail" would pass a build that had
#                 rebuilt it.
#   configured    the one-variable control. Same launch, same flags, real key
#                 material: must exit 0. Without this arm a gate that refused
#                 everything would print exactly what this one prints.
#
# CREDENTIALS ARRIVE THROUGH THE ENVIRONMENT, not through `--<name>-file`, and
# that is a measurement rather than a convenience. `--check-config` deliberately
# does NOT open a file-sourced secret - a dry run establishes each credential's
# SOURCE without I/O - so a dry run over `--control-key-file` judges nothing and
# reports `service_credentials = unverified`. Driving this gate through files
# would therefore have measured the reading, not the rule. The environment tier
# resolves to an in-memory literal that IS judged, and it is also the tier
# `deploy/compose/docker-compose.yml` uses for every platform secret, so this
# drives the shape a deployment actually has.
#
# The variables are set with `env VAR=... <binary>`, i.e. on the CHILD only.
# Nothing here exports into this shell, and the gate's own behaviour does not
# depend on how it was launched.
#
# WHY --check-config AND NOT ONLY A BOOT. Both are driven. A real boot reads
# the file and reaches the same gate; the dry run is the one
# `deploy/scripts/deploy-remote.sh` runs, so the dry run's EXIT CODE is what
# actually gates a deploy - see
# docs/proposals/2026-08-20-metering-transport-not-configured.md for the 43
# days a truthfully-reported posture field bought nobody anything. The gate
# refuses a dry run in every build profile precisely so this arm can drive the
# production behaviour from a debug binary.
#
# WHAT THIS DOES NOT COVER
#   - the DEV ESCAPE arm on a real boot. That needs a debug binary to bind a
#     port and reach a live control plane, which this gate deliberately does
#     not stand up. Both verdict arms are driven as pure functions by
#     `empty_and_sentinel_reach_the_same_verdict` in crates/core, and the
#     production-boot arm is driven against a RELEASE binary by hand.
#   - /readyz answering 503 under the escape. Driven by `is_ready` unit tests
#     in crates/zeroship-gateway/src/health.rs and crates/zeroship-worker/src/health.rs.
#   - control and auth. Neither is launched here; control's rows are asserted
#     by its own unit tests. Adding them means standing up Postgres.
#   - deploy/compose/docker-compose.yml, AS OF 2026-08-21. A `compose` arm
#     delegated to tests/compose_secret_strength_gate.sh and ruled on 14 of its
#     checks; that gate was deleted with the rest of the compose gates and this
#     arm went with it. So the binaries' own defaults are checked here and the
#     deployment artifact people copy is checked nowhere - which is exactly the
#     Loki case the arm was added for.
# ============================================================================
set -uo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
BIN="$ROOT/target/debug"
TMP="$(mktemp -d -t zeroship-credential-gate-XXXXXX)"
trap 'rm -rf "$TMP"' EXIT

# shellcheck source=tests/lib/gate_arms.sh
. "$ROOT/tests/lib/gate_arms.sh"
gate_arms_init service_credential_boot

# The sentinel is read from the product, never spelled here. A gate that
# carried its own copy would keep passing after the product renamed it.
#
# IT ALSO CATCHES A MOVE, and did twice on 2026-08-21: the constant left
# credential_gate.rs for a leaf crate and came back the same day, and on each
# move this refused with the message below instead of testing a sentinel it had
# invented. Keep the path pointed at wherever the constant is DEFINED, never at
# a re-export - config/mod.rs re-exports it, so a grep there would find the NAME
# and never the value.
SENTINEL_SRC="$ROOT/crates/zeroship-core/src/config/credential_gate.rs"
SENTINEL="$(grep -oE 'CHANGE_ME_[A-Z_]+' "$SENTINEL_SRC" | head -1)"
if [ -z "$SENTINEL" ]; then
  echo "REFUSED: no sentinel constant found in $SENTINEL_SRC" >&2
  exit 1
fi
echo "  sentinel under test: $SENTINEL"

STRONG="0123456789abcdef0123456789abcdef"
FAILURES=0

note_fail() {
  FAILURES=$((FAILURES + 1))
  echo "  FAIL $1"
}

# secret_file <name> <contents>  - 0600, which read_secret_file requires.
secret_file() {
  local path="$TMP/$1"
  printf '%s' "$2" > "$path"
  chmod 600 "$path"
  printf '%s' "$path"
}

# run_gate <logfile> <binary> <args...> - exit code WITHOUT a pipe.
# Credential variables are prepended by the caller through `env`.
run_gate() {
  local log="$1"
  shift
  "$@" > "$log" 2>&1
  echo "$?"
}

# The tracing output is JSON with a wall-clock timestamp, so two runs of the
# same refusal are never byte-identical as emitted. Strip the timestamp field
# and compare what is left: the identity being asserted is about the MESSAGE,
# and comparing timestamps would make the assertion impossible to satisfy.
normalise() {
  sed -E 's/"timestamp":"[^"]*"/"timestamp":"T"/g' "$1"
}

SENTINEL_EXAMINED=0
EMPTY_EXAMINED=0
CONFIGURED_EXAMINED=0

if [ ! -x "$BIN/zeroship-gate" ]; then
  echo "REFUSED: $BIN/zeroship-gate is not built; run cargo build -p zeroship-gateway" >&2
  exit 1
fi

# ---------------------------------------------------------------------------
# The gateway, whose four credentials are all unconditional.
# ---------------------------------------------------------------------------
# gateway_run <logfile> <control-key-value> [extra args...]
gateway_run() {
  local log="$1" control="$2"
  shift 2
  run_gate "$log" env \
    "ZEROSHIP_CONTROL_KEY=$control" \
    "ZEROSHIP_WORKER_KEY=$STRONG" \
    "ZEROSHIP_GATEWAY_STASH_SIGNING_KEY=$STRONG" \
    "ZEROSHIP_PAIRWISE_SALT=$STRONG" \
    "$BIN/zeroship-gate" "$@"
}

# --- ARM: sentinel ---------------------------------------------------------
status=$(gateway_run "$TMP/gw_sentinel.log" "$SENTINEL" --check-config)
SENTINEL_EXAMINED=$((SENTINEL_EXAMINED + 1))
if [ "$status" -eq 0 ]; then
  note_fail "zeroship-gate --check-config accepted the $SENTINEL placeholder (exit 0)"
else
  # The banner must name the KEY, the FILE and the exact remediation command.
  for token in "ZEROSHIP_CONTROL_KEY" "config file" "remediation" \
               "zeroship dev init" "$SENTINEL" "REFUSES TO START" \
               "control-route-sync"; do
    grep -qF "$token" "$TMP/gw_sentinel.log" \
      || note_fail "the sentinel banner does not name '$token'"
  done
fi

# The SAME placeholder on a REAL BOOT of the DEBUG binary. This is the DEV
# ESCAPE, and it is keyed to `cfg!(debug_assertions)` and to nothing else - not
# a flag, not an environment variable. It must print the banner (a silent
# escape is the thing this gate exists to prevent) and it must say what it is
# keyed to, so a reader of the log can tell why it did not refuse.
gateway_run "$TMP/gw_sentinel_boot.log" "$SENTINEL" > /dev/null
SENTINEL_EXAMINED=$((SENTINEL_EXAMINED + 1))
for token in "UNCONFIGURED SERVICE CREDENTIAL" "development build only" \
             "debug_assertions" "release build refuses" "/readyz"; do
  grep -qF "$token" "$TMP/gw_sentinel_boot.log" \
    || note_fail "the dev-escape banner does not say '$token'"
done
grep -qF "REFUSES TO START" "$TMP/gw_sentinel_boot.log" \
  && note_fail "a development build printed the production refusal"

# THE ONE-VARIABLE CONTROL ON THE BUILD PROFILE. Same binary name, same flags,
# same environment; the only thing that differs is `cargo build --release`,
# which turns `debug_assertions` off. A release build must REFUSE where the
# debug build escaped. Without this arm, "the escape is keyed to the profile"
# would be a claim about a `cfg!` and not a measurement.
PROFILE_EXAMINED=0
REL="$ROOT/target/release/zeroship-gate"
if [ -x "$REL" ]; then
  status=$(run_gate "$TMP/gw_release_boot.log" env \
    "ZEROSHIP_CONTROL_KEY=$SENTINEL" \
    "ZEROSHIP_WORKER_KEY=$STRONG" \
    "ZEROSHIP_GATEWAY_STASH_SIGNING_KEY=$STRONG" \
    "ZEROSHIP_PAIRWISE_SALT=$STRONG" \
    "$REL")
  PROFILE_EXAMINED=$((PROFILE_EXAMINED + 1))
  SENTINEL_EXAMINED=$((SENTINEL_EXAMINED + 1))
  [ "$status" -eq 0 ] \
    && note_fail "the RELEASE gateway booted on the $SENTINEL placeholder"
  grep -qF "REFUSES TO START" "$TMP/gw_release_boot.log" \
    || note_fail "the release-build refusal printed no banner"
  grep -qF "development build only" "$TMP/gw_release_boot.log" \
    && note_fail "a release build took the development escape"

  # And the partner for THAT: the same release binary with real material must
  # get past the credential gate, so the refusal is about the credential and
  # not about the release build refusing everything.
  run_gate "$TMP/gw_release_ok.log" env \
    "ZEROSHIP_CONTROL_KEY=control-key-material" \
    "ZEROSHIP_WORKER_KEY=$STRONG" \
    "ZEROSHIP_GATEWAY_STASH_SIGNING_KEY=$STRONG" \
    "ZEROSHIP_PAIRWISE_SALT=$STRONG" \
    "$REL" --check-config > /dev/null
  PROFILE_EXAMINED=$((PROFILE_EXAMINED + 1))
  grep -q 'service_credentials = configured' "$TMP/gw_release_ok.log" \
    || note_fail "the release binary did not report a healthy posture"
else
  echo "  note: $REL not built; the build-profile arm did not run."
  echo "        Build it with: cargo build --release -p zeroship-gateway --bin zeroship-gate"
fi

# --- ARM: empty ------------------------------------------------------------
status=$(gateway_run "$TMP/gw_empty.log" "" --check-config)
EMPTY_EXAMINED=$((EMPTY_EXAMINED + 1))
if [ "$status" -eq 0 ]; then
  note_fail "zeroship-gate --check-config accepted an EMPTY control key (exit 0)"
fi
# THE IDENTITY, and it is the sharpest thing this gate rules on: not "both
# failed" but "both failed in the same words". A build that gave the empty case
# its own branch would pass a both-failed check and fail this one.
if ! diff <(normalise "$TMP/gw_sentinel.log") <(normalise "$TMP/gw_empty.log") > /dev/null; then
  note_fail "empty and sentinel produced DIFFERENT output; they must be one branch"
  diff <(normalise "$TMP/gw_sentinel.log") <(normalise "$TMP/gw_empty.log") | sed 's/^/    /'
fi

# The empty case on the same real boot. It must reach the SAME dev-escape
# banner, not a different branch.
gateway_run "$TMP/gw_empty_boot.log" "" > /dev/null
EMPTY_EXAMINED=$((EMPTY_EXAMINED + 1))
diff <(normalise "$TMP/gw_sentinel_boot.log" | head -14) \
     <(normalise "$TMP/gw_empty_boot.log" | head -14) > /dev/null \
  || note_fail "real boot: empty and sentinel produced DIFFERENT banners"

# --- ARM: configured (the one-variable control) ----------------------------
# Only the control key changes from the sentinel run above.
status=$(gateway_run "$TMP/gw_ok.log" "control-key-material" --check-config)
CONFIGURED_EXAMINED=$((CONFIGURED_EXAMINED + 1))
if [ "$status" -ne 0 ]; then
  note_fail "zeroship-gate REFUSED a correctly configured credential (exit $status)"
  sed 's/^/    /' "$TMP/gw_ok.log"
fi
grep -q 'service_credentials = configured' "$TMP/gw_ok.log" \
  || note_fail "the check-config report does not publish the credential posture"
grep -q 'service_credentials_checked = 4' "$TMP/gw_ok.log" \
  || note_fail "the report does not say how many credentials it ruled on"
grep -qF "REFUSES TO START" "$TMP/gw_ok.log" \
  && note_fail "a healthy configuration printed a refusal banner"

# A dry run over FILE-sourced credentials reads none of them, and must say so
# rather than reporting a green built out of zero readings.
status=$(run_gate "$TMP/gw_files.log" "$BIN/zeroship-gate" --check-config \
  --control-key-file "$(secret_file gw_control "control-key-material")" \
  --worker-key-file "$(secret_file gw_worker "$STRONG")" \
  --stash-signing-key-file "$(secret_file gw_stash "$STRONG")" \
  --pairwise-salt-file "$(secret_file gw_salt "$STRONG")")
CONFIGURED_EXAMINED=$((CONFIGURED_EXAMINED + 1))
[ "$status" -ne 0 ] && note_fail "a file-sourced dry run must not fail (exit $status)"
grep -q 'service_credentials = unverified' "$TMP/gw_files.log" \
  || note_fail "a dry run that read nothing reported something other than 'unverified'"

# ---------------------------------------------------------------------------
# The worker. Its control key is the FLOORLESS one - SecretStrength::
# Unrestricted - so nothing but the sentinel branch can refuse a placeholder
# there, and that is the case this whole gate exists for.
# ---------------------------------------------------------------------------
if [ -x "$BIN/zeroship-worker" ]; then
  worker_run() {
    local log="$1" control="$2"
    shift 2
    run_gate "$log" env \
      "ZEROSHIP_CONTROL_KEY=$control" \
      "ZEROSHIP_WORKER_KEY=$STRONG" \
      "$BIN/zeroship-worker" "$@"
  }

  status=$(worker_run "$TMP/wk_sentinel.log" "$SENTINEL" --check-config)
  SENTINEL_EXAMINED=$((SENTINEL_EXAMINED + 1))
  [ "$status" -eq 0 ] \
    && note_fail "zeroship-worker accepted the placeholder on a FLOORLESS credential"

  status=$(worker_run "$TMP/wk_empty.log" "" --check-config)
  EMPTY_EXAMINED=$((EMPTY_EXAMINED + 1))
  [ "$status" -eq 0 ] \
    && note_fail "zeroship-worker accepted an EMPTY floorless credential"
  diff <(normalise "$TMP/wk_sentinel.log") <(normalise "$TMP/wk_empty.log") > /dev/null \
    || note_fail "worker: empty and sentinel produced DIFFERENT output"

  # ONE BYTE of real material in the same floorless credential. This is the
  # control that proves the refusals above are about the placeholder and not
  # about length: `require_nonempty` has no floor to fail.
  status=$(worker_run "$TMP/wk_ok.log" "x" --check-config)
  CONFIGURED_EXAMINED=$((CONFIGURED_EXAMINED + 1))
  if [ "$status" -ne 0 ]; then
    note_fail "zeroship-worker REFUSED one byte of real floorless material (exit $status)"
    sed 's/^/    /' "$TMP/wk_ok.log"
  fi
else
  echo "  note: $BIN/zeroship-worker not built; its arms did not run"
fi

# ---------------------------------------------------------------------------
# migrated: the PER-SUBSYSTEM arm. Its `control_key` is gated on being
# supplied at all, because section 5.2 of the service-identity proposal
# measured that this service makes no credentialed platform call. A launch
# that omits it must still succeed - a single-VPS deployer is not blocked on a
# credential for a subsystem that does not exist - while the seal key it DOES
# need is still refused as a placeholder.
# ---------------------------------------------------------------------------
SUBSYSTEM_EXAMINED=0
if [ -x "$BIN/zeroship-migrate-server" ]; then
  status=$(run_gate "$TMP/mg_skip.log" env \
    "ZEROSHIP_MIGRATE_SERVER_POLICY_SEAL_KEY=$STRONG" \
    "$BIN/zeroship-migrate-server" --check-config)
  SUBSYSTEM_EXAMINED=$((SUBSYSTEM_EXAMINED + 1))
  if [ "$status" -ne 0 ]; then
    note_fail "zeroship-migrate-server refused a launch that omits a credential it does not need"
    sed 's/^/    /' "$TMP/mg_skip.log"
  fi
  grep -q 'service_credentials_skipped = 1' "$TMP/mg_skip.log" \
    || note_fail "migrated did not report the SKIPPED subsystem; a silent skip is a smaller green"

  # The one-variable partner for the skip: the SAME launch with the optional
  # credential supplied as a placeholder is now checked, and refused.
  status=$(run_gate "$TMP/mg_optional_sentinel.log" env \
    "ZEROSHIP_MIGRATE_SERVER_POLICY_SEAL_KEY=$STRONG" \
    "ZEROSHIP_CONTROL_KEY=$SENTINEL" \
    "$BIN/zeroship-migrate-server" --check-config)
  SUBSYSTEM_EXAMINED=$((SUBSYSTEM_EXAMINED + 1))
  SENTINEL_EXAMINED=$((SENTINEL_EXAMINED + 1))
  [ "$status" -eq 0 ] \
    && note_fail "migrated skipped a credential the operator DID supply"

  # The credential it always needs, as a placeholder. This is a dry run that
  # used to exit 0 because every migrated guard sat BELOW the --check-config
  # return, so the report emitted and the process returned Ok without ever
  # reaching a credential check.
  status=$(run_gate "$TMP/mg_seal_sentinel.log" env \
    "ZEROSHIP_MIGRATE_SERVER_POLICY_SEAL_KEY=$SENTINEL" \
    "$BIN/zeroship-migrate-server" --check-config)
  SENTINEL_EXAMINED=$((SENTINEL_EXAMINED + 1))
  SUBSYSTEM_EXAMINED=$((SUBSYSTEM_EXAMINED + 1))
  [ "$status" -eq 0 ] \
    && note_fail "zeroship-migrate-server --check-config accepted a placeholder seal key"
else
  echo "  note: $BIN/zeroship-migrate-server not built; its arms did not run"
fi

# ---------------------------------------------------------------------------
# The census. Floors are bound to what these arms enumerate today and sit
# under it, so a collapse is visible and ordinary editing is not.
#   sentinel:       6 launches today (3 gateway, 1 worker, 2 migrated), floor 3
#   empty:          3 launches today (2 gateway, 1 worker), floor 2
#   configured:     3 today (2 gateway, 1 worker), floor 2 - these are the
#                   controls, and a gate with no control refuses everything
#                   while printing this
#   subsystem:      3 migrated launches today, floor 2
#   profile:        2 release-binary launches, floor 1. ZERO when the release
#                   binary is absent, which is a REFUSAL rather than a quiet
#                   pass: the build-profile claim is the one thing no debug
#                   binary can measure, so a run that skipped it must not print
#                   what a run that made it prints.
# ---------------------------------------------------------------------------
gate_arm sentinel   "$SENTINEL_EXAMINED"   3
gate_arm empty      "$EMPTY_EXAMINED"      2
gate_arm configured "$CONFIGURED_EXAMINED" 2
gate_arm subsystem  "$SUBSYSTEM_EXAMINED"  2
gate_arm profile    "$PROFILE_EXAMINED"    1

status=0
if [ "$FAILURES" -ne 0 ]; then
  echo "  x $FAILURES check(s) failed" >&2
  status=1
fi
gate_arms_finish || status=1
exit "$status"
