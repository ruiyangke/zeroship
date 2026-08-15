#!/usr/bin/env bash
# auth-notes-db smoke - the ownership negative, driven over the real wire.
#
# Run `pnpm dev` first, then `pnpm smoke` (override ZEROSHIP_URL for another port).
#
# What this proves (or fails to):
#   1. anonymous callers get 401, not data
#   2. Alice can create + list + read her own note
#   3. Bob's list does NOT contain Alice's note
#   4. Bob passing ALICE'S NOTE ID straight to notes.get is refused
#
# (4) is the whole reason this example exists. An app whose list query filters
# by owner but whose by-id read does not will pass (3) and fail (4).
set -uo pipefail

URL="${ZEROSHIP_URL:-http://localhost:5173}"
RPC="${URL}/__zeroship/v1"
AUTH="${URL}/__zeroship/auth"
JAR_DIR="$(mktemp -d)"
trap 'rm -rf "$JAR_DIR"' EXIT
FAILED=0

check() {
  local name="$1"; shift
  if "$@"; then
    echo "  [ok]   $name"
  else
    echo "  [FAIL] $name"
    FAILED=$((FAILED + 1))
  fi
}

# Sign a dev user in through the REAL dev-auth flow the browser uses:
#   GET  /authorize  -> login form + CSRF cookie
#   POST /authorize  -> 302 to popup-callback?code=...
#   POST /session    -> spends the code, sets __zeroship_dev_session
# Leaves a usable cookie jar at $JAR_DIR/<name>.jar.
login() {
  local jar="$JAR_DIR/$1.jar" email="$2" password="$3"
  local form csrf code

  form=$(curl -sS -c "$jar" "${AUTH}/authorize?state=smoke&redirect_uri=${URL}/__zeroship/auth/popup-callback")
  csrf=$(printf '%s' "$form" | grep -o 'name="csrf" value="[^"]*"' | sed 's/.*value="//;s/"$//')
  [ -n "$csrf" ] || { echo "  [FAIL] $1: no CSRF token in the login form"; return 1; }

  code=$(curl -sS -b "$jar" -c "$jar" -o /dev/null -w '%{redirect_url}' \
    -X POST "${AUTH}/authorize" \
    --data-urlencode "csrf=$csrf" \
    --data-urlencode "state=smoke" \
    --data-urlencode "redirect_uri=${URL}/__zeroship/auth/popup-callback" \
    --data-urlencode "email=$email" \
    --data-urlencode "password=$password" | grep -o 'code=[^&]*' | cut -d= -f2)
  [ -n "$code" ] || { echo "  [FAIL] $1: login did not return an auth code"; return 1; }

  curl -sS -b "$jar" -c "$jar" -o /dev/null \
    -X POST -H 'content-type: application/json' \
    "${AUTH}/session" -d "{\"code\":\"$code\"}"
}

# rpc <jar|-> <procedure> <json-input>  -> prints "<status> <body>"
rpc() {
  local jar="$1" proc="$2" body="${3:-{\}}"
  local args=(-sS -o - -w '\n%{http_code}' -X POST -H 'content-type: application/json')
  [ "$jar" != "-" ] && args+=(-b "$JAR_DIR/$jar.jar")
  curl "${args[@]}" "${RPC}/${proc}" -d "{\"json\":${body}}"
}

status_of() { printf '%s' "$1" | tail -1; }
body_of()   { printf '%s' "$1" | sed '$d'; }

# PREFLIGHT: prove we are talking to THIS app before asserting anything about it.
#
# `$URL` defaults to :5173, which is vite's default and therefore NOT ours alone.
# Measured 2026-08-12: examples/db-chat was serving :5173 on this machine while
# this harness's default still pointed there. It does not fail silently - db-chat
# has no `notes.*` procedure and no devAuth users, so step [0] and step [1] both
# go red. But they go red MISDIRECTINGLY:
#
#   [FAIL] alice: no CSRF token in the login form
#   [FAIL] anonymous notes.list is 401        (it is 404 - no such procedure)
#
# which reads as "this app's auth is broken" when the truth is "that is not this
# app". One probe with a specific expected value tells the two apart. 401 is the
# right signal: only an app that HAS notes.list and gates it answers 401 here.
echo "[preflight] confirm \$ZEROSHIP_URL is serving auth-notes-db"
PRE=$(rpc - notes.list)
PRE_CODE=$(status_of "$PRE")
echo "  ${RPC}/notes.list (anon) -> $PRE_CODE"
if [ "$PRE_CODE" != "401" ]; then
  echo "  [FAIL] $URL is not serving auth-notes-db (anon notes.list -> $PRE_CODE, want 401)."
  # The split is "did anything answer", NOT a specific error code. I first wrote
  # this with a 404 arm for the wrong-app case, predicting that a foreign app
  # would 404 an unknown procedure. MEASURED 2026-08-12 against examples/db-chat
  # actually serving :5173: it answers 503, so the useful hint never fired and
  # the catch-all did. Any status other than 000 means SOMETHING is serving this
  # port and it is not this app; which 4xx/5xx it happens to pick is that app's
  # business, not a signal worth branching on.
  if [ "$PRE_CODE" = "000" ]; then
    echo "         Nothing answered at all: no dev server is running on $URL."
    echo "         Start it first:  pnpm migrate && pnpm dev"
  else
    echo "         Something IS serving $URL, but it does not have this app's"
    echo "         notes.list. The usual cause is another example on this port -"
    echo "         :5173 is vite's default, not ours. Start THIS app and point at it:"
    echo "           ZEROSHIP_URL=http://localhost:5199 pnpm smoke"
  fi
  echo "  Refusing to run the ownership assertions against an unidentified server."
  exit 2
