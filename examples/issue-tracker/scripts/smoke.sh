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
# docs/reference/sqlite-divergences.md.
#
# It DOES run two identities, so all three access controls are verified as
# actual DENIALS rather than assumed from reading the code: bug-level groups,
# product-level groups, and private comments. Every one is paired with a
# control showing the second user could see the thing BEFORE it was restricted
# -- without that, "Bob cannot see it" is equally consistent with Bob never
# having been able to, and the assertion proves nothing.
#
# Bob is also asserted to be a non-admin, because an admin bypasses every
# restriction below and would make the whole section vacuous while still
# printing green.
#
# Attachment paths go through the same bug check (list/get via assertBugVisible,
# setObsolete/delete via assertBugAccessible), but no assertion here pins
# attachment CONTENT specifically -- that is the remaining second-identity gap.
# Report aggregates ARE covered -- a restricted bug used to be counted in
# reports.summary for a user who could not read it, and that is now pinned.
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


echo "bug-level security groups"
# The whole point of bugGroups: a confidential bug inside a product everyone can
# otherwise read. Until this section existed, bugGroups was a table nothing read
# and assertBugVisible checked only the product, so a "restricted" bug was
# readable by anyone who could see its product.
bob()  { curl -sS -m 25 -X POST -H 'content-type: application/json' -H "Cookie: $BOB_COOKIE" "$RPC/$1" -d "{\"json\":$(_body "${2-}")}"; }
bobc() { curl -sS -m 25 -o /dev/null -w '%{http_code}' -X POST -H 'content-type: application/json' -H "Cookie: $BOB_COOKIE" "$RPC/$1" -d "{\"json\":$(_body "${2-}")}"; }

SECRET_BUG="$(call bugs.create "{\"productId\":\"$PROD\",\"componentId\":\"$COMP\",\"versionId\":\"$VER\",\"summary\":\"Secret $STAMP\",\"description\":\"confidential\"}" | jget 'json.id')"

# Bob is provisioned by his first authenticated call, and is NOT the first
# account, so he must not be an admin -- an admin bypasses every restriction
# below and would make the denial assertions vacuous.
BOB_ID="$(bob users.me | jget "json.id")"
BOB_ADMIN="$(bob users.me | jget 'json.isAdmin')"
[ "$BOB_ADMIN" = "false" ] && pass "the second account is not an admin" \
  || fail "second account is admin=$BOB_ADMIN; restriction assertions below would be vacuous"

# THE CONTROL. Without this, "Bob cannot see it" proves nothing -- he might
# never have been able to.
[ "$(bobc bugs.get "{\"id\":\"$SECRET_BUG\"}")" = "200" ] \
  && pass "control: Bob CAN read the bug before it is restricted" \
  || fail "control failed: Bob could not read the bug even unrestricted"

GROUP="$(call groups.create "{\"name\":\"security-$STAMP\",\"description\":\"confidential bugs\"}" | jget 'json.id')"
case "$GROUP" in
  grou*|grp_*|*_*) pass "groups.create (the first account bootstraps as admin)" ;;
  *) fail "groups.create" "got: $GROUP -- if this is a 403 the admin bootstrap regressed" ;;
esac

RESTRICT_CODE="$(code bugs.restrict "{\"bugId\":\"$SECRET_BUG\",\"groupId\":\"$GROUP\"}")"
[ "$RESTRICT_CODE" = "200" ] && pass "bugs.restrict" || fail "bugs.restrict" "http=$RESTRICT_CODE"

