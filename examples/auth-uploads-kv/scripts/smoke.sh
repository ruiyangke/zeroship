#!/usr/bin/env bash
# auth-uploads-kv smoke - the three properties, driven over the real wire.
#
# Run `pnpm dev` first, then `pnpm smoke` (override ZEROSHIP_URL for another port).
#
# What this proves (or fails to):
#   1. anonymous callers get 401, not data
#   2. Alice can upload / list / download / delete her own object
#   3. CROSS-TENANT: Bob passing ALICE'S KEY to files.download and files.delete
#      is refused, and Alice's object is still there afterwards
#   4. the KV rate limit actually BLOCKS (429), and a blocked upload writes
#      nothing to storage
#   5. after an upload that fails AFTER the KV slot was reserved, the counter
#      is back where it started and no object was written
#
# (3) is the reason this example exists: an app whose list is prefix-scoped but
# whose by-key read trusts the key it was handed passes a single-user test and
# serves every object in the bucket to anyone who can name one.
set -uo pipefail

# Default to the port THIS app declares in vite.config.ts, not vite's generic
# 5173. Any other example's dev server also answers on 5173, and this suite
# would then drive that app instead -- reporting failures that say nothing
# about this one, or worse, passes.
URL="${ZEROSHIP_URL:-http://localhost:5183}"
RPC="${URL}/__zeroship/v1"
AUTH="${URL}/__zeroship/auth"
JAR_DIR="$(mktemp -d)"

# Confirm the target is THIS app before asserting anything about it. Without
# this, pointing the suite at another example yields a wall of failures whose
# real cause -- wrong server -- appears nowhere in the output.
preflight="$(curl -s -X POST "${RPC}/files.quota" -H 'content-type: application/json' -d '{}' 2>&1 || true)"
case "$preflight" in
  *"Method not found"*)
    echo "smoke: ${URL} is serving a DIFFERENT app -- it does not know files.quota." >&2
    echo "       Start this example's dev server (pnpm dev, vite on 5183) or set ZEROSHIP_URL." >&2
    exit 2
    ;;
  "")
    echo "smoke: nothing answered at ${URL}. Start the dev server, or set ZEROSHIP_URL." >&2
    exit 2
    ;;
esac
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

# rpc <jar|-> <procedure> <json-input>  -> prints "<body>\n<status>"
rpc() {
  local jar="$1" proc="$2" body="${3:-{\}}"
  local args=(-sS -o - -w '\n%{http_code}' -X POST -H 'content-type: application/json')
  [ "$jar" != "-" ] && args+=(-b "$JAR_DIR/$jar.jar")
  curl "${args[@]}" "${RPC}/${proc}" -d "{\"json\":${body}}"
}

status_of() { printf '%s' "$1" | tail -1; }
body_of()   { printf '%s' "$1" | sed '$d'; }
jstr()      { printf '%s' "$1" | grep -o "\"$2\":\"[^\"]*\"" | head -1 | cut -d'"' -f4; }
jnum()      { printf '%s' "$1" | grep -o "\"$2\":-\?[0-9]*" | head -1 | cut -d: -f2; }
b64()       { printf '%s' "$1" | base64 -w0; }

ALICE_SECRET="alice-secret-payload-do-not-leak"
BOB_SECRET="bob-own-payload"

# Wait until <user> has at least <n> upload slots left.
#
# The app is rate-limited, so back-to-back smoke runs collide with their own
# predecessor's burst. There is no reset hatch and there should not be one  -
# a "clear the limiter" RPC is app surface that exists only for the test. So
# the harness does what any client would: read the limit and wait it out.
wait_for_quota() {
  local who="$1" need="$2" body remaining reset
  for _ in $(seq 1 4); do
    body=$(body_of "$(rpc "$who" files.quota)")
    remaining=$(jnum "$body" remaining)
    reset=$(jnum "$body" resetMs)
    [ -n "$remaining" ] || { echo "  $who: could not read quota ($body)"; return 1; }
    [ "$remaining" -ge "$need" ] && { echo "  $who has $remaining slot(s), needs $need"; return 0; }
    local secs=$(( (${reset:-60000} / 1000) + 1 ))
    echo "  $who has only $remaining slot(s), needs $need - waiting ${secs}s for the window to reset"
    sleep "$secs"
  done
  echo "  $who: still rate-limited after waiting"
  return 1
}