fi
echo "  [ok]   $URL is serving auth-notes-db (anon notes.list gated with 401)"

echo "[0] sign in as two distinct dev users"
# The dev password is DERIVED from the user id, not declared in vite.config.ts:
# `devPasswordFor` (sdks/bootstrap/src/dev-auth.ts, the authority) returns
# "dev-" + the first 8 characters of the id after "pws_". The ids in
# vite.config.ts are pws_alice000000000000000 and pws_bob00000000000000000.
login alice alice@localhost dev-alice000
login bob   bob@localhost   dev-bob00000
ALICE_ID=$(curl -sS -b "$JAR_DIR/alice.jar" "${AUTH}/session" | grep -o '"id":"[^"]*"' | head -1 | cut -d'"' -f4)
BOB_ID=$(curl -sS -b "$JAR_DIR/bob.jar" "${AUTH}/session" | grep -o '"id":"[^"]*"' | head -1 | cut -d'"' -f4)
echo "  alice = ${ALICE_ID:-<none>}"
echo "  bob   = ${BOB_ID:-<none>}"
check "the two sessions are different users" \
  bash -c "[ -n '$ALICE_ID' ] && [ -n '$BOB_ID' ] && [ '$ALICE_ID' != '$BOB_ID' ]"

echo "[1] anonymous callers are refused"
ANON=$(rpc - notes.list)
echo "  notes.list (anon) -> $(status_of "$ANON") $(body_of "$ANON")"
check "anonymous notes.list is 401" bash -c "[ '$(status_of "$ANON")' = 401 ]"

echo "[2] alice creates a note"
CREATED=$(rpc alice notes.create '{"title":"Alice private","body":"for alice only"}')
echo "  notes.create (alice) -> $(status_of "$CREATED") $(body_of "$CREATED")"
check "notes.create succeeds" bash -c "[ '$(status_of "$CREATED")' = 200 ]"
# A 500 here is almost always an UNMIGRATED database, not an app bug, and the
# wire body says only "internal error" so the cause is invisible from here. The
# dev runtime prints the real reason at boot ("dev schema NOT applied") and the
# server log carries `db: no such table: default.notes`. Name the likely cause
# rather than leaving the reader to find it: this exact 500 cost a full
# investigation on 2026-08-12, and the answer was that `pnpm migrate` had never
# run because the script did not exist in this package's package.json.
if [ "$(status_of "$CREATED")" = 500 ]; then
  echo "  HINT: a 500 here usually means the schema was never applied to the dev"
  echo "        database. Run 'pnpm migrate' (dev applies migrations as a separate,"
  echo "        explicit step) and re-run. Check the dev server log for"
  echo "        'dev schema NOT applied' / 'no such table'."
fi
NOTE_ID=$(body_of "$CREATED" | grep -o '"id":"[^"]*"' | head -1 | cut -d'"' -f4)
echo "  alice note id = ${NOTE_ID:-<none>}"

echo "[3] alice sees her own note"
A_LIST=$(rpc alice notes.list)
echo "  notes.list (alice) -> $(status_of "$A_LIST") $(body_of "$A_LIST")"
check "alice's list contains her note" \
  bash -c "printf '%s' '$(body_of "$A_LIST")' | grep -q '${NOTE_ID:-__none__}'"

A_GET=$(rpc alice notes.get "{\"id\":\"$NOTE_ID\"}")
echo "  notes.get (alice, own id) -> $(status_of "$A_GET") $(body_of "$A_GET")"
check "alice can read her own note by id" bash -c "[ '$(status_of "$A_GET")' = 200 ]"

echo "[4] THE NEGATIVE - bob must not reach alice's note"
B_LIST=$(rpc bob notes.list)
echo "  notes.list (bob) -> $(status_of "$B_LIST") $(body_of "$B_LIST")"
# THE STATUS IS PART OF THE ASSERTION, and leaving it out made this check
# unfailable in the direction that matters. Measured 2026-08-12 on a dev tier
# whose schema was never applied: bob's list returned
#   500 {"message":"internal error"}
# which contains no note id, so the bare `! grep` passed and printed [ok] -
# reporting "no cross-user leakage" about a request that never reached the
# database. An app that leaks every row would fail this check; an app that
# serves nobody passes it. Require the read to have SUCCEEDED first, so the
# absence of alice's id is evidence about scoping rather than about downtime.
check "bob's list is a real 200 (not an error that trivially omits the note)" \
  bash -c "[ '$(status_of "$B_LIST")' = 200 ]"
check "bob's list does NOT contain alice's note" \
  bash -c "! printf '%s' '$(body_of "$B_LIST")' | grep -q '${NOTE_ID:-__none__}'"

B_GET=$(rpc bob notes.get "{\"id\":\"$NOTE_ID\"}")
echo "  notes.get (bob, ALICE'S id) -> $(status_of "$B_GET") $(body_of "$B_GET")"
check "bob reading alice's note by id is refused (404)" \
  bash -c "[ '$(status_of "$B_GET")' = 404 ]"
# Same class: on the broken run this passed against a 400 "Invalid input" for an
# EMPTY id - alice's create had failed, so $NOTE_ID was empty and the request
# never named a note at all. Gate on the 404 so the no-leak claim is made about
# the refusal this example exists to test.
check "the refusal leaks no note content" \
  bash -c "[ '$(status_of "$B_GET")' = 404 ] && ! printf '%s' '$(body_of "$B_GET")' | grep -q 'for alice only'"

echo
if [ "$FAILED" -eq 0 ]; then
  echo "smoke: all checks passed"
  exit 0
else
  echo "smoke: $FAILED check(s) failed"
  exit 1
fi
