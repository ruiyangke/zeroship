#!/usr/bin/env bash
# ============================================================================
# FENCE F4's STARTUP REFUSAL, driven against the REAL BINARIES.
#
# `docs/proposals/2026-09-05-auth-foundation-redesign.md` fence F4 words the
# rule for the worker - "absent a configured gateway public key the worker
# refuses to start" - and step 3 landed only the request-time half of it:
# `ServiceAuth::unconfigured` refused every guarded edge while the process came
# up and served. That difference is the whole point. A process that starts and
# then refuses everything binds its port, answers a liveness probe and looks
# healthy to an orchestrator; the first thing that notices is an end user. A
# process that refuses to START is loud at deploy time, when someone is
# watching.
#
# `a_peer_document_that_is_unset_missing_or_malformed_refuses_to_load` in
# crates/zeroship-core/tests/service_peers_test.rs rules on the LOADER. It cannot
# rule on whether `main` calls it, and it says so. This gate closes that: it
# launches the compiled binary and reads the exit code and the message.
#
# WHAT EACH ARM RULES ON
#
#   unset       one launch per binary with neither setting supplied - the
#               default, and the state that used to BOOT. Must exit non-zero
#               and name the two settings, because there is no file to name.
#   missing     the same launch with both settings supplied and the peer
#               document absent. Must exit non-zero AND NAME THE PATH: an
#               operator reading a boot log needs the file.
#   malformed   the same launch with the document present and unparseable.
#               Same requirement. Paired with `missing` because the two reach
#               the loader by different routes - one never opens a file, the
#               other opens it and fails to read it as a document - and a fence
#               that caught only one would be half a fence.
#   envelope    the worker with a well-formed document that publishes its OWN
#               key and NOT the gateway's. This is F4's own sentence: the
#               material the worker verifies `ZeroShip-User` under is missing,
#               everything else is present, and it must still refuse.
#   configured  THE ONE-VARIABLE CONTROL, one per binary. Same launch, same
#               flags, a valid key and a valid document; the process must get
#               PAST this fence. Without it a binary that refused every launch
#               would print exactly what this gate prints.
#
# NO ARM RESTS ON THE EXIT CODE ALONE, and that is a measurement rather than
# caution. Every launch here exits non-zero whatever this fence does, because
# each binary has a LATER refusal it cannot get past in this environment - the
# worker's database-posture check, the gateway's broker master secret. So each
# arm rules on WHERE the process stopped: a refusal arm requires the fence's
# own message AND the absence of the later one; the control requires the
# reverse. See `stopped_at_the_fence` for the mutation that made this necessary.
#
# THE CONTROL THEREFORE DOES NOT ASSERT EXIT 0, and does not need to. Reaching a
# named later refusal is positive evidence the process walked THROUGH the fence,
# which an exit code cannot express and which no amount of extra infrastructure
# would make sharper.
#
# THE MATERIAL ARRIVES THROUGH THE ENVIRONMENT, matching
# `deploy/compose/docker-compose.yml`, which supplies all six paths that way.
# Note that an EMPTY environment variable is NOT the unset case: clap rejects
# `ZEROSHIP_GATEWAY_SERVICE_KEY_FILE=` with "a value is required" before any of
# this runs, so the unset arm omits the variables entirely. A gate that spelled
# unset as an empty string would be measuring argument parsing.
#
# WHAT THIS DOES NOT COVER
#   - zeroship-control, which loads the same material through the same loader.
#     Its `build_service_auth` takes a live `compio_postgres::Client`, so
#     launching it means standing up Postgres. The loader arm in crates/core
#     covers the refusal; what is unmeasured here is that control's `main`
#     calls it.
#   - zeroship-auth and zeroship-migrate-server, correctly: neither mints nor
#     verifies a service assertion, so neither holds a peer bundle and neither
#     has anything to refuse.
# ============================================================================
set -uo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
BIN="$ROOT/target/debug"
TMP="$(mktemp -d -t zeroship-peer-gate-XXXXXX)"
trap 'rm -rf "$TMP"' EXIT

# shellcheck source=tests/lib/gate_arms.sh
. "$ROOT/tests/lib/gate_arms.sh"
gate_arms_init service_peer_boot

