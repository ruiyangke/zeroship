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
#               and name a setting, because there is no file to name. The
#               worker checks its join token before its peer document, so its
#               unset launch names only `worker.join_token_file`; the gateway
#               names both of its own two settings, having no such ordering.
#   missing     the same launch with both settings supplied and the peer
#               document absent. Must exit non-zero AND NAME THE PATH: an
#               operator reading a boot log needs the file.
#   malformed   the same launch with the document present and unparseable.
#               Same requirement. Paired with `missing` because the two reach
#               the loader by different routes - one never opens a file, the
#               other opens it and fails to read it as a document - and a fence
#               that caught only one would be half a fence.
#   envelope    the worker with a well-formed document that publishes a peer
#               key and NOT the gateway's. This is F4's own sentence: the
#               material the worker verifies `ZeroShip-User` under is missing,
#               everything else is present, and it must still refuse.
#   forged      a well-formed document in which ONE key appears under TWO
#               issuers. The document parses, every entry is a valid Ed25519
#               public key, every issuer is syntactically good, and every key
#               id matches its thumbprint - so every check the loader had
#               before this arm existed passes. What is wrong is a property of
#               the SET, which no per-entry check can see.
#
#               A WORKER HOLDS NO PERSISTENT KEY OF ITS OWN TO FORGE INTO THIS
#               DOCUMENT. It joins with a bearer join token and draws its
#               instance keypair in memory at boot - never on disk, never
#               before a live exchange with Control - so there is no on-disk
#               worker key this fixture could republish under the gateway's
#               issuer the way a persistent enroller key once could. The
#               remaining launch here is the one that DOES still apply to the
#               worker: control and auth sharing ONE key that belongs to
#               NEITHER launched binary. The assertion verifier resolves its
#               key from the issuer parsed OUT OF the presented assertion, so
#               under a shared-key document possession of any one service key
#               file is the ability to present as any service to any service,
#               and the launched process here is a party to neither half of
#               the collision - proving the refusal is a property of the
#               document rather than of "my own key turned up somewhere".
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
# `deploy/compose/docker-compose.yml`, which supplies every path that way. The
# worker's own credential is a JOIN TOKEN (`worker.join_token_file`): it holds
# no signing key of its own on disk, so no document below publishes a
# `svc/worker` key for it. `crate::join::read_join_token` verifies only that
# the file is a readable, owner-only, JWT-shaped bearer string - the actual
# cryptographic verification of a join token happens at Control, which this
# gate never starts - so a fixture file only needs that shape, not a real
# signature.
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
#   - how the `forged` document gets WRITTEN. This gate rules on the reader.
#     The writer is `zeroship dev init`, whose `SERVICE_KEY_FILES` rustdoc says
#     one key per service and not one shared file; it refuses a secrets
#     directory in which two service key paths hold the same key, and
#     `dev_init_refuses_when_two_service_key_paths_hold_the_same_key` in
#     crates/zeroship-cli/tests/dev_init_test.rs is the one-variable control for
#     that. The two halves are deliberately separate: an operator can hand a
#     service a document this tool never wrote, so the reader must refuse it
#     whatever produced it.
# ============================================================================
set -uo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
BIN="$ROOT/target/debug"
REBUILD_CMD="nix develop -c cargo build -p zeroship-worker -p zeroship-gateway"
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
# Read for the `forged` arm's third launch, which shares a key between two
# services the launched process is neither of.
CONTROL_NAME="$(service_name CONTROL)"
AUTH_NAME="$(service_name AUTH)"
if [ -z "$DOMAIN" ] || [ -z "$WORKER_NAME" ] || [ -z "$GATEWAY_NAME" ] \
  || [ -z "$CONTROL_NAME" ] || [ -z "$AUTH_NAME" ]; then
  echo "REFUSED: could not read the trust domain and service names from $NAMES_SRC" >&2
  exit 1
fi
echo "  issuers under test: spiffe://$DOMAIN/{$WORKER_NAME,$GATEWAY_NAME,$CONTROL_NAME,$AUTH_NAME}"

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

