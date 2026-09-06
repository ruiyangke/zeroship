#!/usr/bin/env bash
# ---------------------------------------------------------------------------
# Every mutating organization route is authorized, and every membership
# mutation serializes on the organization row before it writes.
#
# WHY THIS IS COUNTED BY ROUTE AND NOT BY CALL SITE. Counting `authz.require(`
# calls answers "are there guards?", which is the wrong question: a file with
# fourteen guards and one unguarded route reads as heavily guarded. THE HOLE IS
# A ROUTE WITH NO GUARD AT ALL, and it is only visible if the enumeration starts
# from the routing table and asks each entry to account for itself. So this
# reads `configure()` for its mutating routes, follows each to its handler, and
# rules on the HANDLER - one verdict per route, whether or not anything in it
# mentions authorization.
#
# WHY THE LOCK IS THE SECOND HALF. "An organization keeps at least one owner" is
# a claim about a SET, so no CHECK constraint can carry it: two concurrent
# removals each read "another owner remains" in their own snapshot and both
# commit. `SELECT ... FOR UPDATE` on the parent organization row is what makes
# the count in the DELETE's predicate true AT COMMIT rather than at read time.
# The same serialization is what lets a rank comparison inside an effect
# statement be trusted: without it, a demotion committing between the actor's
# seat being read and the effect landing is invisible.
#
# WHAT THIS GATE DOES NOT DO. It rules on the SHAPE of the code, not on the
# decision. A handler that calls `authz.require` with the wrong action or the
# wrong resource type is green here and denies (or permits) wrongly; that is
# `crates/zeroship-authz/tests/platform_policies_test.rs` and
# `crates/zeroship-control/tests/organizations_test.rs`, which drive real
# policies against a real database. This gate exists for the case those cannot
# see: a route nobody wrote a test for.
#
# Run the extractors' positive/control pair: this script --self-test. It also
# runs on every ordinary invocation.
# ---------------------------------------------------------------------------
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"
# shellcheck source=tests/lib/gate_arms.sh
. "$ROOT/tests/lib/gate_arms.sh"
gate_arms_init organization_route_authorization

MODULE="crates/zeroship-control/src/organizations.rs"

PASS=0
FAIL=0
pass() { PASS=$((PASS + 1)); echo "  ok   $1"; }
fail() { FAIL=$((FAIL + 1)); echo "  FAIL $1"; }

[ -f "$MODULE" ] || { echo "gate cannot run: $MODULE is missing" >&2; exit 1; }

# ---------------------------------------------------------------------------
# Extractors
# ---------------------------------------------------------------------------

# `<METHOD>\t<path>\t<handler>` for every route registered with a mutating verb.
# GET is deliberately absent: a read is gated too, but an ungated read is a
# different (and lesser) defect than an ungated write, and mixing them would
# hide the write count inside a bigger number.
mutating_routes() {
  awk '
    /web::resource\("/ { match($0, /"[^"]+"/); path = substr($0, RSTART + 1, RLENGTH - 2) }
    /\.route\(web::(post|patch|put|delete)\(\)\.to\(/ {
      match($0, /web::[a-z]+\(\)/); verb = substr($0, RSTART + 5, RLENGTH - 7)
      match($0, /\.to\([a-z_]+\)/);  handler = substr($0, RSTART + 4, RLENGTH - 5)
      print toupper(verb) "\t" path "\t" handler
    }
  ' "$1"
}

# `<name>\t<body>` for every `async fn` in the file, comments stripped, with the
# body flattened onto one line so a verdict can use `index()` for ORDER.
functions() {
  sed 's|//.*||' "$1" | awk '
    /^(pub )?async fn [a-z_]+/ {
      if (name != "") print name "\t" body
      match($0, /async fn [a-z_]+/); name = substr($0, RSTART + 9, RLENGTH - 9)
      body = ""
    }
    { gsub(/[ \t]+$/, ""); body = body " " $0 }
    END { if (name != "") print name "\t" body }
  '
}

body_of() {
  functions "$1" | awk -F'\t' -v want="$2" '$1 == want { print $2; found = 1 } END { exit !found }'
}

# Does this handler authorize before it acts? `require_seat_authority` is the
# named wrapper that issues one or two `authz.require` calls depending on how
# privileged the requested role is; a handler reaching it is gated.
is_authorized() {
  case "$1" in
    *"authz"*".require("*|*"require_seat_authority("*) return 0 ;;
    *) return 1 ;;
  esac
}

# The statements that CHANGE who holds what. A store function containing one of
# these is a membership mutation and owes the lock.
writes_membership() {
  case "$1" in
    *"INSERT INTO zeroship.organization_members"*|*"UPDATE zeroship.organization_members"*|\
    *"DELETE FROM zeroship.organization_members"*|*"INSERT INTO zeroship.project_members"*|\
    *"DELETE FROM zeroship.project_members"*|*"INSERT INTO zeroship.organization_invites"*|\
    *"UPDATE zeroship.organization_invites"*|*"DELETE FROM zeroship.organization_invites"*)
      return 0 ;;
    *) return 1 ;;
  esac
}