command -v openssl > /dev/null 2>&1 || {
  echo "REFUSED: openssl is required to generate the fixture keys" >&2
  exit 1
}

FAILURES=0
note_fail() {
  FAILURES=$((FAILURES + 1))
  echo "  FAIL $1"
}

# The issuer strings are read FROM THE PRODUCT, never spelled here. A gate
# carrying its own copy of `svc/worker` keeps passing after the constant moves,
# and the peer document it writes would then be one the loader refuses for a
# reason the gate did not intend to test.
NAMES_SRC="$ROOT/crates/zeroship-core/src/service_peers.rs"
trust_domain() {
  grep -oE 'SERVICE_TRUST_DOMAIN: &str = "[^"]+"' "$NAMES_SRC" | head -1 \
    | sed -E 's/.*"([^"]+)".*/\1/'
}
service_name() {
  grep -oE "${1}_SERVICE_NAME: &str = \"[^\"]+\"" "$NAMES_SRC" | head -1 \
    | sed -E 's/.*"([^"]+)".*/\1/'
}
DOMAIN="$(trust_domain)"
WORKER_NAME="$(service_name WORKER)"
GATEWAY_NAME="$(service_name GATEWAY)"
if [ -z "$DOMAIN" ] || [ -z "$WORKER_NAME" ] || [ -z "$GATEWAY_NAME" ]; then
  echo "REFUSED: could not read the trust domain and service names from $NAMES_SRC" >&2
  exit 1
fi
echo "  issuers under test: spiffe://$DOMAIN/{$WORKER_NAME,$GATEWAY_NAME}"

# 0600 is not hygiene: the loader REFUSES a group- or world-readable private
# key, so a fixture at 0644 would fail every arm for the wrong reason.
new_key() {
  openssl genpkey -algorithm ed25519 -out "$TMP/$1.pem" 2>/dev/null || return 1
  chmod 600 "$TMP/$1.pem"
}
# An ed25519 SPKI DER is a fixed prefix plus the 32-byte key, so the raw public
# half is the last 32 bytes. base64url, unpadded, as RFC 7517 requires.
public_x() {
  openssl pkey -in "$TMP/$1.pem" -pubout -outform DER 2>/dev/null | tail -c 32 \
    | base64 | tr '+/' '-_' | tr -d '=\n'
}
peer_entry() {
  printf '{"kty":"OKP","crv":"Ed25519","iss":"spiffe://%s/%s","x":"%s"}' \
    "$DOMAIN" "$1" "$2"
}

new_key svc-worker || { echo "REFUSED: could not generate the worker key" >&2; exit 1; }
new_key svc-gateway || { echo "REFUSED: could not generate the gateway key" >&2; exit 1; }
WORKER_X="$(public_x svc-worker)"
GATEWAY_X="$(public_x svc-gateway)"
[ -n "$WORKER_X" ] && [ -n "$GATEWAY_X" ] || {
  echo "REFUSED: could not derive the fixture public keys" >&2
  exit 1
}

PEERS="$TMP/service-peers.json"
printf '{"keys":[%s,%s]}' \
  "$(peer_entry "$WORKER_NAME" "$WORKER_X")" \
  "$(peer_entry "$GATEWAY_NAME" "$GATEWAY_X")" > "$PEERS"
# The same document with the GATEWAY entry removed and nothing else changed.
PEERS_NO_GATEWAY="$TMP/service-peers-no-gateway.json"
printf '{"keys":[%s]}' "$(peer_entry "$WORKER_NAME" "$WORKER_X")" > "$PEERS_NO_GATEWAY"
PEERS_MALFORMED="$TMP/service-peers-malformed.json"
printf '{ this is not a JWKS document' > "$PEERS_MALFORMED"
PEERS_ABSENT="$TMP/service-peers-absent.json"
rm -f "$PEERS_ABSENT"

UNSET_EXAMINED=0
MISSING_EXAMINED=0
MALFORMED_EXAMINED=0
ENVELOPE_EXAMINED=0
CONFIGURED_EXAMINED=0

# The refusal every arm below looks for. Read as a substring of the boot log,
# which is JSON with a wall-clock timestamp, so a whole-line comparison is not
# available and is not wanted: what is asserted is the SENTENCE.
REFUSAL="refusing to start - service key material rejected"

