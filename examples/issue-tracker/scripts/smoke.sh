#!/usr/bin/env bash
# End-to-end smoke test for issue-tracker, run against a live dev runtime.
#
#   Terminal 1:  pnpm dev
#   Terminal 2:  pnpm smoke
#
# WHY THIS EXISTS RATHER THAN MORE UNIT TESTS. The vitest suite covers the pure
# helpers in src/lib and passes at 66/66 while every defect below is present.
# All three were found the first time the authenticated write path was actually
# driven, and none of them is reachable without a real runtime, a real database
# and a real identity:
#
#   1. bugs.resolve never wrote `resolvedAt`, so the column existed, was
#      indexed, and was null on every resolved bug in the system.
#   2. components.create declared defaultAssigneeId optional against a NOT NULL
#      column, turning a caller-fixable input error into HTTP 500.
#   3. Clearing a timestamp stores an EMPTY STRING rather than SQL NULL, so a
#      reopened bug reads `resolvedAt: ""` and a `!= null` test admits it.
#
# WHAT THIS DOES NOT CATCH. It drives the DEV tier. Identity arrives through
# `dev_auth::resolve_dev_user_json` instead of the gateway's `ZeroShip-User`
# header -- the same `call_fetch_handler_with_user` path underneath, but the
# gateway's own JWT validation, rate limiting and route dispatch are NOT
# exercised here. It also runs against SQLite, so anything Postgres-specific
# (collation, isolation, partial indexes) is out of scope; see
# docs/reference/sqlite-divergences.md. And it asserts on one creator identity,
# so nothing here says whether product visibility or private-comment filtering
# actually hides rows from a DIFFERENT user -- that needs a second identity and
# is not covered.
set -uo pipefail

URL="${ZEROSHIP_URL:-http://localhost:3007}"
RPC="$URL/__zeroship/v1"
PASS=0
FAIL=0

pass() { echo "  ok    $1"; PASS=$((PASS + 1)); }
fail() { echo "  FAIL  $1"; [ -n "${2-}" ] && echo "        $2"; FAIL=$((FAIL + 1)); }

# ---------------------------------------------------------------------------
# Identity. The dev runtime resolves `__zeroship_dev_session` before dispatch;
# the envelope is base64url(user_json) "." hex-hmac-sha256(secret, payload) and
# is defined by crates/runtime/src/core/dev_auth.rs::sign_dev_session. The
# secret is minted per dev server by the Vite plugin and handed to the spawned
# `zeroship serve` child, so it is read back out of that child's environment.
# ---------------------------------------------------------------------------
PORT="${URL##*:}"
SERVE_PID="$(lsof -ti "tcp:$PORT" 2>/dev/null | head -1)"
if [ -z "$SERVE_PID" ]; then
  echo "nothing is listening on $URL -- start the dev runtime first:  pnpm dev" >&2
  exit 2
fi
SECRET="$(tr '\0' '\n' < "/proc/$SERVE_PID/environ" 2>/dev/null | grep '^ZEROSHIP_DEV_AUTH_SECRET=' | cut -d= -f2-)"
if [ -z "$SECRET" ]; then
  echo "could not read ZEROSHIP_DEV_AUTH_SECRET from pid $SERVE_PID." >&2
  echo "That variable is what makes an authenticated dev request possible; without" >&2
  echo "it every mutation below would 401 and the run would report failures that" >&2
  echo "say nothing about the app. Refusing to run a test that cannot pass." >&2
  exit 2
fi
COOKIE="$(node -e '
const { createHmac } = require("crypto");
const user = JSON.stringify({
  id: "pws_devalice0000000000", email: "alice@localhost", name: "Alice Dev",
  avatar: null, email_verified: true, scopes: ["openid", "profile", "email"],
});
const p = Buffer.from(user).toString("base64url");
process.stdout.write(`__zeroship_dev_session=${p}.${createHmac("sha256", process.argv[1]).update(p).digest("hex")}`);
' "$SECRET")"