new_key svc-gateway || { echo "REFUSED: could not generate the gateway key" >&2; exit 1; }
# A key belonging to NEITHER launched binary, for the `forged` launch below.
new_key svc-control || { echo "REFUSED: could not generate the control key" >&2; exit 1; }
GATEWAY_X="$(public_x svc-gateway)"
CONTROL_X="$(public_x svc-control)"
[ -n "$GATEWAY_X" ] && [ -n "$CONTROL_X" ] || {
  echo "REFUSED: could not derive the fixture public keys" >&2
  exit 1
}
# The fixture keys must be DISTINCT, or the `forged` documents below would be
# indistinguishable from the control and the whole arm would be measuring
# nothing. openssl makes this overwhelmingly likely rather than certain, and a
# gate that assumed it would report a green it had not earned.
if [ "$GATEWAY_X" = "$CONTROL_X" ]; then
  echo "REFUSED: two fixture keys came out identical; the forged arm would be vacuous" >&2
  exit 1
fi

# The worker's credential is a bearer JOIN TOKEN, not a key: `crate::
# join::read_join_token` refuses an unset, unreadable, group- or
# world-readable, or non-JWT-shaped file, but it holds no signer key and
# cannot and does not verify the token itself - that happens at Control, which
# every arm here stops before reaching. So the fixture only needs the SHAPE a
# JWT has (three non-empty dot-separated parts) at mode 0600.
JOIN_TOKEN_FILE="$TMP/join-token"
printf 'REFUSEDHEADER.REFUSEDPAYLOAD.REFUSEDSIGNATURE' > "$JOIN_TOKEN_FILE"
chmod 600 "$JOIN_TOKEN_FILE"

PEERS="$TMP/service-peers.json"
printf '{"keys":[%s,%s]}' \
  "$(peer_entry "$CONTROL_NAME" "$CONTROL_X")" \
  "$(peer_entry "$GATEWAY_NAME" "$GATEWAY_X")" > "$PEERS"
# The same document with the GATEWAY entry removed and nothing else changed.
PEERS_NO_GATEWAY="$TMP/service-peers-no-gateway.json"
printf '{"keys":[%s]}' "$(peer_entry "$CONTROL_NAME" "$CONTROL_X")" > "$PEERS_NO_GATEWAY"
PEERS_MALFORMED="$TMP/service-peers-malformed.json"
printf '{ this is not a JWKS document' > "$PEERS_MALFORMED"
PEERS_ABSENT="$TMP/service-peers-absent.json"
rm -f "$PEERS_ABSENT"

# --- the `forged` documents -------------------------------------------------
# Each is `$PEERS` with ONE member's `x` changed and nothing else, so the arm
# below and the `configured` control differ in exactly one variable.
#
# There is no WORKER variant here. That arm existed while the worker held a
# persistent enroller key on disk: republishing it under the gateway's issuer
# let the worker mint an identity envelope its own verifier then accepted. A
# worker now holds no persistent key at all - it draws its instance keypair in
# memory only after a live exchange with Control, which this gate never
# starts - so there is no on-disk worker key a fixture could republish, and
# the attack surface that arm proved closed is closed by construction.
#
# The same shape on the gateway: its own key also published as the worker's, so
# it can present as the worker to any peer that reads this document.
PEERS_GATEWAY_MINTS_ITS_OWN="$TMP/service-peers-gateway-mints-its-own.json"
printf '{"keys":[%s,%s]}' \
  "$(peer_entry "$WORKER_NAME" "$GATEWAY_X")" \
  "$(peer_entry "$GATEWAY_NAME" "$GATEWAY_X")" > "$PEERS_GATEWAY_MINTS_ITS_OWN"
# The blast-radius case: the valid gateway entry, PLUS control and auth sharing
# one key that belongs to neither launched binary. Whoever holds it presents as
# either service, and the process reading this document is not a party to the
# collision - so a refusal here is a property of the document.
PEERS_THIRD_PARTIES_SHARE="$TMP/service-peers-third-parties-share.json"
printf '{"keys":[%s,%s,%s]}' \
  "$(peer_entry "$GATEWAY_NAME" "$GATEWAY_X")" \
  "$(peer_entry "$CONTROL_NAME" "$CONTROL_X")" \
  "$(peer_entry "$AUTH_NAME" "$CONTROL_X")" > "$PEERS_THIRD_PARTIES_SHARE"