# Character offset of the first membership write in a flattened body, or 0.
# Offsets, not line numbers: the body is one line by the time it gets here, and
# the question is ORDER, which is what the offsets answer.
#
# THE BODY TRAVELS THROUGH THE ENVIRONMENT, NOT THROUGH `-v`. awk expands escape
# sequences in a `-v` assignment, and these bodies are Rust source full of `\`
# line continuations and `\"` quotes: `-v` turned every one into a warning and
# silently rewrote the text whose OFFSETS this function is measuring. ENVIRON
# hands the bytes over unchanged.
first_write_offset() {
  zs_body="$1" awk '
    BEGIN {
      body = ENVIRON["zs_body"]
      n = split("INSERT INTO zeroship.organization_members|UPDATE zeroship.organization_members|DELETE FROM zeroship.organization_members|INSERT INTO zeroship.project_members|DELETE FROM zeroship.project_members|INSERT INTO zeroship.organization_invites|UPDATE zeroship.organization_invites|DELETE FROM zeroship.organization_invites", marker, "|")
      best = 0
      for (i = 1; i <= n; i++) {
        at = index(body, marker[i])
        if (at > 0 && (best == 0 || at < best)) best = at
      }
      print best
    }'
}

first_lock_offset() {
  zs_body="$1" awk '
    BEGIN {
      body = ENVIRON["zs_body"]
      best = 0
      for (i = 1; i <= 2; i++) {
        marker = (i == 1) ? "lock_organization(" : "lock_project_organization("
        at = index(body, marker)
        if (at > 0 && (best == 0 || at < best)) best = at
      }
      print best
    }'
}

# ---------------------------------------------------------------------------
# The one excused function, with its reason and its liveness check
# ---------------------------------------------------------------------------
#
# `create_organization` seats its caller as owner without taking the lock, and
# that is correct rather than tolerated: the organization row is INSERTed in the
# same transaction, so no other session can see it, hold it, or write a second
# membership against it. There is nothing to serialize with.
#
# An excuse that stops matching is an exemption nobody removed, so the reverse
# direction is checked below: the named function must still exist and must still
# open a transaction.
LOCK_EXCUSES='create_organization'

self_test() {
  local tmp status=0 got
  tmp="$(mktemp -d)"
  trap 'rm -rf "$tmp"' RETURN

  cat > "$tmp/probe.rs" <<'RS'
pub async fn guarded_handler(authz: AuthzGuard) -> HttpResponse {
    if let Err(resp) = authz
        .require(AuthzAction::OrganizationWrite, Resource::Any, &state)
        .await
    { return resp; }
    HttpResponse::Ok().finish()
}

pub async fn unguarded_handler() -> HttpResponse {
    HttpResponse::Ok().finish()
}

pub async fn locks_first(registry: &Registry) -> Result<(), E> {
    let tx = conn.transaction().await?;
    lock_organization(&tx, organization_id).await?;
    tx.execute("INSERT INTO zeroship.organization_members (a) VALUES ($1)", &[]).await?;
    Ok(())
}

pub async fn writes_first(registry: &Registry) -> Result<(), E> {
    let tx = conn.transaction().await?;
    tx.execute("INSERT INTO zeroship.organization_members (a) VALUES ($1)", &[]).await?;
    lock_organization(&tx, organization_id).await?;
    Ok(())
}
RS

  is_authorized "$(body_of "$tmp/probe.rs" guarded_handler)" \
    && echo "  ok   a handler that calls authz.require is recognised" \
    || { echo "  FAIL the guard detector cannot see a plain authz.require"; status=1; }

  # One-variable control: the same shape with the guard removed.
  is_authorized "$(body_of "$tmp/probe.rs" unguarded_handler)" \
    && { echo "  FAIL an unguarded handler was reported as authorized"; status=1; } \
    || echo "  ok   a handler with no guard is refused"

  got=$(body_of "$tmp/probe.rs" locks_first)
  if [ "$(first_lock_offset "$got")" -gt 0 ] \
     && [ "$(first_lock_offset "$got")" -lt "$(first_write_offset "$got")" ]; then
    echo "  ok   a lock taken before the write is seen as ordered"
  else
    echo "  FAIL the ordering check cannot see a correctly ordered function"
    status=1
  fi

  # The control that makes the ordering check mean something: the SAME two
  # statements, swapped. A check that only asked "is a lock mentioned" passes
  # this one, which is the whole reason offsets are compared.
  got=$(body_of "$tmp/probe.rs" writes_first)
  if [ "$(first_lock_offset "$got")" -gt "$(first_write_offset "$got")" ]; then
    echo "  ok   a lock taken after the write is seen as out of order"
  else
    echo "  FAIL a write before its lock was accepted; the check is presence-only"
    status=1
  fi

  # The route extractor, on the shape `configure` actually uses.
  cat > "$tmp/routes.rs" <<'RS'
    cfg.service(
        web::resource("/api/things/{id}")
            .route(web::get().to(show))
            .route(web::patch().to(edit)),
    );
RS
  got=$(mutating_routes "$tmp/routes.rs")
  if [ "$got" = "$(printf 'PATCH\t/api/things/{id}\tedit')" ]; then
    echo "  ok   the route extractor reads the mutating verb and skips the read"
  else
    echo "  FAIL the route extractor produced: $got"
    status=1
  fi

  return "$status"
}