echo "[0] sign in as two distinct dev users"
login alice alice@localhost alice
login bob   bob@localhost   bob
ALICE_ID=$(jstr "$(curl -sS -b "$JAR_DIR/alice.jar" "${AUTH}/session")" id)
BOB_ID=$(jstr "$(curl -sS -b "$JAR_DIR/bob.jar" "${AUTH}/session")" id)
echo "  alice = ${ALICE_ID:-<none>}"
echo "  bob   = ${BOB_ID:-<none>}"
check "the two sessions are different users" \
  bash -c "[ -n '$ALICE_ID' ] && [ -n '$BOB_ID' ] && [ '$ALICE_ID' != '$BOB_ID' ]"

echo "[0b] make room under the app's own rate limit (this run may have to wait)"
check "alice has upload headroom" wait_for_quota alice 3
check "bob has upload headroom"   wait_for_quota bob 2

echo "[1] anonymous callers are refused"
ANON=$(rpc - files.list)
echo "  files.list (anon) -> $(status_of "$ANON") $(body_of "$ANON")"
check "anonymous files.list is 401" bash -c "[ '$(status_of "$ANON")' = 401 ]"
ANON_UP=$(rpc - files.upload "{\"name\":\"x.txt\",\"contentBase64\":\"$(b64 hi)\"}")
echo "  files.upload (anon) -> $(status_of "$ANON_UP") $(body_of "$ANON_UP")"
check "anonymous files.upload is 401" bash -c "[ '$(status_of "$ANON_UP")' = 401 ]"

echo "[2] alice uploads, lists, downloads, and owns her key"
A_UP=$(rpc alice files.upload "{\"name\":\"secret.txt\",\"contentBase64\":\"$(b64 "$ALICE_SECRET")\",\"contentType\":\"text/plain\"}")
echo "  files.upload (alice) -> $(status_of "$A_UP") $(body_of "$A_UP")"
check "alice's upload succeeds" bash -c "[ '$(status_of "$A_UP")' = 200 ]"
A_KEY=$(jstr "$(body_of "$A_UP")" key)
echo "  alice key = ${A_KEY:-<none>}"
check "the key is namespaced by alice's subject" \
  bash -c "printf '%s' '$A_KEY' | grep -q '^u/${ALICE_ID}/'"

A_LIST=$(rpc alice files.list)
echo "  files.list (alice) -> $(status_of "$A_LIST") $(body_of "$A_LIST")"
check "alice's list call itself succeeds" bash -c "[ '$(status_of "$A_LIST")' = 200 ]"
check "alice's list contains her key" \
  bash -c "printf '%s' '$(body_of "$A_LIST")' | grep -q '${A_KEY:-__none__}'"

A_DL=$(rpc alice files.download "{\"key\":\"$A_KEY\"}")
echo "  files.download (alice, own key) -> $(status_of "$A_DL") $(body_of "$A_DL")"
check "alice can download her own object" bash -c "[ '$(status_of "$A_DL")' = 200 ]"
check "the bytes round-trip unchanged" \
  bash -c "printf '%s' '$(body_of "$A_DL")' | grep -q '$(b64 "$ALICE_SECRET")'"

echo "[3] bob uploads his own object (so his list is not vacuously empty)"
B_UP=$(rpc bob files.upload "{\"name\":\"bob.txt\",\"contentBase64\":\"$(b64 "$BOB_SECRET")\"}")
echo "  files.upload (bob) -> $(status_of "$B_UP") $(body_of "$B_UP")"
check "bob's upload succeeds" bash -c "[ '$(status_of "$B_UP")' = 200 ]"
B_KEY=$(jstr "$(body_of "$B_UP")" key)
echo "  bob key = ${B_KEY:-<none>}"

