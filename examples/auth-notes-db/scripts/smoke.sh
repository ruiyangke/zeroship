#!/usr/bin/env bash
# auth-notes-db smoke — the ownership negative, driven over the real wire.
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

echo "[0] sign in as two distinct dev users"
login alice alice@localhost alice
login bob   bob@localhost   bob
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

echo "[4] THE NEGATIVE — bob must not reach alice's note"
B_LIST=$(rpc bob notes.list)
echo "  notes.list (bob) -> $(status_of "$B_LIST") $(body_of "$B_LIST")"
check "bob's list does NOT contain alice's note" \
  bash -c "! printf '%s' '$(body_of "$B_LIST")' | grep -q '${NOTE_ID:-__none__}'"

B_GET=$(rpc bob notes.get "{\"id\":\"$NOTE_ID\"}")
echo "  notes.get (bob, ALICE'S id) -> $(status_of "$B_GET") $(body_of "$B_GET")"
check "bob reading alice's note by id is refused (404)" \
  bash -c "[ '$(status_of "$B_GET")' = 404 ]"
check "the refusal leaks no note content" \
  bash -c "! printf '%s' '$(body_of "$B_GET")' | grep -q 'for alice only'"

echo
if [ "$FAILED" -eq 0 ]; then
  echo "smoke: all checks passed"
  exit 0
else
  echo "smoke: $FAILED check(s) failed"
  exit 1
fi
