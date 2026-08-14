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
#   1. issues.resolve never wrote `resolvedAt`, so the column existed, was
#      indexed, and was null on every resolved issue in the system.
#   2. components.create declared defaultAssigneeId optional against a NOT NULL
#      column, turning a caller-fixable input error into HTTP 500.
#   3. Clearing a timestamp stores an EMPTY STRING rather than SQL NULL, so a
#      reopened issue reads `resolvedAt: ""` and a `!= null` test admits it.
#
# WHAT THIS DOES NOT CATCH. It drives the DEV tier. Identity arrives through
# `dev_auth::resolve_dev_user_json` instead of the gateway's `ZeroShip-User`
# header -- the same `call_fetch_handler_with_user` path underneath, but the
# gateway's own JWT validation, rate limiting and route dispatch are NOT
# exercised here. It also runs against SQLite, so anything Postgres-specific
# (collation, isolation, partial indexes) is out of scope; see
# docs/reference/sqlite-divergences.md.
#
# It DOES run two identities, so all three access controls are verified as
# actual DENIALS rather than assumed from reading the code: issue-level groups,
# product-level groups, and private comments. Every one is paired with a
# control showing the second user could see the thing BEFORE it was restricted
# -- without that, "Bob cannot see it" is equally consistent with Bob never
# having been able to, and the assertion proves nothing.
#
# Bob is also asserted to be a non-admin, because an admin bypasses every
# restriction below and would make the whole section vacuous while still
# printing green.
#
# Covered as a denial with a control: issue-level and product-level groups,
# private comments, report aggregates (a restricted issue used to be counted in
# reports.summary for someone who could not read it), duplicate clusters,
# dependency edges and the graph, the activity history, and attachment content,
# list and obsolete.
#
# WHAT NO ASSERTION HERE COVERS, so it is not mistaken for tested:
#   - the login form itself; every request carries a session cookie this script
#     signs, so the credential exchange is never exercised;
#   - the gateway's JWT validation, rate limiting and route dispatch, because
#     this drives the dev runtime directly;
#   - anything Postgres-specific. Two fixes in this app -- the serializable
#     idempotent joins and the votes.cast lost update -- address failures that
#     SQLite CANNOT produce, because it serialises writes. Those are argued
#     from the code, not observed here.
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
SERVE_PIDS="$(lsof -ti "tcp:$PORT" 2>/dev/null)"
if [ -z "$SERVE_PIDS" ]; then
  echo "nothing is listening on $URL -- start the dev runtime first:  pnpm dev" >&2
  exit 2
fi
# EVERY pid on the port, not the first. Two processes hold this socket -- the
# vite parent and the `zeroship serve` child it spawns -- and only the child
# carries the secret. `head -1` returned whichever lsof happened to list first,
# so this refused to run whenever that was the parent: a suite that passed
# 88/88 one hour and declined to start the next, with nothing about the app
# having changed. e2e/session.ts had the same issue and was fixed; this copy was
# missed.
SECRET=""
CHECKED=""
for pid in $SERVE_PIDS; do
  found="$(tr '\0' '\n' < "/proc/$pid/environ" 2>/dev/null | grep '^ZEROSHIP_DEV_AUTH_SECRET=' | cut -d= -f2-)"
  if [ -n "$found" ]; then
    SECRET="$found"
    break
  fi
  CHECKED="$CHECKED $pid"
done
if [ -z "$SECRET" ]; then
  echo "no process listening on $URL carries ZEROSHIP_DEV_AUTH_SECRET (checked:$CHECKED)." >&2
  echo "That variable is what makes an authenticated dev request possible; without" >&2
  echo "it every mutation below would 401 and the run would report failures that" >&2
  echo "say nothing about the app. Refusing to run a test that cannot pass." >&2
  exit 2
fi
# Parameterised by identity so the visibility section can run a SECOND user.
# One identity can only ever show that access is granted; showing that it is
# denied to somebody needs somebody else.
mint_cookie() {
  node -e '
const { createHmac } = require("crypto");
const [secret, id, email, name] = process.argv.slice(1);
const user = JSON.stringify({
  id, email, name, avatar: null, email_verified: true,
  scopes: ["openid", "profile", "email"],
});
const p = Buffer.from(user).toString("base64url");
process.stdout.write(`__zeroship_dev_session=${p}.${createHmac("sha256", secret).update(p).digest("hex")}`);
' "$SECRET" "$1" "$2" "$3"
}

COOKIE="$(mint_cookie "pws_devalice0000000000" "alice@localhost" "Alice Dev")"
BOB_COOKIE="$(mint_cookie "pws_devbob00000000000" "bob@localhost" "Bob Dev")"

# The empty-args default is spelled with a variable rather than inline in the
# parameter expansion: `${2:-\{\}}` expands to a LITERAL \{\}, which sends
# invalid JSON and comes back as an error the assertion then reads as a missing
# field. Every call that passed an argument worked, so the issue only showed on
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

echo "issue lifecycle"
ISSUE="$(call issues.create "{\"productId\":\"$PROD\",\"componentId\":\"$COMP\",\"versionId\":\"$VER\",\"summary\":\"Parser drops trailing newline\",\"description\":\"repro\",\"severity\":\"major\",\"priority\":\"P2\"}" | jget 'json.id')"
[ "${ISSUE#issu_}" != "$ISSUE" ] && pass "issues.create" || { fail "issues.create" "got: $ISSUE"; exit 1; }