# The empty-args default is spelled with a variable rather than inline in the
# parameter expansion: `${2:-\{\}}` expands to a LITERAL \{\}, which sends
# invalid JSON and comes back as an error the assertion then reads as a missing
# field. Every call that passed an argument worked, so the bug only showed on
# the one call that relied on the default.
_body() { [ -n "${1-}" ] && printf '%s' "$1" || printf '{}'; }
call() { curl -sS -m 25 -X POST -H 'content-type: application/json' -H "Cookie: $COOKIE" "$RPC/$1" -d "{\"json\":$(_body "${2-}")}"; }
code() { curl -sS -m 25 -o /dev/null -w '%{http_code}' -X POST -H 'content-type: application/json' -H "Cookie: $COOKIE" "$RPC/$1" -d "{\"json\":$(_body "${2-}")}"; }
anon() { curl -sS -m 25 -o /dev/null -w '%{http_code}' -X POST -H 'content-type: application/json' "$RPC/$1" -d "{\"json\":$(_body "${2-}")}"; }
jget() { node -e 'let s="";process.stdin.on("data",d=>s+=d).on("end",()=>{try{const o=JSON.parse(s);const v=process.argv[1].split(".").reduce((a,k)=>a?.[k],o);process.stdout.write(v===undefined?"<missing>":(v===null?"<null>":String(v)))}catch{process.stdout.write("<unparseable>")}})' "$1"; }

STAMP="$$-$(date +%s)"

echo "identity"
ME="$(call users.me | jget 'json.email')"
[ "$ME" = "alice@localhost" ] && pass "users.me resolves the dev identity" \
  || fail "users.me did not resolve the dev identity" "got: $ME"

echo "structure"
PROD="$(call products.create "{\"name\":\"Smoke $STAMP\",\"description\":\"smoke product\"}" | jget 'json.id')"
[ "${PROD#prod_}" != "$PROD" ] && pass "products.create" || { fail "products.create" "got: $PROD"; echo "cannot continue without a product"; exit 1; }

# REGRESSION (defect 2): defaultAssigneeId is NOT NULL in the schema and was
# declared optional here. Omitting it used to reach the insert and come back as
# HTTP 500 "internal error". The assertion is specifically that this is NOT a
# 5xx -- a 400 would also be a legitimate design, but a 500 never is.
COMP_CODE="$(code components.create "{\"productId\":\"$PROD\",\"name\":\"Parser\",\"description\":\"parser\"}")"
[ "$COMP_CODE" = "200" ] && pass "components.create with no defaultAssigneeId succeeds (was 500)" \
  || fail "components.create with no defaultAssigneeId" "http=$COMP_CODE (a 5xx here is defect 2 regressing)"
COMP="$(call components.list "{\"productId\":\"$PROD\"}" | jget 'json.0.id')"
VER="$(call versions.create "{\"productId\":\"$PROD\",\"name\":\"1.0\"}" | jget 'json.id')"
[ "${VER#vers_}" != "$VER" ] && pass "versions.create" || fail "versions.create" "got: $VER"

echo "bug lifecycle"
BUG="$(call bugs.create "{\"productId\":\"$PROD\",\"componentId\":\"$COMP\",\"versionId\":\"$VER\",\"summary\":\"Parser drops trailing newline\",\"description\":\"repro\",\"severity\":\"major\",\"priority\":\"P2\"}" | jget 'json.id')"
[ "${BUG#bug_}" != "$BUG" ] && pass "bugs.create" || { fail "bugs.create" "got: $BUG"; exit 1; }