echo "[4] THE CROSS-TENANT NEGATIVE - bob must not reach alice's object"
B_LIST=$(rpc bob files.list)
echo "  files.list (bob) -> $(status_of "$B_LIST") $(body_of "$B_LIST")"
check "bob's list call itself succeeds (so the next check is not vacuous)" \
  bash -c "[ '$(status_of "$B_LIST")' = 200 ]"
check "bob's list DOES contain bob's own key (non-vacuous)" \
  bash -c "printf '%s' '$(body_of "$B_LIST")' | grep -q '${B_KEY:-__none__}'"
check "bob's list does NOT contain alice's key" \
  bash -c "! printf '%s' '$(body_of "$B_LIST")' | grep -q '${A_KEY:-__none__}'"

B_DL=$(rpc bob files.download "{\"key\":\"$A_KEY\"}")
echo "  files.download (bob, ALICE'S key) -> $(status_of "$B_DL") $(body_of "$B_DL")"
check "bob downloading alice's key is refused (404)" \
  bash -c "[ '$(status_of "$B_DL")' = 404 ]"
check "the refusal leaks no bytes" \
  bash -c "! printf '%s' '$(body_of "$B_DL")' | grep -q '$(b64 "$ALICE_SECRET")'"

B_RM=$(rpc bob files.delete "{\"key\":\"$A_KEY\"}")
echo "  files.delete (bob, ALICE'S key) -> $(status_of "$B_RM") $(body_of "$B_RM")"
check "bob deleting alice's key is refused (404)" \
  bash -c "[ '$(status_of "$B_RM")' = 404 ]"

A_DL2=$(rpc alice files.download "{\"key\":\"$A_KEY\"}")
echo "  files.download (alice, after bob's delete attempt) -> $(status_of "$A_DL2")"
check "alice's object survived bob's delete attempt" \
  bash -c "[ '$(status_of "$A_DL2")' = 200 ]"

echo "[5] KV/storage consistency after a post-reserve failure"
Q_BEFORE=$(rpc alice files.quota)
USED_BEFORE=$(jnum "$(body_of "$Q_BEFORE")" used)
echo "  files.quota (alice, before) -> $(status_of "$Q_BEFORE") $(body_of "$Q_BEFORE")"
BAD=$(rpc alice files.upload '{"name":"broken.txt","contentBase64":"!!!not-base64!!!"}')
echo "  files.upload (alice, malformed body) -> $(status_of "$BAD") $(body_of "$BAD")"
check "the malformed upload is refused (400)" bash -c "[ '$(status_of "$BAD")' = 400 ]"
Q_AFTER=$(rpc alice files.quota)
USED_AFTER=$(jnum "$(body_of "$Q_AFTER")" used)
echo "  files.quota (alice, after)  -> $(status_of "$Q_AFTER") $(body_of "$Q_AFTER")"
echo "  used before=${USED_BEFORE:-?} after=${USED_AFTER:-?}"
check "the failed upload did NOT consume a slot (counter compensated)" \
  bash -c "[ -n '$USED_BEFORE' ] && [ '$USED_BEFORE' = '$USED_AFTER' ]"
A_LIST2=$(rpc alice files.list)
check "the failed upload wrote no object" \
  bash -c "! printf '%s' '$(body_of "$A_LIST2")' | grep -q 'broken.txt'"

# The same seam on a different failure: the body is well-formed base64 but
# decodes past the app's 512 KiB ceiling, so the reserve has already committed
# by the time the size check fires. The whole request stays under the runtime's
# 1 MiB MAX_BODY_BYTES, which is the only reason this path is reachable at all.
BIG_B64=$(head -c 600000 /dev/zero | tr '\0' 'A' | base64 -w0)
printf '{"json":{"name":"big.bin","contentBase64":"%s"}}' "$BIG_B64" > "$JAR_DIR/big.json"
BIG_STATUS=$(curl -sS -o "$JAR_DIR/big.out" -w '%{http_code}' \
  -X POST -H 'content-type: application/json' -b "$JAR_DIR/alice.jar" \
  "${RPC}/files.upload" --data-binary @"$JAR_DIR/big.json" 2>/dev/null || echo 000)