[ "$(call issues.get "{\"id\":\"$ISSUE\"}" | jget 'json.issue.resolvedAt')" = "<null>" ] \
  && pass "a new issue has no resolvedAt" || fail "a new issue already carries resolvedAt"

call comments.add "{\"issueId\":\"$ISSUE\",\"body\":\"confirmed on 1.0\"}" >/dev/null
[ "$(call issues.get "{\"id\":\"$ISSUE\"}" | jget 'json.issue.commentCount')" = "2" ] \
  && pass "commentCount tracks the description plus one comment" \
  || fail "commentCount wrong after comments.add"

# Severity and priority are separate axes in Bugzilla. If a future refactor
# collapses them, this catches it: setting one must not move the other.
call issues.setPriority "{\"id\":\"$ISSUE\",\"priority\":\"P1\"}" >/dev/null
SEV="$(call issues.get "{\"id\":\"$ISSUE\"}" | jget 'json.issue.severity')"
[ "$SEV" = "major" ] && pass "changing priority leaves severity alone" \
  || fail "severity moved when priority changed" "severity=$SEV"

echo "resolution"
call issues.resolve "{\"id\":\"$ISSUE\",\"resolution\":\"FIXED\"}" >/dev/null
STATE="$(call issues.get "{\"id\":\"$ISSUE\"}")"
[ "$(echo "$STATE" | jget 'json.issue.status')" = "RESOLVED" ] && pass "status is RESOLVED" || fail "status after resolve"
[ "$(echo "$STATE" | jget 'json.issue.resolution')" = "FIXED" ] && pass "resolution is FIXED" || fail "resolution after resolve"

# REGRESSION (defect 1): this was null on every resolved issue, which left
# reports.timeToResolve scanning the activities table for status->RESOLVED.
RESOLVED_AT="$(echo "$STATE" | jget 'json.issue.resolvedAt')"
case "$RESOLVED_AT" in
  ''|*[!0-9]*) fail "issues.resolve stamps resolvedAt" "got: $RESOLVED_AT (defect 1 regressing)" ;;
  *)           pass "issues.resolve stamps resolvedAt" ;;
esac

# REGRESSION (defect 1, the consequence): the report must actually count it.
# Asserted as ">= 1" rather than "== 1" because the dev database is not reset
# between runs, so earlier smoke runs legitimately leave resolved issues behind.
COUNT="$(call reports.timeToResolve '{}' | jget 'json.resolvedCount')"
case "$COUNT" in
  ''|*[!0-9]*) fail "reports.timeToResolve returns a count" "got: $COUNT" ;;
  *) [ "$COUNT" -ge 1 ] && pass "reports.timeToResolve counts the resolved issue" \
       || fail "reports.timeToResolve counted nothing" "resolvedCount=$COUNT" ;;
esac

# REGRESSION (defect 3): reopen clears resolvedAt to an EMPTY STRING, not NULL.
# The report filters on typeof number so "" cannot become NaN and poison the
# average. This asserts the average stays a number after a reopen.
call issues.reopen "{\"id\":\"$ISSUE\"}" >/dev/null
AVG="$(call reports.timeToResolve '{}' | jget 'json.averageMs')"
# A mean is legitimately FRACTIONAL. An earlier version of this tested for
# digits only, so it rejected the decimal point and passed purely because the
# sample happened to average to a whole number -- it went red the first time a
# run produced 448.5, reporting an app defect that was not there.
#
# What it actually guards is that an empty resolvedAt never leaks in as NaN,
# so the real assertion is "null, or a finite number".
if [ "$AVG" = "<null>" ] \
  || node -e 'process.exit(Number.isFinite(Number(process.argv[1])) && process.argv[1] !== "" ? 0 : 1)' "$AVG"; then
  pass "averageMs is null or a finite number after a reopen ($AVG)"
else
  fail "averageMs is not finite after a reopen" "got: $AVG (an empty resolvedAt leaked in as NaN)"
fi

# The transition table is enforced by the runtime, not only by the unit tests:
# the issue is now open, so a second reopen must be refused.
[ "$(code issues.reopen "{\"id\":\"$ISSUE\"}")" = "409" ] \
  && pass "reopening an open issue is refused with 409" || fail "second reopen was not refused"

echo "history"
HIST="$(call issues.get "{\"id\":\"$ISSUE\"}" | node -e 'let s="";process.stdin.on("data",d=>s+=d).on("end",()=>{const a=JSON.parse(s).json.activities;process.stdout.write(a.filter(x=>x.fieldName==="status").map(x=>`${x.oldValue}>${x.newValue}`).join(","))})')"
case "$HIST" in
  *"UNCONFIRMED>RESOLVED"*) pass "activity history records the status transition" ;;
  *) fail "status transition missing from history" "status rows: $HIST" ;;
esac


echo "issue-level security groups"
# The whole point of issueGroups: a confidential issue inside a product everyone can
# otherwise read. Until this section existed, issueGroups was a table nothing read
# and assertIssueVisible checked only the product, so a "restricted" issue was
# readable by anyone who could see its product.
bob()  { curl -sS -m 25 -X POST -H 'content-type: application/json' -H "Cookie: $BOB_COOKIE" "$RPC/$1" -d "{\"json\":$(_body "${2-}")}"; }
bobc() { curl -sS -m 25 -o /dev/null -w '%{http_code}' -X POST -H 'content-type: application/json' -H "Cookie: $BOB_COOKIE" "$RPC/$1" -d "{\"json\":$(_body "${2-}")}"; }

SECRET_BUG="$(call issues.create "{\"productId\":\"$PROD\",\"componentId\":\"$COMP\",\"versionId\":\"$VER\",\"summary\":\"Secret $STAMP\",\"description\":\"confidential\"}" | jget 'json.id')"

# Bob is provisioned by his first authenticated call, and is NOT the first
# account, so he must not be an admin -- an admin bypasses every restriction
# below and would make the denial assertions vacuous.
BOB_ID="$(bob users.me | jget "json.id")"
BOB_ADMIN="$(bob users.me | jget 'json.isAdmin')"
[ "$BOB_ADMIN" = "false" ] && pass "the second account is not an admin" \
  || fail "second account is admin=$BOB_ADMIN; restriction assertions below would be vacuous"

# THE CONTROL. Without this, "Bob cannot see it" proves nothing -- he might
# never have been able to.
[ "$(bobc issues.get "{\"id\":\"$SECRET_BUG\"}")" = "200" ] \
  && pass "control: Bob CAN read the issue before it is restricted" \
  || fail "control failed: Bob could not read the issue even unrestricted"

GROUP="$(call groups.create "{\"name\":\"security-$STAMP\",\"description\":\"confidential issues\"}" | jget 'json.id')"
case "$GROUP" in
  grou*|grp_*|*_*) pass "groups.create (the first account bootstraps as admin)" ;;
  *) fail "groups.create" "got: $GROUP -- if this is a 403 the admin bootstrap regressed" ;;