[ "$(call bugs.get "{\"id\":\"$BUG\"}" | jget 'json.bug.resolvedAt')" = "<null>" ] \
  && pass "a new bug has no resolvedAt" || fail "a new bug already carries resolvedAt"

call comments.add "{\"bugId\":\"$BUG\",\"body\":\"confirmed on 1.0\"}" >/dev/null
[ "$(call bugs.get "{\"id\":\"$BUG\"}" | jget 'json.bug.commentCount')" = "2" ] \
  && pass "commentCount tracks the description plus one comment" \
  || fail "commentCount wrong after comments.add"

# Severity and priority are separate axes in Bugzilla. If a future refactor
# collapses them, this catches it: setting one must not move the other.
call bugs.setPriority "{\"id\":\"$BUG\",\"priority\":\"P1\"}" >/dev/null
SEV="$(call bugs.get "{\"id\":\"$BUG\"}" | jget 'json.bug.severity')"
[ "$SEV" = "major" ] && pass "changing priority leaves severity alone" \
  || fail "severity moved when priority changed" "severity=$SEV"

echo "resolution"
call bugs.resolve "{\"id\":\"$BUG\",\"resolution\":\"FIXED\"}" >/dev/null
STATE="$(call bugs.get "{\"id\":\"$BUG\"}")"
[ "$(echo "$STATE" | jget 'json.bug.status')" = "RESOLVED" ] && pass "status is RESOLVED" || fail "status after resolve"
[ "$(echo "$STATE" | jget 'json.bug.resolution')" = "FIXED" ] && pass "resolution is FIXED" || fail "resolution after resolve"

# REGRESSION (defect 1): this was null on every resolved bug, which left
# reports.timeToResolve scanning the activities table for status->RESOLVED.
RESOLVED_AT="$(echo "$STATE" | jget 'json.bug.resolvedAt')"
case "$RESOLVED_AT" in
  ''|*[!0-9]*) fail "bugs.resolve stamps resolvedAt" "got: $RESOLVED_AT (defect 1 regressing)" ;;
  *)           pass "bugs.resolve stamps resolvedAt" ;;
esac

# REGRESSION (defect 1, the consequence): the report must actually count it.
# Asserted as ">= 1" rather than "== 1" because the dev database is not reset
# between runs, so earlier smoke runs legitimately leave resolved bugs behind.
COUNT="$(call reports.timeToResolve '{}' | jget 'json.resolvedCount')"
case "$COUNT" in
  ''|*[!0-9]*) fail "reports.timeToResolve returns a count" "got: $COUNT" ;;
  *) [ "$COUNT" -ge 1 ] && pass "reports.timeToResolve counts the resolved bug" \
       || fail "reports.timeToResolve counted nothing" "resolvedCount=$COUNT" ;;
esac

# REGRESSION (defect 3): reopen clears resolvedAt to an EMPTY STRING, not NULL.
# The report filters on typeof number so "" cannot become NaN and poison the
# average. This asserts the average stays a number after a reopen.
call bugs.reopen "{\"id\":\"$BUG\"}" >/dev/null
AVG="$(call reports.timeToResolve '{}' | jget 'json.averageMs')"
case "$AVG" in
  ''|*[!0-9]*) [ "$AVG" = "<null>" ] && pass "averageMs stays clean after a reopen (null, no bugs in window)" \
                 || fail "averageMs is not a number after a reopen" "got: $AVG (an empty resolvedAt leaked in as NaN)" ;;
  *)           pass "averageMs stays a number after a reopen" ;;
esac

# The transition table is enforced by the runtime, not only by the unit tests:
# the bug is now open, so a second reopen must be refused.
[ "$(code bugs.reopen "{\"id\":\"$BUG\"}")" = "409" ] \
  && pass "reopening an open bug is refused with 409" || fail "second reopen was not refused"

echo "history"
HIST="$(call bugs.get "{\"id\":\"$BUG\"}" | node -e 'let s="";process.stdin.on("data",d=>s+=d).on("end",()=>{const a=JSON.parse(s).json.activities;process.stdout.write(a.filter(x=>x.fieldName==="status").map(x=>`${x.oldValue}>${x.newValue}`).join(","))})')"
case "$HIST" in
  *"UNCONFIRMED>RESOLVED"*) pass "activity history records the status transition" ;;
  *) fail "status transition missing from history" "status rows: $HIST" ;;
esac

echo "auth posture"
# Fail-closed: a write with no identity must be refused, not silently accepted.
[ "$(anon products.create '{"name":"nope","description":"nope"}')" = "401" ] \
  && pass "an unauthenticated write is refused with 401" || fail "an unauthenticated write was NOT refused"
[ "$(anon products.list '{}')" = "200" ] \
  && pass "an anonymous read is allowed" || fail "an anonymous read was refused"

echo
echo "passed $PASS, failed $FAIL"
[ "$FAIL" -eq 0 ] || exit 1