# The one variable that changed is the restriction.
[ "$(bobc bugs.get "{\"id\":\"$SECRET_BUG\"}")" = "403" ] \
  && pass "Bob is refused the restricted bug (403)" \
  || fail "Bob can still read a restricted bug" "http=$(bobc bugs.get "{\"id\":\"$SECRET_BUG\"}")"

# Enforcing only on the detail route would still leak summary, status and
# assignee through the list -- most of what a confidential bug is hiding.
BOB_SEES="$(bob bugs.search "{\"text\":\"Secret $STAMP\"}" | grep -c "$SECRET_BUG" || true)"
[ "$BOB_SEES" = "0" ] && pass "the restricted bug is absent from Bob's search results" \
  || fail "the restricted bug leaks through bugs.search for Bob"

# Alice must still see it, or the restriction is just breakage.
[ "$(code bugs.get "{\"id\":\"$SECRET_BUG\"}")" = "200" ] \
  && pass "Alice still reads the bug she restricted" || fail "Alice lost access to her own restricted bug"

# Private comments. comments.list filters on isPrivate for anyone but the
# author, and until now no test drove that path as a different user -- the
# filter was code that read correctly and was never observed working.
PRIV_BUG="$(call bugs.create "{\"productId\":\"$PROD\",\"componentId\":\"$COMP\",\"versionId\":\"$VER\",\"summary\":\"Private notes $STAMP\",\"description\":\"public description\"}" | jget 'json.id')"
call comments.add "{\"bugId\":\"$PRIV_BUG\",\"body\":\"PUBLIC-$STAMP\"}" >/dev/null
call comments.add "{\"bugId\":\"$PRIV_BUG\",\"body\":\"SECRET-$STAMP\",\"isPrivate\":true}" >/dev/null

# Control first: Bob must see the public comment, or "Bob cannot see the
# private one" would just mean Bob sees nothing at all.
BOB_COMMENTS="$(bob comments.list "{\"bugId\":\"$PRIV_BUG\"}")"
echo "$BOB_COMMENTS" | grep -q "PUBLIC-$STAMP" \
  && pass "control: Bob sees the public comment on that bug" \
  || fail "control failed: Bob sees no comments at all, so the private check below is vacuous"

echo "$BOB_COMMENTS" | grep -q "SECRET-$STAMP" \
  && fail "a private comment LEAKS to another user through comments.list" \
  || pass "the private comment is withheld from Bob"

# Alice authored it, so she must still see it -- otherwise "private" would just
# mean "lost".
call comments.list "{\"bugId\":\"$PRIV_BUG\"}" | grep -q "SECRET-$STAMP" \
  && pass "the author still sees her own private comment" \
  || fail "the author cannot see her own private comment"

# Product-level restriction (productGroups). Distinct from the bug-level check
# above: this hides an entire product's bugs, and it is the older of the two
# mechanisms -- it had RPCs after the last commit but no assertion.
PROD2="$(call products.create "{\"name\":\"Locked $STAMP\",\"description\":\"restricted product\"}" | jget 'json.id')"
COMP2="$(call components.create "{\"productId\":\"$PROD2\",\"name\":\"Core\",\"description\":\"core\"}" | jget 'json.id')"
VER2="$(call versions.create "{\"productId\":\"$PROD2\",\"name\":\"1.0\"}" | jget 'json.id')"
LOCKED_BUG="$(call bugs.create "{\"productId\":\"$PROD2\",\"componentId\":\"$COMP2\",\"versionId\":\"$VER2\",\"summary\":\"Locked $STAMP\",\"description\":\"in a restricted product\"}" | jget 'json.id')"

[ "$(bobc bugs.get "{\"id\":\"$LOCKED_BUG\"}")" = "200" ] \
  && pass "control: Bob reads the bug before the product is restricted" \
  || fail "control failed: Bob could not read the bug even unrestricted"

call products.restrict "{\"productId\":\"$PROD2\",\"groupId\":\"$GROUP\"}" >/dev/null
[ "$(bobc bugs.get "{\"id\":\"$LOCKED_BUG\"}")" = "403" ] \
  && pass "Bob is refused a bug in a restricted product (403)" \
  || fail "product restriction does not deny" "http=$(bobc bugs.get "{\"id\":\"$LOCKED_BUG\"}")"

bob products.list | grep -q "$PROD2" \
  && fail "the restricted product still appears in Bob's products.list" \
  || pass "the restricted product is absent from Bob's product list"

# A count is a disclosure. reports.* filtered on product visibility only, so a
# bug restricted to a security group was still counted in every aggregate --
# and reports.* are anon-accessible, so the audience was everyone.
BOB_TOTAL_BEFORE="$(bob reports.summary "{\"productId\":\"$PROD\"}" | jget 'json.total')"
SEC2="$(call bugs.create "{\"productId\":\"$PROD\",\"componentId\":\"$COMP\",\"versionId\":\"$VER\",\"summary\":\"Counted $STAMP\",\"description\":\"x\"}" | jget 'json.id')"
BOB_TOTAL_MID="$(bob reports.summary "{\"productId\":\"$PROD\"}" | jget 'json.total')"

# Control: the new bug must move Bob's count while it is unrestricted, or the
# drop after restricting proves nothing.
[ "$BOB_TOTAL_MID" -gt "$BOB_TOTAL_BEFORE" ] 2>/dev/null \
  && pass "control: an unrestricted bug raises Bob's report total ($BOB_TOTAL_BEFORE -> $BOB_TOTAL_MID)" \
  || fail "control failed: report total did not move when a bug was added" "$BOB_TOTAL_BEFORE -> $BOB_TOTAL_MID"

ALICE_TOTAL_BEFORE="$(call reports.summary "{\"productId\":\"$PROD\"}" | jget 'json.total')"
call bugs.restrict "{\"bugId\":\"$SEC2\",\"groupId\":\"$GROUP\"}" >/dev/null
ALICE_TOTAL_AFTER="$(call reports.summary "{\"productId\":\"$PROD\"}" | jget 'json.total')"
BOB_TOTAL_AFTER="$(bob reports.summary "{\"productId\":\"$PROD\"}" | jget 'json.total')"
[ "$BOB_TOTAL_AFTER" = "$BOB_TOTAL_BEFORE" ] \
  && pass "restricting the bug removes it from Bob's report total again" \
  || fail "a restricted bug is still counted in reports for a user who cannot read it" \
          "before=$BOB_TOTAL_BEFORE after-restrict=$BOB_TOTAL_AFTER (expected them equal)"

# Alice keeps counting it, or the fix is just breakage.
#
# Compared against ALICE's own before/after, not against Bob's number. Earlier
# sections leave other restricted bugs in this product, so Alice's total is
# legitimately higher than any of Bob's -- an earlier version of this assertion
# equated the two and failed on correct behaviour.
[ "$ALICE_TOTAL_BEFORE" = "$ALICE_TOTAL_AFTER" ] \
  && pass "Alice's report total is unchanged by the restriction ($ALICE_TOTAL_AFTER)" \
  || fail "Alice lost the restricted bug from her own reports" \
          "before=$ALICE_TOTAL_BEFORE after=$ALICE_TOTAL_AFTER"

# READ AND WRITE ARE THE SAME PERMISSION. Every bug mutation used to call the
# product-level check only, so bug-level restriction guarded reads and nothing
# else: measured before the fix, Bob got 403 from bugs.get on the restricted bug
# and 200 from comments.add on that same bug in the same session. Commenting,
# resolving, reassigning, CC'ing and touching attachments were all reachable on
# a bug he could not open.
#
# Control first: Bob must be able to comment on an UNRESTRICTED bug, or the
# refusals below would just mean Bob cannot comment at all.
[ "$(bobc comments.add "{\"bugId\":\"$BUG\",\"body\":\"bob comments $STAMP\"}")" = "200" ] \
  && pass "control: Bob can comment on an unrestricted bug" \
  || fail "control failed: Bob cannot comment even on an open bug"

for probe in "comments.add:{\"bugId\":\"$SECRET_BUG\",\"body\":\"x\"}" \
             "bugs.resolve:{\"id\":\"$SECRET_BUG\",\"resolution\":\"FIXED\"}" \
             "bugs.reassign:{\"id\":\"$SECRET_BUG\",\"assigneeId\":null}"; do
  proc="${probe%%:*}"; body="${probe#*:}"
  got="$(bobc "$proc" "$body")"
  [ "$got" = "403" ] \
    && pass "Bob cannot $proc on a bug he cannot read" \
    || fail "Bob can $proc on a restricted bug" "http=$got (200 here is the read/write split regressing)"
done

# Dependency edges are disclosures. deps.add checked the edited bug at the
# bug level but the DEPENDENCY at the product level only, so an edge could be
# pointed at a restricted bug the caller cannot open -- confirming it exists
# and naming it in the tree. deps.tree/graph filtered on product visibility
# alone for the same reason.
[ "$(bobc deps.add "{\"bugId\":\"$BUG\",\"dependsOnId\":\"$SECRET_BUG\"}")" = "403" ] \
  && pass "Bob cannot point a dependency at a bug he cannot read" \
  || fail "deps.add accepts an edge to a restricted bug" \
          "http=$(bobc deps.add "{\"bugId\":\"$BUG\",\"dependsOnId\":\"$SECRET_BUG\"}")"

# Control: Alice CAN create an edge to that same restricted bug -- from a
# DIFFERENT source bug, so it does not collide with the probe above. An
# earlier version reused the same pair, and under mutation Bob's edge landed
# first, making the control fail with "already exists" and report a cascade
# rather than an independent result.
# access and not about the edge being rejected for some unrelated reason.
[ "$(code deps.add "{\"bugId\":\"$PRIV_BUG\",\"dependsOnId\":\"$SECRET_BUG\"}")" = "200" ] \
  && pass "control: Alice can create the same edge" \
  || fail "control failed: even Alice cannot create the edge, so the refusal proves nothing"

# With that edge in place, the restricted bug must not surface as a node in
# Bob's graph.
bob deps.graph | grep -q "$SECRET_BUG" \
  && fail "the restricted bug appears as a node in Bob's dependency graph" \
  || pass "the restricted bug is absent from Bob's dependency graph"

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

# Alice's own run of that query sees the restricted bug; Bob's must not.
call search.query "{\"where\":$SS_QUERY,\"limit\":200}" | grep -q "$SECRET_BUG" \
  && pass "control: the shared query does match the restricted bug for its owner" \
  || fail "control failed: the query does not match the restricted bug even for Alice"

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
VB="$(call bugs.create "{\"productId\":\"$VP\",\"componentId\":\"$VC\",\"versionId\":\"$VV\",\"summary\":\"Vote me $STAMP\",\"description\":\"d\"}" | jget 'json.id')"

# Voting is OFF by default (all three columns default to 0), which is the
# whole reason 0 is the default rather than something permissive.
[ "$(code votes.cast "{\"bugId\":\"$VB\",\"count\":1}")" = "409" ] \
  && pass "voting is refused while the product has no vote budget" \
  || fail "votes.cast succeeded on a product with voting disabled"

# Asserted, not discarded: an update that silently failed here would make
# every voting check below fail for a reason that has nothing to do with votes.
UPD="$(code products.update "{\"id\":\"$VP\",\"changes\":{\"votesPerUser\":5,\"maxVotesPerBug\":3,\"votesToConfirm\":2}}")"
[ "$UPD" = "200" ] && pass "the product vote limits are editable" || fail "products.update rejected the vote limits" "http=$UPD"

[ "$(code votes.cast "{\"bugId\":\"$VB\",\"count\":4}")" = "400" ] \
  && pass "a vote above maxVotesPerBug is refused" \
  || fail "maxVotesPerBug is not enforced"

VR="$(call votes.cast "{\"bugId\":\"$VB\",\"count\":2}")"
[ "$(echo "$VR" | jget 'json.voteCount')" = "2" ] \
  && pass "voteCount is the SUM of vote quantities, not a row count" \
  || fail "voteCount wrong after casting 2 votes" "$(echo "$VR" | head -c 120)"

# Bugzilla's auto-confirm: votesToConfirm=2 and the bug was UNCONFIRMED.
[ "$(echo "$VR" | jget 'json.confirmed')" = "true" ] \
  && pass "reaching votesToConfirm confirms an UNCONFIRMED bug" \
  || fail "the bug was not auto-confirmed at the vote threshold"
[ "$(call bugs.get "{\"id\":\"$VB\"}" | jget 'json.bug.status')" = "CONFIRMED" ] \
  && pass "the auto-confirmed status is persisted" || fail "status did not persist as CONFIRMED"

echo "notifications and watching"
# The notifications table had three READ procedures and no writer, so the inbox
# was permanently empty and the nav's unread badge could never appear.
# Bob is CC'd on the bug, so a comment by Alice must reach him.
call cc.add "{\"bugId\":\"$BUG\",\"userId\":\"$BOB_ID\"}" >/dev/null
BOB_BEFORE="$(bob notifications.unreadCount | jget 'json.count')"
call comments.add "{\"bugId\":\"$BUG\",\"body\":\"ping $STAMP\"}" >/dev/null
BOB_AFTER="$(bob notifications.unreadCount | jget 'json.count')"
[ "$BOB_AFTER" -gt "$BOB_BEFORE" ] 2>/dev/null \
  && pass "a comment notifies the CC'd user ($BOB_BEFORE -> $BOB_AFTER)" \
  || fail "no notification reached the CC'd user" "before=$BOB_BEFORE after=$BOB_AFTER"

# The actor does not notify herself.
ALICE_BEFORE="$(call notifications.unreadCount | jget 'json.count')"
call comments.add "{\"bugId\":\"$BUG\",\"body\":\"self $STAMP\"}" >/dev/null
[ "$(call notifications.unreadCount | jget 'json.count')" = "$ALICE_BEFORE" ] \
  && pass "the actor is not notified about her own change" \
  || fail "the actor notified herself"

# A restricted bug must not notify someone who cannot read it -- the title
# carries the summary, so a notification is a disclosure.
SEC_BEFORE="$(bob notifications.unreadCount | jget 'json.count')"
call comments.add "{\"bugId\":\"$SECRET_BUG\",\"body\":\"secret note $STAMP\"}" >/dev/null
[ "$(bob notifications.unreadCount | jget 'json.count')" = "$SEC_BEFORE" ] \
  && pass "no notification about a bug the recipient cannot read" \
  || fail "a restricted bug's summary leaked through a notification"

# The unread count is cached in KV and written by notifications.markRead, so a
# user who has EVER marked something read has a warm cache and the
# authoritative database fallback never runs for them. A fanout that inserts a
# row without refreshing that cache leaves them looking at a stale number.
# This sequence is specifically markRead-then-notify, which is the only order
# that exposes it.
FIRST_NOTIF="$(bob notifications.list '{"limit":1}' | jget 'json.0.id')"
[ -n "$FIRST_NOTIF" ] && bob notifications.markRead "{\"id\":\"$FIRST_NOTIF\"}" >/dev/null
WARM="$(bob notifications.unreadCount | jget 'json.count')"
call comments.add "{\"bugId\":\"$BUG\",\"body\":\"cache probe $STAMP\"}" >/dev/null
AFTER_WARM="$(bob notifications.unreadCount | jget 'json.count')"
[ "$AFTER_WARM" -gt "$WARM" ] 2>/dev/null \
  && pass "the unread count is fresh after a notification even with a warm cache ($WARM -> $AFTER_WARM)" \
  || fail "stale unread count: the KV cache was not refreshed by the fanout" "warm=$WARM after=$AFTER_WARM"
echo "auth posture"
# Fail-closed: a write with no identity must be refused, not silently accepted.
[ "$(anon products.create '{"name":"nope","description":"nope"}')" = "401" ] \
  && pass "an unauthenticated write is refused with 401" || fail "an unauthenticated write was NOT refused"
[ "$(anon products.list '{}')" = "200" ] \
  && pass "an anonymous read is allowed" || fail "an anonymous read was refused"

echo
echo "passed $PASS, failed $FAIL"
[ "$FAIL" -eq 0 ] || exit 1