esac

RESTRICT_CODE="$(code issues.restrict "{\"issueId\":\"$SECRET_BUG\",\"groupId\":\"$GROUP\"}")"
[ "$RESTRICT_CODE" = "200" ] && pass "issues.restrict" || fail "issues.restrict" "http=$RESTRICT_CODE"

# The one variable that changed is the restriction.
[ "$(bobc issues.get "{\"id\":\"$SECRET_BUG\"}")" = "403" ] \
  && pass "Bob is refused the restricted issue (403)" \
  || fail "Bob can still read a restricted issue" "http=$(bobc issues.get "{\"id\":\"$SECRET_BUG\"}")"

# Enforcing only on the detail route would still leak summary, status and
# assignee through the list -- most of what a confidential issue is hiding.
BOB_SEES="$(bob issues.search "{\"text\":\"Secret $STAMP\"}" | grep -c "$SECRET_BUG" || true)"
[ "$BOB_SEES" = "0" ] && pass "the restricted issue is absent from Bob's search results" \
  || fail "the restricted issue leaks through issues.search for Bob"

# Alice must still see it, or the restriction is just breakage.
[ "$(code issues.get "{\"id\":\"$SECRET_BUG\"}")" = "200" ] \
  && pass "Alice still reads the issue she restricted" || fail "Alice lost access to her own restricted issue"

# Private comments. comments.list filters on isPrivate for anyone but the
# author, and until now no test drove that path as a different user -- the
# filter was code that read correctly and was never observed working.
PRIV_BUG="$(call issues.create "{\"productId\":\"$PROD\",\"componentId\":\"$COMP\",\"versionId\":\"$VER\",\"summary\":\"Private notes $STAMP\",\"description\":\"public description\"}" | jget 'json.id')"
call comments.add "{\"issueId\":\"$PRIV_BUG\",\"body\":\"PUBLIC-$STAMP\"}" >/dev/null
call comments.add "{\"issueId\":\"$PRIV_BUG\",\"body\":\"SECRET-$STAMP\",\"isPrivate\":true}" >/dev/null

# Control first: Bob must see the public comment, or "Bob cannot see the
# private one" would just mean Bob sees nothing at all.
BOB_COMMENTS="$(bob comments.list "{\"issueId\":\"$PRIV_BUG\"}")"
echo "$BOB_COMMENTS" | grep -q "PUBLIC-$STAMP" \
  && pass "control: Bob sees the public comment on that issue" \
  || fail "control failed: Bob sees no comments at all, so the private check below is vacuous"

echo "$BOB_COMMENTS" | grep -q "SECRET-$STAMP" \
  && fail "a private comment LEAKS to another user through comments.list" \
  || pass "the private comment is withheld from Bob"

# Alice authored it, so she must still see it -- otherwise "private" would just
# mean "lost".
call comments.list "{\"issueId\":\"$PRIV_BUG\"}" | grep -q "SECRET-$STAMP" \
  && pass "the author still sees her own private comment" \
  || fail "the author cannot see her own private comment"

# Product-level restriction (productGroups). Distinct from the issue-level check
# above: this hides an entire product's issues, and it is the older of the two
# mechanisms -- it had RPCs after the last commit but no assertion.
PROD2="$(call products.create "{\"name\":\"Locked $STAMP\",\"description\":\"restricted product\"}" | jget 'json.id')"
COMP2="$(call components.create "{\"productId\":\"$PROD2\",\"name\":\"Core\",\"description\":\"core\"}" | jget 'json.id')"
VER2="$(call versions.create "{\"productId\":\"$PROD2\",\"name\":\"1.0\"}" | jget 'json.id')"
LOCKED_BUG="$(call issues.create "{\"productId\":\"$PROD2\",\"componentId\":\"$COMP2\",\"versionId\":\"$VER2\",\"summary\":\"Locked $STAMP\",\"description\":\"in a restricted product\"}" | jget 'json.id')"

[ "$(bobc issues.get "{\"id\":\"$LOCKED_BUG\"}")" = "200" ] \
  && pass "control: Bob reads the issue before the product is restricted" \
  || fail "control failed: Bob could not read the issue even unrestricted"

call products.restrict "{\"productId\":\"$PROD2\",\"groupId\":\"$GROUP\"}" >/dev/null
[ "$(bobc issues.get "{\"id\":\"$LOCKED_BUG\"}")" = "403" ] \
  && pass "Bob is refused an issue in a restricted product (403)" \
  || fail "product restriction does not deny" "http=$(bobc issues.get "{\"id\":\"$LOCKED_BUG\"}")"

bob products.list | grep -q "$PROD2" \
  && fail "the restricted product still appears in Bob's products.list" \
  || pass "the restricted product is absent from Bob's product list"

# A count is a disclosure. reports.* filtered on product visibility only, so a
# issue restricted to a security group was still counted in every aggregate --
# and reports.* are anon-accessible, so the audience was everyone.
BOB_TOTAL_BEFORE="$(bob reports.summary "{\"productId\":\"$PROD\"}" | jget 'json.total')"
SEC2="$(call issues.create "{\"productId\":\"$PROD\",\"componentId\":\"$COMP\",\"versionId\":\"$VER\",\"summary\":\"Counted $STAMP\",\"description\":\"x\"}" | jget 'json.id')"
BOB_TOTAL_MID="$(bob reports.summary "{\"productId\":\"$PROD\"}" | jget 'json.total')"

# Control: the new issue must move Bob's count while it is unrestricted, or the
# drop after restricting proves nothing.
[ "$BOB_TOTAL_MID" -gt "$BOB_TOTAL_BEFORE" ] 2>/dev/null \
  && pass "control: an unrestricted issue raises Bob's report total ($BOB_TOTAL_BEFORE -> $BOB_TOTAL_MID)" \
  || fail "control failed: report total did not move when an issue was added" "$BOB_TOTAL_BEFORE -> $BOB_TOTAL_MID"

ALICE_TOTAL_BEFORE="$(call reports.summary "{\"productId\":\"$PROD\"}" | jget 'json.total')"
call issues.restrict "{\"issueId\":\"$SEC2\",\"groupId\":\"$GROUP\"}" >/dev/null
ALICE_TOTAL_AFTER="$(call reports.summary "{\"productId\":\"$PROD\"}" | jget 'json.total')"
BOB_TOTAL_AFTER="$(bob reports.summary "{\"productId\":\"$PROD\"}" | jget 'json.total')"
[ "$BOB_TOTAL_AFTER" = "$BOB_TOTAL_BEFORE" ] \
  && pass "restricting the issue removes it from Bob's report total again" \
  || fail "a restricted issue is still counted in reports for a user who cannot read it" \
          "before=$BOB_TOTAL_BEFORE after-restrict=$BOB_TOTAL_AFTER (expected them equal)"

# Alice keeps counting it, or the fix is just breakage.
#
# Compared against ALICE's own before/after, not against Bob's number. Earlier
# sections leave other restricted issues in this product, so Alice's total is
# legitimately higher than any of Bob's -- an earlier version of this assertion
# equated the two and failed on correct behaviour.
[ "$ALICE_TOTAL_BEFORE" = "$ALICE_TOTAL_AFTER" ] \
  && pass "Alice's report total is unchanged by the restriction ($ALICE_TOTAL_AFTER)" \
  || fail "Alice lost the restricted issue from her own reports" \
          "before=$ALICE_TOTAL_BEFORE after=$ALICE_TOTAL_AFTER"

# READ AND WRITE ARE THE SAME PERMISSION. Every issue mutation used to call the
# product-level check only, so issue-level restriction guarded reads and nothing
# else: measured before the fix, Bob got 403 from issues.get on the restricted issue
# and 200 from comments.add on that same issue in the same session. Commenting,
# resolving, reassigning, CC'ing and touching attachments were all reachable on
# an issue he could not open.
#
# Control first: Bob must be able to comment on an UNRESTRICTED issue, or the
# refusals below would just mean Bob cannot comment at all.
[ "$(bobc comments.add "{\"issueId\":\"$ISSUE\",\"body\":\"bob comments $STAMP\"}")" = "200" ] \
  && pass "control: Bob can comment on an unrestricted issue" \
  || fail "control failed: Bob cannot comment even on an open issue"

for probe in "comments.add:{\"issueId\":\"$SECRET_BUG\",\"body\":\"x\"}" \
             "issues.resolve:{\"id\":\"$SECRET_BUG\",\"resolution\":\"FIXED\"}" \
             "issues.reassign:{\"id\":\"$SECRET_BUG\",\"assigneeId\":null}"; do
  proc="${probe%%:*}"; body="${probe#*:}"
  got="$(bobc "$proc" "$body")"
  [ "$got" = "403" ] \
    && pass "Bob cannot $proc on an issue he cannot read" \
    || fail "Bob can $proc on a restricted issue" "http=$got (200 here is the read/write split regressing)"
done

# Dependency edges are disclosures. deps.add checked the edited issue at the
# issue level but the DEPENDENCY at the product level only, so an edge could be
# pointed at a restricted issue the caller cannot open -- confirming it exists
# and naming it in the tree. deps.tree/graph filtered on product visibility
# alone for the same reason.
[ "$(bobc deps.add "{\"issueId\":\"$ISSUE\",\"dependsOnId\":\"$SECRET_BUG\"}")" = "403" ] \
  && pass "Bob cannot point a dependency at an issue he cannot read" \
  || fail "deps.add accepts an edge to a restricted issue" \
          "http=$(bobc deps.add "{\"issueId\":\"$ISSUE\",\"dependsOnId\":\"$SECRET_BUG\"}")"

# Control: Alice CAN create an edge to that same restricted issue -- from a
# DIFFERENT source issue, so it does not collide with the probe above. An
# earlier version reused the same pair, and under mutation Bob's edge landed
# first, making the control fail with "already exists" and report a cascade
# rather than an independent result.
# access and not about the edge being rejected for some unrelated reason.
[ "$(code deps.add "{\"issueId\":\"$PRIV_BUG\",\"dependsOnId\":\"$SECRET_BUG\"}")" = "200" ] \
  && pass "control: Alice can create the same edge" \
  || fail "control failed: even Alice cannot create the edge, so the refusal proves nothing"

# With that edge in place, the restricted issue must not surface as a node in
# Bob's graph.
bob deps.graph | grep -q "$SECRET_BUG" \
  && fail "the restricted issue appears as a node in Bob's dependency graph" \
  || pass "the restricted issue is absent from Bob's dependency graph"

# Shared saved searches must run under the VIEWER's permissions, never the
# owner's. The design satisfies this by construction -- savedSearches.list
# returns the stored queryJson but never EXECUTES it, and execution goes
# through search.query under the caller's own identity -- so this asserts the
# behaviour rather than the mechanism, and would catch a future "run it for
# them server-side" convenience.
SS_QUERY='{"field":"productId","operator":"eq","value":"'"$PROD"'"}'
call savedSearches.save "{\"name\":\"shared-$STAMP\",\"queryJson\":$SS_QUERY,\"isShared\":true}" >/dev/null

bob savedSearches.list | grep -q "shared-$STAMP" \
  && pass "Bob can see Alice's shared saved search" \
  || fail "the shared saved search is not visible to Bob, so the check below is vacuous"

# Alice's own run of that query sees the restricted issue; Bob's must not.
call search.query "{\"where\":$SS_QUERY,\"limit\":200}" | grep -q "$SECRET_BUG" \
  && pass "control: the shared query does match the restricted issue for its owner" \
  || fail "control failed: the query does not match the restricted issue even for Alice"

bob search.query "{\"where\":$SS_QUERY,\"limit\":200}" | grep -q "$SECRET_BUG" \
  && fail "a shared saved search runs with the OWNER's visibility for Bob" \
  || pass "the shared search runs under Bob's own visibility"

# The advanced-search compiler takes a caller-supplied clause tree, so it is
# an untrusted input surface: fields are whitelisted rather than passed
# through to the query builder.
[ "$(code search.query '{"where":{"field":"passwordHash","operator":"eq","value":"x"}}')" = "400" ] \
  && pass "an unsupported search field is rejected (400)" \
  || fail "search.query accepted a field outside the whitelist"

echo "voting"
# The votes table and the three product columns existed with ZERO server
# references: the schema described a feature the app did not have.
VP="$(call products.create "{\"name\":\"Voting $STAMP\",\"description\":\"vote product\"}" | jget 'json.id')"
VC="$(call components.create "{\"productId\":\"$VP\",\"name\":\"Core\",\"description\":\"c\"}" | jget 'json.id')"
VV="$(call versions.create "{\"productId\":\"$VP\",\"name\":\"1.0\"}" | jget 'json.id')"
VB="$(call issues.create "{\"productId\":\"$VP\",\"componentId\":\"$VC\",\"versionId\":\"$VV\",\"summary\":\"Vote me $STAMP\",\"description\":\"d\"}" | jget 'json.id')"

# Voting is OFF by default (all three columns default to 0), which is the
# whole reason 0 is the default rather than something permissive.
[ "$(code votes.cast "{\"issueId\":\"$VB\",\"count\":1}")" = "409" ] \
  && pass "voting is refused while the product has no vote budget" \
  || fail "votes.cast succeeded on a product with voting disabled"

# Asserted, not discarded: an update that silently failed here would make
# every voting check below fail for a reason that has nothing to do with votes.
UPD="$(code products.update "{\"id\":\"$VP\",\"changes\":{\"votesPerUser\":5,\"maxVotesPerIssue\":3,\"votesToConfirm\":2}}")"
[ "$UPD" = "200" ] && pass "the product vote limits are editable" || fail "products.update rejected the vote limits" "http=$UPD"

[ "$(code votes.cast "{\"issueId\":\"$VB\",\"count\":4}")" = "400" ] \
  && pass "a vote above maxVotesPerIssue is refused" \
  || fail "maxVotesPerIssue is not enforced"

VR="$(call votes.cast "{\"issueId\":\"$VB\",\"count\":2}")"
[ "$(echo "$VR" | jget 'json.voteCount')" = "2" ] \
  && pass "voteCount is the SUM of vote quantities, not a row count" \
  || fail "voteCount wrong after casting 2 votes" "$(echo "$VR" | head -c 120)"

# Bugzilla's auto-confirm: votesToConfirm=2 and the issue was UNCONFIRMED.
[ "$(echo "$VR" | jget 'json.confirmed')" = "true" ] \
  && pass "reaching votesToConfirm confirms an UNCONFIRMED issue" \
  || fail "the issue was not auto-confirmed at the vote threshold"
[ "$(call issues.get "{\"id\":\"$VB\"}" | jget 'json.issue.status')" = "CONFIRMED" ] \
  && pass "the auto-confirmed status is persisted" || fail "status did not persist as CONFIRMED"

echo "notifications and watching"
# The notifications table had three READ procedures and no writer, so the inbox
# was permanently empty and the nav's unread badge could never appear.
# Bob is CC'd on the issue, so a comment by Alice must reach him.
# Re-captured HERE, not reused from the top of the file. `users.me` does not
# provision: it returns id null for a user who has never written, and Bob only
# gets a row when he first writes -- which happens further down. Reusing the
# early capture CC'd nobody, so no notification was ever sent. An accumulated
# dev database hid this, because Bob already existed from an earlier run.
BOB_ID="$(bob users.me | jget 'json.id')"
case "$BOB_ID" in
  user_*) pass "Bob is provisioned before being CC'd" ;;
  *) fail "Bob has no app user row yet ($BOB_ID); the CC below would target nobody" ;;
esac
call cc.add "{\"issueId\":\"$ISSUE\",\"userId\":\"$BOB_ID\"}" >/dev/null
BOB_BEFORE="$(bob notifications.unreadCount | jget 'json.count')"
call comments.add "{\"issueId\":\"$ISSUE\",\"body\":\"ping $STAMP\"}" >/dev/null
BOB_AFTER="$(bob notifications.unreadCount | jget 'json.count')"
[ "$BOB_AFTER" -gt "$BOB_BEFORE" ] 2>/dev/null \
  && pass "a comment notifies the CC'd user ($BOB_BEFORE -> $BOB_AFTER)" \
  || fail "no notification reached the CC'd user" "before=$BOB_BEFORE after=$BOB_AFTER"

# The actor does not notify herself.
ALICE_BEFORE="$(call notifications.unreadCount | jget 'json.count')"
call comments.add "{\"issueId\":\"$ISSUE\",\"body\":\"self $STAMP\"}" >/dev/null
[ "$(call notifications.unreadCount | jget 'json.count')" = "$ALICE_BEFORE" ] \
  && pass "the actor is not notified about her own change" \
  || fail "the actor notified herself"

# A restricted issue must not notify someone who cannot read it -- the title
# carries the summary, so a notification is a disclosure.
SEC_BEFORE="$(bob notifications.unreadCount | jget 'json.count')"
call comments.add "{\"issueId\":\"$SECRET_BUG\",\"body\":\"secret note $STAMP\"}" >/dev/null
[ "$(bob notifications.unreadCount | jget 'json.count')" = "$SEC_BEFORE" ] \
  && pass "no notification about an issue the recipient cannot read" \
  || fail "a restricted issue's summary leaked through a notification"

# The unread count is cached in KV and written by notifications.markRead, so a
# user who has EVER marked something read has a warm cache and the
# authoritative database fallback never runs for them. A fanout that inserts a
# row without refreshing that cache leaves them looking at a stale number.
# This sequence is specifically markRead-then-notify, which is the only order
# that exposes it.
FIRST_NOTIF="$(bob notifications.list '{"limit":1}' | jget 'json.0.id')"
[ -n "$FIRST_NOTIF" ] && bob notifications.markRead "{\"id\":\"$FIRST_NOTIF\"}" >/dev/null
WARM="$(bob notifications.unreadCount | jget 'json.count')"
call comments.add "{\"issueId\":\"$ISSUE\",\"body\":\"cache probe $STAMP\"}" >/dev/null
AFTER_WARM="$(bob notifications.unreadCount | jget 'json.count')"
[ "$AFTER_WARM" -gt "$WARM" ] 2>/dev/null \
  && pass "the unread count is fresh after a notification even with a warm cache ($WARM -> $AFTER_WARM)" \
  || fail "stale unread count: the KV cache was not refreshed by the fanout" "warm=$WARM after=$AFTER_WARM"


# Fanout used to live at call sites, and so covered 2 of the 10 mutations that
# change an issue. issues.reassign was silent -- a new assignee was never told they
# had been given an issue, which is the most useful notification a tracker sends.
REASSIGN_BEFORE="$(bob notifications.unreadCount | jget 'json.count')"
call issues.reassign "{\"id\":\"$ISSUE\",\"assigneeId\":\"$BOB_ID\"}" >/dev/null
[ "$(bob notifications.unreadCount | jget 'json.count')" -gt "$REASSIGN_BEFORE" ] 2>/dev/null \
  && pass "reassigning an issue notifies the new assignee" \
  || fail "issues.reassign sent no notification to the new assignee" \
          "before=$REASSIGN_BEFORE after=$(bob notifications.unreadCount | jget 'json.count')"

# A no-op update must NOT wake anyone: the fanout is driven by the fields that
# actually differed, not by the call happening.
NOOP_BEFORE="$(bob notifications.unreadCount | jget 'json.count')"
call issues.reassign "{\"id\":\"$ISSUE\",\"assigneeId\":\"$BOB_ID\"}" >/dev/null
[ "$(bob notifications.unreadCount | jget 'json.count')" = "$NOOP_BEFORE" ] \
  && pass "re-assigning to the same user notifies nobody" \
  || fail "a no-op update sent a notification"

# Two list surfaces that were retrofitted with hiddenIssueIds one at a time, and
# these two were missed because they do not share the search code path.
#
# A duplicate cluster is the classic route to a confidential issue: a PUBLIC issue
# marked as a duplicate of a restricted one pulled the restricted issue's whole
# row into dupes.list for anyone who could see the public one.
DUP_PUBLIC="$(call issues.create "{\"productId\":\"$PROD\",\"componentId\":\"$COMP\",\"versionId\":\"$VER\",\"summary\":\"Dup of secret $STAMP\",\"description\":\"d\"}" | jget 'json.id')"
call issues.markDuplicate "{\"id\":\"$DUP_PUBLIC\",\"duplicateOfId\":\"$SECRET_BUG\"}" >/dev/null

# Control: Alice, who can see the restricted issue, DOES get it in the cluster.
call dupes.list "{\"issueId\":\"$DUP_PUBLIC\"}" | grep -q "$SECRET_BUG" \
  && pass "control: the duplicate cluster contains the restricted issue for Alice" \
  || fail "control failed: the cluster does not contain it even for Alice"

bob dupes.list "{\"issueId\":\"$DUP_PUBLIC\"}" | grep -q "$SECRET_BUG" \
  && fail "dupes.list leaks a restricted issue through a public duplicate" \
  || pass "the restricted issue is absent from Bob's duplicate cluster"

# ...and the existence oracle: asking directly about the restricted issue must
# 404 for Bob rather than answering.
[ "$(bobc dupes.list "{\"issueId\":\"$SECRET_BUG\"}")" = "404" ] \
  && pass "dupes.list does not confirm a restricted issue id exists" \
  || fail "dupes.list answers for a restricted issue id (existence oracle)"

# cc.listMine: restricting an issue does not clear its CC list, so a user CC'd
# before the restriction kept reading the row here while issues.get 403s.
call cc.add "{\"issueId\":\"$SECRET_BUG\",\"userId\":\"$BOB_ID\"}" >/dev/null
bob cc.listMine | grep -q "$SECRET_BUG" \
  && fail "cc.listMine returns an issue the caller cannot read" \
  || pass "cc.listMine withholds an issue restricted after the CC"

# Both ends of a dependency edge are checked on REMOVE as well as add.
# Checking only the near end made this an existence oracle (404 "Dependency"
# vs success distinguishes a real restricted issue from a nonexistent id) and
# let a public-side caller delete a restricted issue's blocker bookkeeping.
[ "$(bobc deps.remove "{\"issueId\":\"$ISSUE\",\"dependsOnId\":\"$SECRET_BUG\"}")" = "403" ] \
  && pass "deps.remove refuses an edge whose far end is unreadable" \
  || fail "deps.remove checks only the near end of the edge"

# Removing a restriction requires belonging to the group, exactly as adding
# one does. Bob is not in the group and cannot see the issue, so this is 403
# either way; the sharper case (a member of another group stripping this one)
# needs a second group and is left to the reader.
[ "$(bobc issues.unrestrict "{\"issueId\":\"$SECRET_BUG\",\"groupId\":\"$GROUP\"}")" = "403" ] \
  && pass "issues.unrestrict refuses a non-member" \
  || fail "issues.unrestrict let a non-member strip a restriction"

echo "moving an issue between products"
# This procedure could never succeed: it refused any issue with a version, and
# issues.versionId is NOT NULL, so that was every issue. It is exported and
# policy-listed, and no test had ever driven it.
MV_SRC="$(call products.create "{\"name\":\"MoveA $STAMP\",\"description\":\"a\"}" | jget 'json.id')"
MV_SC="$(call components.create "{\"productId\":\"$MV_SRC\",\"name\":\"Core\",\"description\":\"c\"}" | jget 'json.id')"
MV_SV="$(call versions.create "{\"productId\":\"$MV_SRC\",\"name\":\"1.0\"}" | jget 'json.id')"
MV_BUG="$(call issues.create "{\"productId\":\"$MV_SRC\",\"componentId\":\"$MV_SC\",\"versionId\":\"$MV_SV\",\"summary\":\"Movable $STAMP\",\"description\":\"d\"}" | jget 'json.id')"

MV_DST="$(call products.create "{\"name\":\"MoveB $STAMP\",\"description\":\"b\"}" | jget 'json.id')"
MV_DC="$(call components.create "{\"productId\":\"$MV_DST\",\"name\":\"Core\",\"description\":\"c\"}" | jget 'json.id')"

# The target has no "1.0" yet, so the move must be refused with a message that
# says what to do -- not silently carry a version belonging to another product.
[ "$(code issues.move "{\"id\":\"$MV_BUG\",\"productId\":\"$MV_DST\",\"componentId\":\"$MV_DC\"}")" = "409" ] \
  && pass "moving to a product without a matching version is refused" \
  || fail "an issue moved into a product that has no matching version"

call versions.create "{\"productId\":\"$MV_DST\",\"name\":\"1.0\"}" >/dev/null
MV_RESULT="$(call issues.move "{\"id\":\"$MV_BUG\",\"productId\":\"$MV_DST\",\"componentId\":\"$MV_DC\"}")"
[ "$(echo "$MV_RESULT" | jget 'json.productId')" = "$MV_DST" ] \
  && pass "the issue moves once the target has the same version name" \
  || fail "issues.move did not move the issue" "$(echo "$MV_RESULT" | head -c 120)"

# The version must be REMAPPED to the target product's row, not carried over:
# versions are product-scoped, so keeping the old id would leave the issue
# pointing into the product it left.
MV_NEWV="$(echo "$MV_RESULT" | jget 'json.versionId')"
[ -n "$MV_NEWV" ] && [ "$MV_NEWV" != "$MV_SV" ] \
  && pass "the version is remapped to the target product's own row" \
  || fail "the issue kept the source product's version id" "was=$MV_SV now=$MV_NEWV"

# The user directory returned the RAW row to any authenticated caller: email,
# isAdmin, isDisabled, and prefs -- a free-form bag holding whatever that user
# stored. reports.byAssignee already projected {id, handle, name}, so these two
# were the outliers.
ALICE_ID="$(call users.me | jget 'json.id')"
bob users.get "{\"id\":\"$ALICE_ID\"}" | grep -q "alice@localhost" \
  && fail "users.get discloses another user's email" \
  || pass "users.get withholds another user's email"

# Control: the endpoint still ANSWERS -- the name is there, so the absence of
# the email is a projection and not a denial.
bob users.get "{\"id\":\"$ALICE_ID\"}" | grep -q "Alice Dev" \
  && pass "control: users.get still returns the display name" \
  || fail "users.get returned nothing, so the check above proves nothing"

# The people picker must still FIND someone by email without RETURNING it:
# matching on a value is not the same as disclosing it.
BOB_SEARCH="$(bob users.list '{"text":"alice@localhost"}')"
echo "$BOB_SEARCH" | grep -q "Alice Dev" \
  && pass "users.list still matches on email so a picker can find people" \
  || fail "users.list can no longer find a user by email"
echo "$BOB_SEARCH" | grep -q "alice@localhost" \
  && fail "users.list echoes the email it matched on" \
  || pass "users.list matches on email without returning it"

# Self is unprojected: users.me is how you read your own record.
call users.me | grep -q "alice@localhost" \
  && pass "users.me still returns the caller's own email" \
  || fail "users.me stopped returning the caller's own email"

# The mask that stops a public row NAMING a restricted issue was applied in
# dupes.list only, while issues.get and issues.search -- both ANONYMOUS -- returned
# duplicateOfId untouched. A confidential issue exists to hide its existence, and
# its id leaked through the two most-used read paths.
MASK_PUB="$(call issues.create "{\"productId\":\"$PROD\",\"componentId\":\"$COMP\",\"versionId\":\"$VER\",\"summary\":\"Mask probe $STAMP\",\"description\":\"d\"}" | jget 'json.id')"
call issues.markDuplicate "{\"id\":\"$MASK_PUB\",\"duplicateOfId\":\"$SECRET_BUG\"}" >/dev/null

# Control: Alice CAN see the link, so its absence for others is a mask and not
# a failed write.
[ "$(call issues.get "{\"id\":\"$MASK_PUB\"}" | jget 'json.issue.duplicateOfId')" = "$SECRET_BUG" ] \
  && pass "control: the duplicate link is visible to someone who can read both" \
  || fail "control failed: even Alice cannot see the duplicate link"

anon_body() { curl -sS -m 25 -X POST -H 'content-type: application/json' "$RPC/$1" -d "{\"json\":$(_body "${2-}")}"; }
anon_body issues.get "{\"id\":\"$MASK_PUB\"}" | grep -q "$SECRET_BUG" \
  && fail "issues.get discloses a restricted issue id to an anonymous caller" \
  || pass "issues.get masks the restricted duplicate link for anonymous callers"

anon_body issues.search "{\"text\":\"Mask probe $STAMP\"}" | grep -q "$SECRET_BUG" \
  && fail "issues.search discloses a restricted issue id to an anonymous caller" \
  || pass "issues.search masks the restricted duplicate link"

# The activity stream is permanent and carries the same id in its VALUES, so an
# unfiltered history re-leaks what the row-level mask withholds.
#
# Asserted on the activities array specifically. An earlier version grepped the
# whole response for the string "duplicateOfId" -- which is a legitimate FIELD
# NAME on every issue row -- and so reported a leak that was not there.
if anon_body issues.get "{\"id\":\"$MASK_PUB\"}" | node -e 'let s="";process.stdin.on("data",d=>s+=d).on("end",()=>{const a=JSON.parse(s).json?.activities??[];process.exit(a.some((r)=>[r.oldValue,r.newValue].includes(process.argv[1]))?1:0)})' "$SECRET_BUG"; then
  pass "the activity history withholds the restricted reference"
else
  fail "an activity row still carries the restricted issue id in its value"
fi

# Marking a duplicate is a closure like issues.resolve, and was the one closure
# that notified nobody.
DUP_NOTIFY_BEFORE="$(bob notifications.unreadCount | jget 'json.count')"
DUP_SRC="$(call issues.create "{\"productId\":\"$PROD\",\"componentId\":\"$COMP\",\"versionId\":\"$VER\",\"summary\":\"Dup notify $STAMP\",\"description\":\"d\"}" | jget 'json.id')"
call cc.add "{\"issueId\":\"$DUP_SRC\",\"userId\":\"$BOB_ID\"}" >/dev/null
call issues.markDuplicate "{\"id\":\"$DUP_SRC\",\"duplicateOfId\":\"$ISSUE\"}" >/dev/null
[ "$(bob notifications.unreadCount | jget 'json.count')" -gt "$DUP_NOTIFY_BEFORE" ] 2>/dev/null \
  && pass "marking a duplicate notifies the CC'd user" \
  || fail "issues.markDuplicate closed an issue and told nobody"

# Idempotence is the contract these joins promise, so it is asserted rather
# than assumed: adding the same CC, keyword or link twice must be a no-op that
# returns the existing row, not a second row and not an error.
IDEM_BEFORE="$(call cc.list "{\"issueId\":\"$ISSUE\"}" | node -e 'let s="";process.stdin.on("data",d=>s+=d).on("end",()=>process.stdout.write(String((JSON.parse(s).json||[]).length)))')"
call cc.add "{\"issueId\":\"$ISSUE\",\"userId\":\"$BOB_ID\"}" >/dev/null
call cc.add "{\"issueId\":\"$ISSUE\",\"userId\":\"$BOB_ID\"}" >/dev/null
IDEM_AFTER="$(call cc.list "{\"issueId\":\"$ISSUE\"}" | node -e 'let s="";process.stdin.on("data",d=>s+=d).on("end",()=>process.stdout.write(String((JSON.parse(s).json||[]).length)))')"
[ "$IDEM_AFTER" -le "$((IDEM_BEFORE + 1))" ] 2>/dev/null \
  && pass "cc.add twice adds at most one row ($IDEM_BEFORE -> $IDEM_AFTER)" \
  || fail "cc.add is not idempotent" "before=$IDEM_BEFORE after=$IDEM_AFTER"

[ "$(code cc.add "{\"issueId\":\"$ISSUE\",\"userId\":\"$BOB_ID\"}")" = "200" ] \
  && pass "a repeated cc.add still answers 200" \
  || fail "a repeated cc.add errored instead of returning the existing row"

# Attachment CONTENT as a second identity -- the last gap this file's own
# header named. attachments.list/get call assertIssueVisible and
# setObsolete/delete call assertIssueAccessible, but nothing pinned that the
# BYTES are actually withheld from someone who cannot read the issue.
ATT_B64="$(node -e 'process.stdout.write(Buffer.from("secret patch contents").toString("base64"))')"
ATT_ID="$(call attachments.upload "{\"issueId\":\"$SECRET_BUG\",\"filename\":\"fix.patch\",\"contentBase64\":\"$ATT_B64\",\"contentType\":\"text/plain\",\"isPatch\":true}" | jget 'json.id')"
case "$ATT_ID" in
  atch_*|att_*|*_*) pass "an attachment uploads to the restricted issue" ;;
  *) fail "attachments.upload" "got: $ATT_ID" ;;
esac

# Control: Alice can read the bytes back, so a refusal for Bob is about access
# and not about the upload having failed.
call attachments.get "{\"id\":\"$ATT_ID\"}" | grep -q "$ATT_B64" \
  && pass "control: the uploader reads the attachment bytes back" \
  || fail "control failed: even the uploader cannot read the bytes"

[ "$(bobc attachments.get "{\"id\":\"$ATT_ID\"}")" = "403" ] \
  && pass "attachment CONTENT is refused to a user who cannot read the issue" \
  || fail "attachment content is readable by a user who cannot read the issue" \
          "http=$(bobc attachments.get "{\"id\":\"$ATT_ID\"}")"

[ "$(bobc attachments.list "{\"issueId\":\"$SECRET_BUG\"}")" = "403" ] \
  && pass "the attachment LIST is refused too (filenames are a disclosure)" \
  || fail "attachments.list answered for an issue the caller cannot read"

# And the write side: marking someone else's attachment obsolete on an issue you
# cannot open must be refused, not just the read.
[ "$(bobc attachments.setObsolete "{\"id\":\"$ATT_ID\",\"isObsolete\":true}")" = "403" ] \
  && pass "attachments.setObsolete is refused on an unreadable issue" \
  || fail "a user could obsolete an attachment on an issue they cannot read"
echo "see also"
# The issueSeeAlso table had zero server references: schema described the
# feature, nothing implemented it.
SA="$(call seeAlso.add "{\"issueId\":\"$ISSUE\",\"url\":\"https://bugzilla.mozilla.org/show_bug.cgi?id=1\"}" | jget 'json.id')"
[ -n "$SA" ] && [ "$SA" != "<missing>" ] && pass "seeAlso.add stores an external link" || fail "seeAlso.add" "got: $SA"
call seeAlso.list "{\"issueId\":\"$ISSUE\"}" | grep -q "bugzilla.mozilla.org" \
  && pass "seeAlso.list returns the link" || fail "seeAlso.list did not return the link"

# This value is rendered as an href, so a javascript: URL is script execution
# rather than a link. Rejected at the API, not left to the client to sanitise.
[ "$(code seeAlso.add "{\"issueId\":\"$ISSUE\",\"url\":\"javascript:alert(1)\"}")" = "400" ] \
  && pass "a javascript: url is refused" || fail "a javascript: url was accepted as a See Also link"
[ "$(code seeAlso.add "{\"issueId\":\"$ISSUE\",\"url\":\"not-a-url\"}")" = "400" ] \
  && pass "a relative url is refused" || fail "a non-absolute url was accepted"

# Duplicate add is idempotent rather than a second row.
call seeAlso.add "{\"issueId\":\"$ISSUE\",\"url\":\"https://bugzilla.mozilla.org/show_bug.cgi?id=1\"}" >/dev/null
[ "$(call seeAlso.list "{\"issueId\":\"$ISSUE\"}" | grep -o 'bugzilla.mozilla.org' | wc -l)" = "1" ] \
  && pass "adding the same link twice does not duplicate it" || fail "duplicate See Also rows"
echo "auth posture"
# Fail-closed: a write with no identity must be refused, not silently accepted.
[ "$(anon products.create '{"name":"nope","description":"nope"}')" = "401" ] \
  && pass "an unauthenticated write is refused with 401" || fail "an unauthenticated write was NOT refused"
[ "$(anon products.list '{}')" = "200" ] \
  && pass "an anonymous read is allowed" || fail "an anonymous read was refused"

echo
echo "passed $PASS, failed $FAIL"
[ "$FAIL" -eq 0 ] || exit 1