# The NEXT boot refusal each binary reaches once it is past this fence: the
# worker's database posture check, the gateway's broker master secret. Each is
# the marker its own `configured` control requires and its own refusal arms
# forbid.
WORKER_NEXT="refusing unsafe database authority"
GATEWAY_NEXT="without a readable"

# EVERY REFUSAL ARM CALLS THIS, and it exists because of a measurement rather
# than for symmetry. Mutating `build_service_auth` to log its message and then
# CARRY ON - rather than exit - left this gate fully green: the message
# assertion still matched the line the mutation had not touched, and the
# non-zero exit was supplied by the later refusal. Two assertions, and both were
# satisfied by a build that had removed the fence. What discriminates is that a
# process which stopped AT the fence cannot have reached what comes after it.
stopped_at_the_fence() {
  local log="$1" later="$2" label="$3"
  grep -qF "$later" "$log" \
    && note_fail "$label walked PAST the fence and stopped at a later refusal instead"
}

# ---------------------------------------------------------------------------
# The worker. F4 names it, and it is the binary whose peer document supplies
# the gateway public key the `ZeroShip-User` envelope is verified under.
# ---------------------------------------------------------------------------
# worker_run <logfile> [KEY=VALUE ...] - the credential-bearing variables are
# constant across every arm, so only the peer document varies.
worker_run() {
  local log="$1"
  shift
  env ZEROSHIP_CONTROL_KEY=control-key-material "$@" \
    "$BIN/zeroship-worker" --port 0 > "$log" 2>&1
  echo "$?"
}

if [ ! -x "$BIN/zeroship-worker" ]; then
  echo "REFUSED: $BIN/zeroship-worker is not built; run cargo build -p zeroship-worker" >&2
  exit 1
fi

# --- ARM: unset ------------------------------------------------------------
status=$(worker_run "$TMP/wk_unset.log")
UNSET_EXAMINED=$((UNSET_EXAMINED + 1))
if [ "$status" -eq 0 ]; then
  note_fail "zeroship-worker BOOTED with no service key material (exit 0)"
else
  grep -qF "$REFUSAL" "$TMP/wk_unset.log" \
    || note_fail "the worker's unconfigured refusal does not say '$REFUSAL'"
  # It cannot name a file, so it must name the settings.
  for token in "worker.service_key_file" "worker.service_peers_file"; do
    grep -qF "$token" "$TMP/wk_unset.log" \
      || note_fail "the worker's unconfigured refusal does not name '$token'"
  done
fi
stopped_at_the_fence "$TMP/wk_unset.log" "$WORKER_NEXT" "the unconfigured worker"

# --- ARM: missing ----------------------------------------------------------
status=$(worker_run "$TMP/wk_missing.log" \
  "ZEROSHIP_WORKER_SERVICE_KEY_FILE=$TMP/svc-worker.pem" \
  "ZEROSHIP_WORKER_SERVICE_PEERS_FILE=$PEERS_ABSENT")
MISSING_EXAMINED=$((MISSING_EXAMINED + 1))
if [ "$status" -eq 0 ]; then
  note_fail "zeroship-worker BOOTED with an absent peer document (exit 0)"
else
  grep -qF "$REFUSAL" "$TMP/wk_missing.log" \
    || note_fail "the worker did not refuse an absent peer document by name"
  grep -qF "$PEERS_ABSENT" "$TMP/wk_missing.log" \
    || note_fail "the worker's refusal does not name the absent file $PEERS_ABSENT"
fi
stopped_at_the_fence "$TMP/wk_missing.log" "$WORKER_NEXT" "the worker with an absent document"

# --- ARM: malformed --------------------------------------------------------
status=$(worker_run "$TMP/wk_malformed.log" \
  "ZEROSHIP_WORKER_SERVICE_KEY_FILE=$TMP/svc-worker.pem" \
  "ZEROSHIP_WORKER_SERVICE_PEERS_FILE=$PEERS_MALFORMED")
MALFORMED_EXAMINED=$((MALFORMED_EXAMINED + 1))
if [ "$status" -eq 0 ]; then
  note_fail "zeroship-worker BOOTED with an unparseable peer document (exit 0)"
