#!/usr/bin/env bash
# ============================================================================
# user_erasure_reachability_gate.sh - prove that a human who asks to be erased
# can be, and that a human who changes their mind can say so.
#
# THREE MEASURED DEFECTS THIS EXISTS FOR, all of which printed clean:
#
#   1 THE UNDO WAS UNREACHABLE. `POST /me/delete/cancel` resolved its caller
#     through `sessions::validate`, and the request it reverses revokes every
#     predicate that function requires - the credential version, the
#     `deletion_requested_at` stamp, the `idp_sessions.revoked_at` flag. Three
#     independent refusals, plus `check_user_eligible` on re-login. The
#     thirty-day window was advertised in the confirmation email, whose link
#     pointed at `/me`, which the same request had just made unreachable.
#
#   2 THE ERASURE COULD NOT RUN. `cron::account_reaper` asked whether the user
#     had financial history by SELECTing `zeroship.organization_members`,
#     `zeroship.organization_accounts` and `zeroship.invoices` on the AUTH
#     service's connection. `zeroship_auth` holds no privilege on any of them,
#     so under the real role that is `42501` and the whole erasure fails. Every
#     test connected as the superuser, so nothing saw it.
#
#   3 THE FK LIST WAS WRONG IN A WAY SET NULL COULD NOT FIX. `ATTRIBUTION_FKS`
#     claimed to be exactly the users references that block a delete. Read out
#     of `pg_constraint` it named one of five, and three of the five were
#     `NOT NULL`, so the `SET NULL` it performed was not a spelling they
#     accepted.
#
# Every one of those is a question that can be ASKED - of a real database, or
# of the source - so this gate asks rather than trusting the prose.
#
# WHAT THIS GATE DOES NOT RULE ON. Whether the preflight's POLICY is right
# (that is `crates/zeroship-control/tests/erasure_preflight_test.rs`), whether
# the undo token's crypto is sound, or whether the reaper's audit row is
# readable by anyone. It rules on reachability and privilege only.
#
# Usage:
#   tests/user_erasure_reachability_gate.sh
#   tests/user_erasure_reachability_gate.sh --dsn <url>   # an EMPTY database
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# shellcheck source=lib/gate_arms.sh
. "$ROOT/tests/lib/gate_arms.sh"

DSN=""
while [ "$#" -gt 0 ]; do
  case "$1" in
    --dsn) DSN="${2:-}"; shift 2 ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
done

gate_arms_init user_erasure_reachability

OWN_CONTAINER=""
WORK_DIR="$(mktemp -d -t zeroship-user-erasure.XXXXXX)"
cleanup() {
  if [ -n "$OWN_CONTAINER" ]; then
    docker rm -f "$OWN_CONTAINER" >/dev/null 2>&1 || true
  fi
  [ -n "$WORK_DIR" ] && [ -d "$WORK_DIR" ] && rm -rf -- "$WORK_DIR"
}
trap cleanup EXIT HUP INT TERM

PASS=0
FAIL=0
pass() { PASS=$((PASS + 1)); echo "  ok   $1"; }
fail() { FAIL=$((FAIL + 1)); echo "  FAIL $1"; }

echo "== user erasure reachability gate =="

# A port range of this gate's own. The corpus gate takes 5560-5599 and the
# organization-authority gate 5600-5639; two gates racing for one port is a
# failure that looks like a database fault.
if [ -z "$DSN" ]; then
  if ! command -v docker >/dev/null 2>&1; then
    echo "FAIL: no --dsn given and docker is not on PATH" >&2
    exit 2
  fi
  port=""
  for p in $(seq 5640 5679); do
    if ! ss -ltn 2>/dev/null | grep -q ":$p "; then
      port="$p"
      break
    fi
  done
  if [ -z "$port" ]; then
    echo "FAIL: no free TCP port in 5640-5679 for PostgreSQL" >&2
    exit 2
  fi
  OWN_CONTAINER="zs-erasure-gate-$port"
  docker rm -f "$OWN_CONTAINER" >/dev/null 2>&1 || true
  echo "starting fresh PostgreSQL on 127.0.0.1:$port ($OWN_CONTAINER)"
  if ! docker run -d --name "$OWN_CONTAINER" \
      -p "127.0.0.1:$port:5432" \
      -e POSTGRES_PASSWORD=zeroship -e POSTGRES_USER=postgres -e POSTGRES_DB=zeroship \
      postgres:17 >/dev/null 2>&1; then
    echo "FAIL: could not start PostgreSQL container" >&2
    exit 2
  fi
  if ! docker exec "$OWN_CONTAINER" sh -c \
      'for i in $(seq 1 60); do pg_isready -U postgres -q && exit 0; sleep 1; done; exit 1'; then
    echo "FAIL: PostgreSQL did not become ready" >&2
    exit 2
  fi
  DSN="postgres://postgres:zeroship@localhost:$port/zeroship"

  echo "applying the committed corpus with deploy/ops/db-migrate.sh"
  if ! ZEROSHIP_MIGRATE_DSN="$DSN" bash "$ROOT/deploy/ops/db-migrate.sh" \
      >"$WORK_DIR/apply.log" 2>&1; then
    echo "FAIL: the corpus did not apply; this gate has nothing to rule on" >&2
    tail -30 "$WORK_DIR/apply.log" >&2
    exit 2
  fi