UNSET_EXAMINED=0
MISSING_EXAMINED=0
MALFORMED_EXAMINED=0
ENVELOPE_EXAMINED=0
FORGED_EXAMINED=0
CONFIGURED_EXAMINED=0

# The refusal every arm below looks for. Read as a substring of the boot log,
# which is JSON with a wall-clock timestamp, so a whole-line comparison is not
# available and is not wanted: what is asserted is the SENTENCE.
#
# THE WORKER NAMES TWO, NOT ONE, because `load_join_material` in
# crates/zeroship-worker/src/main.rs checks its join token and its peer
# document as two separate fences with two separate messages, in that order:
# an unset/unreadable/malformed join token is refused before the peer document
# is ever opened, so the `unset` arm below can only ever see
# WORKER_JOIN_TOKEN_REFUSAL. Once a join token of the right SHAPE is supplied,
# missing/malformed/forged peer documents all reach `load_peer_bundle` and
# share WORKER_PEERS_REFUSAL - the envelope arm's omitted-gateway-key case is
# checked separately again, by `UserEnvelopeVerifier::for_issuer`, and keeps
# its own distinct sentence below.
REFUSAL="refusing to start - service key material rejected"
WORKER_JOIN_TOKEN_REFUSAL="refusing to start - join token rejected"
WORKER_PEERS_REFUSAL="refusing to start - peer document rejected"

# The NEXT boot refusal each binary reaches once it is past this fence: the
# worker's database posture check, the gateway's broker master secret. Each is
# the marker its own `configured` control requires and its own refusal arms
# forbid.
WORKER_NEXT="refusing unsafe database authority"
GATEWAY_NEXT="without a readable"

# EVERY REFUSAL ARM CALLS THIS, and it exists because of a measurement rather
# than for symmetry. Mutating the key-material loader - `load_join_material`
# in the worker, `build_service_auth` in the gateway - to log its message and then
# CARRY ON rather than exit left this gate fully green: the message
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

# This gate LAUNCHES binaries rather than building them, so what it rules on is
# whatever is on disk. Absence and STALENESS are different failures and only one
# of them used to be caught: a binary older than the sources it was built from
# prints exactly what a correct clean tree prints, so a mutation run against it
# reports green while proving nothing. That happened - a run with both key-reuse
# refusals deleted passed all six arms, because the worker on disk predated the
# deletion by half an hour. Refuse both, naming the rebuild.
for prog in zeroship-worker zeroship-gate; do
  if [ ! -x "$BIN/$prog" ]; then
    echo "REFUSED: $BIN/$prog is not built; run $REBUILD_CMD" >&2
    exit 1
  fi
  # The crates whose behaviour these arms rule on. A newer source file in any of
  # them means the binary cannot answer for the tree you are asking about.
  stale=$(find "$ROOT/crates/zeroship-core/src" "$ROOT/crates/zeroship-worker/src" \
    "$ROOT/crates/zeroship-gateway/src" -name '*.rs' -newer "$BIN/$prog" \
    -print -quit 2>/dev/null)
  if [ -n "$stale" ]; then
    echo "REFUSED: $BIN/$prog is OLDER than $stale, so it cannot rule on this tree; run $REBUILD_CMD" >&2
    exit 1
  fi
done

# --- ARM: unset ------------------------------------------------------------
# Neither setting supplied. The join token is checked FIRST, so this is the
# only worker arm that can ever see WORKER_JOIN_TOKEN_REFUSAL: the process
# exits before the peer document is ever opened, so it cannot name
# `worker.service_peers_file` either.
status=$(worker_run "$TMP/wk_unset.log")
UNSET_EXAMINED=$((UNSET_EXAMINED + 1))
if [ "$status" -eq 0 ]; then
  note_fail "zeroship-worker BOOTED with no service key material (exit 0)"