echo "  files.upload (alice, 600000B payload / $(wc -c < "$JAR_DIR/big.json")B request) -> $BIG_STATUS $(head -c 120 "$JAR_DIR/big.out")"
check "the oversize upload is refused (413)" bash -c "[ '$BIG_STATUS' = 413 ]"
Q_AFTER_BIG=$(rpc alice files.quota)
USED_AFTER_BIG=$(jnum "$(body_of "$Q_AFTER_BIG")" used)
echo "  used after oversize=${USED_AFTER_BIG:-?} (was ${USED_BEFORE:-?})"
check "the oversize upload did NOT consume a slot either" \
  bash -c "[ '$USED_BEFORE' = '$USED_AFTER_BIG' ]"

echo "[6] the rate limit actually blocks"
B_FILES_BEFORE=$(printf '%s' "$(body_of "$(rpc bob files.list)")" | grep -o '"key":' | wc -l)
LIMIT_HIT=""
LIMIT_STATUS=""
ACCEPTED=0
for i in $(seq 1 12); do
  R=$(rpc bob files.upload "{\"name\":\"burst-$i.txt\",\"contentBase64\":\"$(b64 "burst-$i")\"}")
  S=$(status_of "$R")
  echo "  files.upload (bob, burst $i) -> $S $(body_of "$R" | head -c 160)"
  if [ "$S" = 200 ]; then
    ACCEPTED=$((ACCEPTED + 1))
  else
    LIMIT_HIT="$(body_of "$R")"
    LIMIT_STATUS="$S"
    break
  fi
done
check "an upload was eventually REFUSED, not merely counted" \
  bash -c "[ -n '$LIMIT_STATUS' ]"
check "the refusal is a 429" bash -c "[ '$LIMIT_STATUS' = 429 ]"
check "the refusal names the rate limit" \
  bash -c "printf '%s' '$LIMIT_HIT' | grep -qi 'limit'"
B_FILES_AFTER=$(printf '%s' "$(body_of "$(rpc bob files.list)")" | grep -o '"key":' | wc -l)
GREW=$(( B_FILES_AFTER - B_FILES_BEFORE ))
echo "  bob objects: before=${B_FILES_BEFORE} after=${B_FILES_AFTER} (grew ${GREW}), accepted=${ACCEPTED}"
# EXACT, not a bound: the blocked attempt must have written nothing, so the
# object count moved by precisely the number of 200s.
#
# `accepted` can legitimately exceed UPLOAD_LIMIT. The window is a FIXED
# window, so a burst that straddles a boundary gets a fresh allowance - that
# is the documented behaviour of this limiter shape, not a leak. What must
# hold regardless is that the refused attempt wrote nothing.
check "the blocked upload stored nothing (objects grew by exactly the accepted count)" \
  bash -c "[ $GREW -eq $ACCEPTED ]"
# The limit is per user, not global. If it were global, alice would now be
# blocked too - and every check above would still have passed.
A_STILL=$(rpc alice files.upload "{\"name\":\"after-bob-limit.txt\",\"contentBase64\":\"$(b64 still-fine)\"}")
echo "  files.upload (alice, while bob is blocked) -> $(status_of "$A_STILL")"
check "the limit is per-user: alice is unaffected by bob's exhaustion" \
  bash -c "[ '$(status_of "$A_STILL")' = 200 ]"

echo "[7] alice can delete her own object"
A_RM=$(rpc alice files.delete "{\"key\":\"$A_KEY\"}")
echo "  files.delete (alice, own key) -> $(status_of "$A_RM") $(body_of "$A_RM")"
check "alice's delete succeeds" \
  bash -c "[ '$(status_of "$A_RM")' = 200 ] && printf '%s' '$(body_of "$A_RM")' | grep -q '\"deleted\":true'"
A_DL3=$(rpc alice files.download "{\"key\":\"$A_KEY\"}")
check "the deleted object is gone (404)" bash -c "[ '$(status_of "$A_DL3")' = 404 ]"

echo
if [ "$FAILED" -eq 0 ]; then
  echo "smoke: all checks passed"
  exit 0
else
  echo "smoke: $FAILED check(s) failed"
  exit 1
fi