else
  grep -qF "$REFUSAL" "$TMP/wk_malformed.log" \
    || note_fail "the worker did not refuse an unparseable peer document by name"
  grep -qF "$PEERS_MALFORMED" "$TMP/wk_malformed.log" \
    || note_fail "the worker's refusal does not name the unparseable file"
fi
stopped_at_the_fence "$TMP/wk_malformed.log" "$WORKER_NEXT" "the worker with an unparseable document"

# --- ARM: envelope ---------------------------------------------------------
# F4's own sentence, and the arm the predecessor could not have: an empty
# `worker_key` used to turn the envelope check OFF. One member of the document
# differs from the control below.
status=$(worker_run "$TMP/wk_no_gateway.log" \
  "ZEROSHIP_WORKER_SERVICE_KEY_FILE=$TMP/svc-worker.pem" \
  "ZEROSHIP_WORKER_SERVICE_PEERS_FILE=$PEERS_NO_GATEWAY")
ENVELOPE_EXAMINED=$((ENVELOPE_EXAMINED + 1))
if [ "$status" -eq 0 ]; then
  note_fail "zeroship-worker BOOTED without the gateway public key (exit 0)"
else
  grep -qF "must publish the gateway's public key" "$TMP/wk_no_gateway.log" \
    || note_fail "the worker did not refuse a document that omits the gateway key"
fi
stopped_at_the_fence "$TMP/wk_no_gateway.log" "$WORKER_NEXT" "the worker without the gateway key"

# --- ARM: configured (the one-variable control) ----------------------------
# Only the peer document changes from the arm above. The worker must walk past
# this fence and fail on the NEXT one, which is the database posture check.
worker_run "$TMP/wk_ok.log" \
  "ZEROSHIP_WORKER_SERVICE_KEY_FILE=$TMP/svc-worker.pem" \
  "ZEROSHIP_WORKER_SERVICE_PEERS_FILE=$PEERS" > /dev/null
CONFIGURED_EXAMINED=$((CONFIGURED_EXAMINED + 1))
grep -qF "$REFUSAL" "$TMP/wk_ok.log" \
  && note_fail "zeroship-worker refused VALID key material"
grep -qF "refusing unsafe database authority" "$TMP/wk_ok.log" \
  || note_fail "the worker did not reach the database check, so it never passed the fence"