fi

# One psql invocation shape for the whole gate, with the same container
# fallback the organization-authority gate uses: a developer shell often has no
# libpq, and refusing there would make this gate unrunnable on the machine that
# most needs it. NO `2>/dev/null` anywhere: a query that failed and a query that
# found nothing print the same empty result, and every arm below turns its
# output into a count.
PSQL_IMAGE="postgres:17"
psql_q() {
  if command -v psql >/dev/null 2>&1; then
    psql "$DSN" -Atq -F'|' -c "$1"
  else
    docker run --rm --network host "$PSQL_IMAGE" psql "$DSN" -Atq -F'|' -c "$1"
  fi
}

if ! command -v psql >/dev/null 2>&1 && ! command -v docker >/dev/null 2>&1; then
  echo "FAIL: neither psql nor docker is on PATH; nothing can query the database" >&2
  exit 2
fi
if ! psql_q "SELECT 1 FROM information_schema.tables \
             WHERE table_schema='zeroship' AND table_name='users'" | grep -q 1; then
  echo "FAIL: the DSN does not name a database with the corpus applied" >&2
  exit 2
fi

# ---------------------------------------------------------------------------
# Arm 1 - every reference to zeroship.users clears itself on a DELETE.
#
# The claim is not "the list is right"; there is no list. It is that PostgreSQL
# removes or nulls EVERY dependent, so no hand-written pass is needed and no
# hard delete can be blocked. Read from pg_constraint, never from the migration
# source: the migration is what this arm exists to check.
#
# A SET NULL edge on a NOT NULL column is counted as blocking, because that is
# what it is - the declaration is accepted and the delete raises `23502` when it
# fires. That is defect 3's shape and a naive confdeltype scan misses it.
# ---------------------------------------------------------------------------
echo ""
echo "-- 1. no reference to zeroship.users blocks a delete --"
USERS_FKS_SQL="
SELECT c.conname,
       src.relname,
       c.confdeltype,
       (SELECT bool_and(a.attnotnull)
          FROM unnest(c.conkey) AS k(attnum)
          JOIN pg_attribute a ON a.attrelid = c.conrelid AND a.attnum = k.attnum)
FROM pg_constraint c
JOIN pg_class src ON src.oid = c.conrelid
JOIN pg_class tgt ON tgt.oid = c.confrelid
JOIN pg_namespace n ON n.oid = tgt.relnamespace
WHERE c.contype = 'f' AND tgt.relname = 'users' AND n.nspname = 'zeroship'
ORDER BY src.relname, c.conname"
psql_q "$USERS_FKS_SQL" >"$WORK_DIR/users_fks.txt"
N_USERS_FKS=$(grep -c . "$WORK_DIR/users_fks.txt")
BLOCKING=""
while IFS='|' read -r conname relname deltype notnull; do
  [ -z "$conname" ] && continue
  case "$deltype" in
    c) ;;                                   # CASCADE - the dependent goes
    n) [ "$notnull" = "t" ] && BLOCKING="$BLOCKING $conname(set-null-on-not-null)" ;;
    *) BLOCKING="$BLOCKING $conname(confdeltype=$deltype)" ;;
  esac
done <"$WORK_DIR/users_fks.txt"
if [ -z "$BLOCKING" ]; then
  pass "all $N_USERS_FKS references to zeroship.users are CASCADE or a nullable SET NULL"
else
  fail "these references block a hard delete:$BLOCKING"
fi
gate_arm users_fks "$N_USERS_FKS" 20

# ---------------------------------------------------------------------------
# Arm 2 - the auth service names no table it cannot read.
#
# Defect 2 in general form. Every `zeroship.<table>` the auth sources mention is
# joined against the grants `zeroship_auth` actually holds, so a cross-domain
# read is a gate failure rather than a `42501` a cron swallows.
#
# The table list comes from the SOURCE and the privilege list from the
# DATABASE. Neither is written here, which is the point: a gate carrying its own
# roster of control-owned tables is satisfiable by editing the roster.
# ---------------------------------------------------------------------------
echo ""
echo "-- 2. the auth service reads only tables zeroship_auth is granted --"
# COMMENT LINES ARE EXCLUDED, and that exclusion is load-bearing rather than
# tidying: the modules that used to reach across the boundary now EXPLAIN the
# boundary, naming the very tables they must not read. A spelling-only scan
# reports those sentences as the defect they describe, which is how a gate ends
# up demanding that its own rationale be deleted.
grep -rh --include='*.rs' -vE '^[[:space:]]*(//|\*)' "$ROOT/crates/zeroship-auth/src" \
  | grep -oE 'zeroship\.[a-z_][a-z0-9_]*' \
  | sed 's/^zeroship\.//' | sort -u >"$WORK_DIR/named.txt"