else
  grep -qF "$WORKER_JOIN_TOKEN_REFUSAL" "$TMP/wk_unset.log" \
    || note_fail "the worker's unconfigured refusal does not say '$WORKER_JOIN_TOKEN_REFUSAL'"
  # It cannot name a file, so it must name the setting.
  grep -qF "worker.join_token_file" "$TMP/wk_unset.log" \
    || note_fail "the worker's unconfigured refusal does not name 'worker.join_token_file'"
fi
stopped_at_the_fence "$TMP/wk_unset.log" "$WORKER_NEXT" "the unconfigured worker"

# --- ARM: missing ----------------------------------------------------------
# A valid-SHAPED join token, so the process walks past that fence and reaches
# the peer document one, where the interesting variable lives.
status=$(worker_run "$TMP/wk_missing.log" \
  "ZEROSHIP_WORKER_JOIN_TOKEN_FILE=$JOIN_TOKEN_FILE" \
  "ZEROSHIP_WORKER_SERVICE_PEERS_FILE=$PEERS_ABSENT")
MISSING_EXAMINED=$((MISSING_EXAMINED + 1))
if [ "$status" -eq 0 ]; then
  note_fail "zeroship-worker BOOTED with an absent peer document (exit 0)"
else
  grep -qF "$WORKER_PEERS_REFUSAL" "$TMP/wk_missing.log" \
    || note_fail "the worker did not refuse an absent peer document by name"
  grep -qF "$PEERS_ABSENT" "$TMP/wk_missing.log" \
    || note_fail "the worker's refusal does not name the absent file $PEERS_ABSENT"
fi
stopped_at_the_fence "$TMP/wk_missing.log" "$WORKER_NEXT" "the worker with an absent document"

# --- ARM: malformed --------------------------------------------------------
status=$(worker_run "$TMP/wk_malformed.log" \
  "ZEROSHIP_WORKER_JOIN_TOKEN_FILE=$JOIN_TOKEN_FILE" \
  "ZEROSHIP_WORKER_SERVICE_PEERS_FILE=$PEERS_MALFORMED")
MALFORMED_EXAMINED=$((MALFORMED_EXAMINED + 1))
if [ "$status" -eq 0 ]; then
  note_fail "zeroship-worker BOOTED with an unparseable peer document (exit 0)"
else
  grep -qF "$WORKER_PEERS_REFUSAL" "$TMP/wk_malformed.log" \
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
  "ZEROSHIP_WORKER_JOIN_TOKEN_FILE=$JOIN_TOKEN_FILE" \
  "ZEROSHIP_WORKER_SERVICE_PEERS_FILE=$PEERS_NO_GATEWAY")
ENVELOPE_EXAMINED=$((ENVELOPE_EXAMINED + 1))
if [ "$status" -eq 0 ]; then
  note_fail "zeroship-worker BOOTED without the gateway public key (exit 0)"
else
  grep -qF "must publish the gateway's public key" "$TMP/wk_no_gateway.log" \
    || note_fail "the worker did not refuse a document that omits the gateway key"
fi
stopped_at_the_fence "$TMP/wk_no_gateway.log" "$WORKER_NEXT" "the worker without the gateway key"

# --- ARM: forged -----------------------------------------------------------
# ONE KEY UNDER TWO ISSUERS. Every per-entry check passes; what is wrong is a
# property of the set, and `load_peer_bundle` itself is what refuses it, so
# this reaches the same WORKER_PEERS_REFUSAL sentence as the missing/malformed
# arms above.
#
# ONLY ONE WORKER VARIANT REMAINS. The old second variant published the
# worker's own persistent enroller key under the gateway's issuer; that key no
# longer exists (see the fixture-generation comment above), so the only
# forged document left that says anything about the WORKER is the
# third-party one: control and auth sharing a key that belongs to neither
# launched binary.
forged_worker() {
  local log="$1" peers="$2" label="$3"
  local status
  status=$(worker_run "$log" \
    "ZEROSHIP_WORKER_JOIN_TOKEN_FILE=$JOIN_TOKEN_FILE" \
    "ZEROSHIP_WORKER_SERVICE_PEERS_FILE=$peers")
  FORGED_EXAMINED=$((FORGED_EXAMINED + 1))
  if [ "$status" -eq 0 ]; then
    note_fail "zeroship-worker BOOTED on a document where $label (exit 0)"
  else
    grep -qF "$WORKER_PEERS_REFUSAL" "$log" \
      || note_fail "the worker did not refuse a document where $label"
  fi
  stopped_at_the_fence "$log" "$WORKER_NEXT" "the worker on a document where $label"
}