# ---------------------------------------------------------------------------
# The gateway. It holds the SIGNING half of the identity envelope, so the same
# absence costs every dispatch its credential.
# ---------------------------------------------------------------------------
if [ -x "$BIN/zeroship-gate" ]; then
  STRONG="0123456789abcdef0123456789abcdef"
  # A DSN the gateway PARSES but never connects to: `DbConfig::new` stores the
  # parameters and the pool is built per worker thread on first use, so no
  # Postgres is reached here. It is supplied because the inbound advance edge's
  # single-use claim needs a store, and its absence is a DIFFERENT refusal.
  gateway_run() {
    local log="$1"
    shift
    env ZEROSHIP_CONTROL_KEY=control-key-material \
      "ZEROSHIP_GATEWAY_STASH_SIGNING_KEY=$STRONG" \
      "ZEROSHIP_PAIRWISE_SALT=$STRONG" \
      "ZEROSHIP_GATEWAY_DATABASE_URL=postgres://u:p@127.0.0.1:1/unreached" \
      "$@" \
      "$BIN/zeroship-gate" --port 0 --worker-urls http://127.0.0.1:1 \
      > "$log" 2>&1
    echo "$?"
  }

  status=$(gateway_run "$TMP/gw_unset.log")
  UNSET_EXAMINED=$((UNSET_EXAMINED + 1))
  if [ "$status" -eq 0 ]; then
    note_fail "zeroship-gate BOOTED with no service key material (exit 0)"
  else
    grep -qF "$REFUSAL" "$TMP/gw_unset.log" \
      || note_fail "the gateway's unconfigured refusal does not say '$REFUSAL'"
    for token in "gateway.service_key_file" "gateway.service_peers_file"; do
      grep -qF "$token" "$TMP/gw_unset.log" \
        || note_fail "the gateway's unconfigured refusal does not name '$token'"
    done
  fi
  stopped_at_the_fence "$TMP/gw_unset.log" "$GATEWAY_NEXT" "the unconfigured gateway"

  status=$(gateway_run "$TMP/gw_missing.log" \
    "ZEROSHIP_GATEWAY_SERVICE_KEY_FILE=$TMP/svc-gateway.pem" \
    "ZEROSHIP_GATEWAY_SERVICE_PEERS_FILE=$PEERS_ABSENT")
  MISSING_EXAMINED=$((MISSING_EXAMINED + 1))
  if [ "$status" -eq 0 ]; then
    note_fail "zeroship-gate BOOTED with an absent peer document (exit 0)"
  else
    grep -qF "$REFUSAL" "$TMP/gw_missing.log" \
      || note_fail "the gateway did not refuse an absent peer document by name"
    grep -qF "$PEERS_ABSENT" "$TMP/gw_missing.log" \
      || note_fail "the gateway's refusal does not name the absent file"
  fi
  stopped_at_the_fence "$TMP/gw_missing.log" "$GATEWAY_NEXT" "the gateway with an absent document"

  status=$(gateway_run "$TMP/gw_malformed.log" \
    "ZEROSHIP_GATEWAY_SERVICE_KEY_FILE=$TMP/svc-gateway.pem" \
    "ZEROSHIP_GATEWAY_SERVICE_PEERS_FILE=$PEERS_MALFORMED")
  MALFORMED_EXAMINED=$((MALFORMED_EXAMINED + 1))
  if [ "$status" -eq 0 ]; then
    note_fail "zeroship-gate BOOTED with an unparseable peer document (exit 0)"
  else
    grep -qF "$REFUSAL" "$TMP/gw_malformed.log" \
      || note_fail "the gateway did not refuse an unparseable peer document by name"
    grep -qF "$PEERS_MALFORMED" "$TMP/gw_malformed.log" \
      || note_fail "the gateway's refusal does not name the unparseable file"
  fi
  stopped_at_the_fence "$TMP/gw_malformed.log" "$GATEWAY_NEXT" "the gateway with an unparseable document"

  # The control. No broker master secret is supplied in ANY gateway arm, so the
  # gateway with valid key material walks past this fence and stops at that one
  # instead - which is what makes this positive evidence rather than a
  # differently-shaped failure.
  gateway_run "$TMP/gw_ok.log" \
    "ZEROSHIP_GATEWAY_SERVICE_KEY_FILE=$TMP/svc-gateway.pem" \
    "ZEROSHIP_GATEWAY_SERVICE_PEERS_FILE=$PEERS" > /dev/null
  CONFIGURED_EXAMINED=$((CONFIGURED_EXAMINED + 1))
  grep -qF "$REFUSAL" "$TMP/gw_ok.log" \
    && note_fail "zeroship-gate refused VALID key material"
  grep -qF "$GATEWAY_NEXT" "$TMP/gw_ok.log" \
    || note_fail "the gateway did not reach the broker secret, so it never passed the fence"
else
  echo "  note: $BIN/zeroship-gate not built; its arms did not run"
fi

# ---------------------------------------------------------------------------
# The census. Floors sit under what these arms enumerate today, far enough that
# ordinary editing does not reach them and close enough that a collapse does.
#   unset:      2 launches today (worker, gateway), floor 1. The worker alone
#               satisfies F4 as worded; the gateway is the same fence on the
#               other end of the same hop.
#   missing:    2 today, floor 1
#   malformed:  2 today, floor 1
#   envelope:   1 today (worker only - the gateway verifies no envelope), floor 1
#   configured: 2 today, floor 2. THE CONTROLS, and the floor is the full count
#               deliberately: a gate that lost a control would be a gate that
#               refuses everything while printing this.
# ---------------------------------------------------------------------------
gate_arm unset      "$UNSET_EXAMINED"      1
gate_arm missing    "$MISSING_EXAMINED"    1
gate_arm malformed  "$MALFORMED_EXAMINED"  1
gate_arm envelope   "$ENVELOPE_EXAMINED"   1
gate_arm configured "$CONFIGURED_EXAMINED" 2

status=0
if [ "$FAILURES" -ne 0 ]; then
  echo "  x $FAILURES check(s) failed" >&2
  status=1
fi
gate_arms_finish || status=1
exit "$status"