if [ "${1:-}" = "--self-test" ]; then
  echo "organization route authorization gate self-test"
  self_test
  exit $?
fi

echo "organization route authorization gate"
echo "  extractor self-check"
if ! self_test; then
  echo "GATE CANNOT RUN: its own extractors failed their controls." >&2
  exit 1
fi

# ---------------------------------------------------------------------------
# Arm 1: every mutating route reaches an authorization call
# ---------------------------------------------------------------------------
n_routes=0
ungated=""
missing_handler=""
while IFS=$'\t' read -r verb path handler; do
  [ -n "$handler" ] || continue
  n_routes=$((n_routes + 1))
  if ! body=$(body_of "$MODULE" "$handler"); then
    missing_handler="$missing_handler
       $verb $path -> $handler (no such async fn)"
    continue
  fi
  is_authorized "$body" || ungated="$ungated
       $verb $path -> $handler"
done < <(mutating_routes "$MODULE")

# MEASURED 2026-09-06: the module registers a dozen mutating routes. Floor 8
# leaves room for a route to be withdrawn without failing the gate, and is far
# above what a broken `web::resource` match would produce - which is zero.
if ! gate_arm mutating_routes "$n_routes" 8; then
  fail "the route table yielded $n_routes mutating route(s). The extractor stopped
       matching; whatever it says about guards is meaningless."
elif [ -n "$missing_handler" ]; then
  fail "these routes name a handler this file does not define:$missing_handler
       Either the handler moved, or the extractor is reading the wrong thing."
elif [ -z "$ungated" ]; then
  pass "all $n_routes mutating route(s) reach an authorization call"
else
  fail "these mutating routes reach no authz.require:$ungated
       A route with NO guard is the hole this arm exists for. Cedar runs FIRST,
       on the shared client and OUTSIDE the transaction, because enforce()
       writes the authz_decisions row and a refused mutation rolling back would
       erase the record of its own refusal."
fi

# ---------------------------------------------------------------------------
# Arm 2: every membership mutation takes the organization lock first
# ---------------------------------------------------------------------------
n_mutators=0
unlocked=""
out_of_order=""
while IFS=$'\t' read -r name body; do
  case "$body" in *".transaction()"*) ;; *) continue ;; esac
  writes_membership "$body" || continue
  case " $LOCK_EXCUSES " in *" $name "*) continue ;; esac
  n_mutators=$((n_mutators + 1))
  lock_at=$(first_lock_offset "$body")
  write_at=$(first_write_offset "$body")
  if [ "$lock_at" -eq 0 ]; then
    unlocked="$unlocked
       $name"
  elif [ "$lock_at" -gt "$write_at" ]; then
    out_of_order="$out_of_order
       $name (writes at $write_at, locks at $lock_at)"
  fi
done < <(functions "$MODULE")

# MEASURED 2026-09-06: eleven store functions write membership, invite or
# project-seat rows against an organization that already exists.
if ! gate_arm membership_locks "$n_mutators" 8; then
  fail "only $n_mutators membership-mutating function(s) were found. The
       function extractor or the write markers stopped matching."
elif [ -n "$unlocked" ]; then
  fail "these membership mutations never take the organization lock:$unlocked
       SELECT ... FOR UPDATE on the organizations row is what makes a count over
       its members true at COMMIT rather than at read time. Without it two
       concurrent owner removals each see another owner remaining and both
       commit, and the organization ends with none."
elif [ -n "$out_of_order" ]; then
  fail "these mutations write before they lock:$out_of_order
       Taking the lock afterwards serializes nothing that has already happened."
else
  pass "all $n_mutators membership mutation(s) lock the organization first"
fi

# ---------------------------------------------------------------------------
# Arm 3: the excuse list is still describing something real
# ---------------------------------------------------------------------------
n_excuses=0
stale=""
for name in $LOCK_EXCUSES; do
  n_excuses=$((n_excuses + 1))
  if ! body=$(body_of "$MODULE" "$name"); then
    stale="$stale
       $name (no such function)"
    continue
  fi
  case "$body" in
    *".transaction()"*) ;;
    *) stale="$stale
       $name (no longer opens a transaction)" ;;
  esac
done
# An emptied excuse list would silently return every excused function to arm 2,
# which is fine - but a list emptied by accident while arm 2 still consults it
# is not, and neither is an entry left behind after its function is deleted.
if ! gate_arm lock_excuses "$n_excuses" 1; then
  fail "the excuse list is empty while arm 2 still consults it."
elif [ -z "$stale" ]; then
  pass "all $n_excuses lock excuse(s) still name a live transactional function"
else
  fail "these lock excuses no longer describe anything:$stale
       Remove the entry, or find out what replaced the function it excused."
fi

gate_arms_finish || FAIL=$((FAIL + 1))
echo "  organization route authorization gate: $PASS passed, $FAIL failed"
[ "$FAIL" -eq 0 ] || exit 1