forged_worker "$TMP/wk_forged_third.log" "$PEERS_THIRD_PARTIES_SHARE" \
  "control and auth share one key, which is neither the worker's nor the gateway's"

# --- ARM: configured (the one-variable control) ----------------------------
# Only the peer document changes from the arm above. The worker must walk past
# this fence and fail on the NEXT one, which is the database posture check.
worker_run "$TMP/wk_ok.log" \
  "ZEROSHIP_WORKER_JOIN_TOKEN_FILE=$JOIN_TOKEN_FILE" \
  "ZEROSHIP_WORKER_SERVICE_PEERS_FILE=$PEERS" > /dev/null
CONFIGURED_EXAMINED=$((CONFIGURED_EXAMINED + 1))
grep -qF "$WORKER_JOIN_TOKEN_REFUSAL" "$TMP/wk_ok.log" \
  && note_fail "zeroship-worker refused a VALID-shaped join token"
grep -qF "$WORKER_PEERS_REFUSAL" "$TMP/wk_ok.log" \
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

  # The gateway's half of the `forged` arm: its OWN key also published as the
  # worker's. The gateway verifies no identity envelope, so F4 as worded does
  # not reach it - but it holds a service key and presents assertions, and the
  # assertion verifier resolves material from the issuer parsed out of what it
  # is shown. A document that maps two issuers onto one key therefore makes
  # this process able to present as the worker, which is the wider consequence
  # the header describes.
  status=$(gateway_run "$TMP/gw_forged_self.log" \
    "ZEROSHIP_GATEWAY_SERVICE_KEY_FILE=$TMP/svc-gateway.pem" \
    "ZEROSHIP_GATEWAY_SERVICE_PEERS_FILE=$PEERS_GATEWAY_MINTS_ITS_OWN")
  FORGED_EXAMINED=$((FORGED_EXAMINED + 1))
  if [ "$status" -eq 0 ]; then
    note_fail "zeroship-gate BOOTED on a document publishing its own key as the worker's (exit 0)"
  else
    grep -qF "$REFUSAL" "$TMP/gw_forged_self.log" \
      || note_fail "the gateway did not refuse a document publishing its own key as the worker's"
  fi
  stopped_at_the_fence "$TMP/gw_forged_self.log" "$GATEWAY_NEXT" \
    "the gateway on a document publishing its own key as the worker's"

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
#   forged:     2 today - the worker on two THIRD parties sharing a key, and
#               the gateway on its own key republished as the worker's. Floor
#               2, the full count deliberately: a worker holds no persistent
#               key of its own to republish, so there is no redundant second
#               worker variant left to drop, and dropping either of these two
#               remaining launches would silently narrow the arm to one actor.
#   configured: 2 today, floor 2. THE CONTROLS, and the floor is the full count
#               deliberately: a gate that lost a control would be a gate that
#               refuses everything while printing this.
# ---------------------------------------------------------------------------
gate_arm unset      "$UNSET_EXAMINED"      1
gate_arm missing    "$MISSING_EXAMINED"    1
gate_arm malformed  "$MALFORMED_EXAMINED"  1
gate_arm envelope   "$ENVELOPE_EXAMINED"   1
gate_arm forged     "$FORGED_EXAMINED"     2
gate_arm configured "$CONFIGURED_EXAMINED" 2

status=0
if [ "$FAILURES" -ne 0 ]; then
  echo "  x $FAILURES check(s) failed" >&2
  status=1
fi
gate_arms_finish || status=1
exit "$status"