psql_q "SELECT DISTINCT table_name FROM information_schema.role_table_grants
        WHERE table_schema='zeroship' AND grantee='zeroship_auth'" \
  | sort -u >"$WORK_DIR/granted.txt"
psql_q "SELECT table_name FROM information_schema.tables WHERE table_schema='zeroship'" \
  | sort -u >"$WORK_DIR/real_tables.txt"
# Only real tables can be ruled on; the grep also catches schema-qualified
# function and type names, which hold no grant and are not reads.
comm -12 "$WORK_DIR/named.txt" "$WORK_DIR/real_tables.txt" >"$WORK_DIR/named_tables.txt"
N_NAMED=$(grep -c . "$WORK_DIR/named_tables.txt")
comm -23 "$WORK_DIR/named_tables.txt" "$WORK_DIR/granted.txt" >"$WORK_DIR/ungranted.txt"
N_UNGRANTED=$(grep -c . "$WORK_DIR/ungranted.txt")
if [ "$N_UNGRANTED" -eq 0 ]; then
  pass "all $N_NAMED tables named by crates/zeroship-auth/src are granted to zeroship_auth"
else
  fail "crates/zeroship-auth/src names $N_UNGRANTED table(s) zeroship_auth cannot touch:"
  sed 's/^/       /' "$WORK_DIR/ungranted.txt"
fi
gate_arm auth_named_tables "$N_NAMED" 15

# ---------------------------------------------------------------------------
# Arm 3 - the undo is reachable, and the email points at it.
#
# Defect 1. Four claims, each a separate way the window can go dark again:
# the GET that renders the form, the POST that spends the token, the link the
# confirmation email carries, and the absence of a by-id cancel that would let
# the schedule be cleared without presenting the credential.
# ---------------------------------------------------------------------------
echo ""
echo "-- 3. the thirty-day undo has a door --"
SERVER="$ROOT/crates/zeroship-auth/src/server.rs"
DELETION="$ROOT/crates/zeroship-auth/src/ui/account_deletion.rs"
USERS_RS="$ROOT/crates/zeroship-auth/src/store/users.rs"
N_CLAIMS=0
claim() {
  N_CLAIMS=$((N_CLAIMS + 1))
  if eval "$2"; then pass "$1"; else fail "$1"; fi
}
claim "GET /me/delete/cancel is registered" \
  "grep -A3 '\"/me/delete/cancel\"' '$SERVER' | grep -q 'web::get()'"
claim "POST /me/delete/cancel is registered" \
  "grep -A3 '\"/me/delete/cancel\"' '$SERVER' | grep -q 'web::post()'"
claim "the confirmation email links to the cancel route with a token" \
  "grep -q '/me/delete/cancel?token=' '$DELETION'"
claim "no by-id cancel exists beside the token redemption" \
  "! grep -q 'fn cancel_deletion' '$USERS_RS'"
gate_arm undo_reachability "$N_CLAIMS" 4

# ---------------------------------------------------------------------------
# Arm 4 - the sole-owner precondition is checked before anything is written,
# and an unanswerable check is a refusal.
#
# The ordering is the whole guarantee: a preflight consulted after
# `request_deletion` has committed would be advice, not a precondition. Read by
# line offset within one function rather than by presence, because both symbols
# being in the file says nothing about which runs first.
# ---------------------------------------------------------------------------
echo ""
echo "-- 4. the erasure precondition runs before the window opens --"
N_ORDER=0
order_claim() {
  N_ORDER=$((N_ORDER + 1))
  if eval "$2"; then pass "$1"; else fail "$1"; fi
}
PREFLIGHT_LINE=$(grep -n 'erasure_preflight(' "$DELETION" | head -1 | cut -d: -f1)
REQUEST_LINE=$(grep -n 'users::request_deletion(' "$DELETION" | head -1 | cut -d: -f1)
order_claim "the preflight is called at all" "[ -n '$PREFLIGHT_LINE' ]"
order_claim "request_deletion is called at all" "[ -n '$REQUEST_LINE' ]"
order_claim "the preflight runs BEFORE request_deletion" \
  "[ -n '$PREFLIGHT_LINE' ] && [ -n '$REQUEST_LINE' ] && [ '$PREFLIGHT_LINE' -lt '$REQUEST_LINE' ]"
order_claim "the reaper re-checks it before the delete" \
  "grep -q 'still_erasable' '$ROOT/crates/zeroship-auth/src/cron/account_reaper.rs'"
order_claim "a missing control key refuses rather than proceeding" \
  "grep -q 'PreflightError::NoCredential' '$ROOT/crates/zeroship-auth/src/control_client.rs'"
gate_arm precondition_ordering "$N_ORDER" 5

echo ""
echo "============================================"
echo "  pass $PASS   fail $FAIL"
echo "============================================"
gate_arms_finish || exit 1
[ "$FAIL" -eq 0 ]
