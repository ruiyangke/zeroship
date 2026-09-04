#!/usr/bin/env bash
# ============================================================================
# The worker's REPLICATION attribute is ONE decision written in three places.
#
# `db/migrations-ts/20260818000200_worker_database_authority.ts` GRANTS
# `zeroship_worker` the PostgreSQL `REPLICATION` attribute.
# `crates/zeroship-worker/src/db_posture.rs` REFUSES TO BOOT without it.
# `crates/zeroship-plugin-db/src/{replication,wal_consumer,slot_reaper}.rs` USE
# it. AGENTS.md's "Privilege follows the PROCESS, not the function" says the
# process that executes creator code must not hold a privilege it does not
# need, so those three must agree in both directions - and nothing checked that
# they did.
#
# ## The mistake this exists to catch, stated as the sequence that produces it
#
# `docs/proposals/2026-08-28-cdc-service.md` ("The reaper is a privilege
# change") lists four edits that "land in one commit or not at all": drop the
# role attributes, invert the boot check, delete the worker's reaper, delete
# plugin-db's reaper. Read quickly, edits 3 and 4 look self-contained - delete
# two files, the worker stops linking the relay's code, done. They are not.
#
#   * Deleting the reaper drops NO privilege. MEASURED 2026-09-04 against the
#     built `target/debug/zeroship-worker` (default features, `--bin`), the
#     shipped binary still contains `pg_create_logical_replication_slot($1,
#     'pgoutput', false, false)`, `SELECT pg_drop_replication_slot($1)` and
#     `START_REPLICATION SLOT ... LOGICAL ...` after the reaper's own
#     occurrence is discounted. All three are refused to a NOREPLICATION role.
#   * Deleting the reaper REMOVES A SAFETY VALVE. The worker still mints one
#     replication slot per (app, worker) from creator JS - `Subscription.ready()`
#     -> `cdc_lifecycle::ensure_ready` -> `PgChangeStream::spawn_consumer` ->
#     `replication::ensure_worker_slot`. A worker that crashes leaves that slot
#     behind, and the reaper is the only thing in the tree that removes it.
#
# So the gate asserts the joint invariant rather than any one file:
#
#   1. the shipped worker executes a REPLICATION-gated statement
#          IF AND ONLY IF the migration grants the attribute;
#   2. the shipped worker executes a REPLICATION-gated statement
#          IF AND ONLY IF the boot check demands the attribute;
#   3. IF the shipped worker CREATES replication slots, a reaper is supervised.
#
# It is self-inverting on purpose. The day the CDC relay takes the streaming
# path, rules 1 and 2 stop demanding the grant and start FORBIDDING it, so the
# same gate that blocks a premature deletion also blocks a stale grant.
#
# ## What the three statements are, and why those three
#
# PostgreSQL gates them on `pg_roles.rolreplication`. MEASURED 2026-09-04 with a
# `NOSUPERUSER NOREPLICATION` login on PostgreSQL 18.6 (pgvector/pgvector:pg18)
# and 16.14 (pgvector/pgvector:pg16, the version `deploy/compose/docker-compose.yml`
# pins):
#
#   pg_create_logical_replication_slot -> ERROR: permission denied to use
#       replication slots / Only roles with the REPLICATION attribute may use
#       replication slots.                                    (18.6 and 16.14)
#   pg_drop_replication_slot           -> the same error.     (18.6 and 16.14)
#   a `replication=database` connection -> FATAL: permission denied to start
#       WAL sender / Only roles with the REPLICATION attribute may start a WAL
#       sender process.                                              (18.6)
#
# The CONTROLS matter as much: the same NOREPLICATION login SELECTs
# `pg_replication_slots` and takes `pg_try_advisory_lock` without error on both
# servers. Four of the reaper's five statements need no privilege at all; only
# its drop does. That is why "the reaper holds REPLICATION" is a claim about one
# line, not about a file.
#
# ## What this gate does NOT do, stated so nobody reads it as complete
#
#   * It reads SOURCE, not the linked binary. The scan truncates each file at
#     its first `#[cfg(test)]` and drops `//` comment lines; it does not model
#     item-level `#[cfg(any(test, feature = "test-helpers"))]`. That is exact
#     for the files it finds today (each has exactly one `#[cfg(test)]`, the
#     trailing test module, and no replication statement inside a
#     test-helpers-gated item) and is stated rather than assumed.
#   * It scans `crates/` only, never `libs/`. `libs/compio-postgres` is a
#     standalone PostgreSQL driver; that it can SPEAK the streaming protocol is
#     its job. The privilege question is who CALLS it.
#   * It rules on spelling, not on whether the statement is reachable. Arm 4's
#     reaper check is likewise textual - `crates/zeroship-worker/src/main.rs`
#     carries its own in-file test for the supervision shape.
# ============================================================================
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT" || exit 1

# shellcheck source=tests/lib/gate_arms.sh
. "$ROOT/tests/lib/gate_arms.sh"
gate_arms_init worker_replication_privilege

FAIL=0
bad() { printf '  FAIL %s\n' "$1"; FAIL=$((FAIL + 1)); }
good() { printf '  ok   %s\n' "$1"; }

MIGRATION="db/migrations-ts/20260818000200_worker_database_authority.ts"
POSTURE="crates/zeroship-worker/src/db_posture.rs"
WORKER_MAIN="crates/zeroship-worker/src/main.rs"

# ---------------------------------------------------------------------------
# Arm 1: what the shipped worker's own crates ask PostgreSQL to do.
#
# The crate list is the worker's NORMAL dependency closure, so a statement that
# moves from plugin-db to another crate the worker links is still counted, and
# one that moves OUT of the closure correctly stops being counted.
# ---------------------------------------------------------------------------
echo "== REPLICATION-gated statements in the shipped worker's closure =="

REPLICATION_OPS='pg_create_logical_replication_slot(
pg_drop_replication_slot(
replication=database'

if ! CLOSURE=$(cargo tree -p zeroship-worker -e normal --prefix none 2>/dev/null); then
  bad "cargo tree failed for zeroship-worker - the gate cannot rule, which is a refusal"
  CLOSURE=""
fi
if [ -z "$CLOSURE" ]; then
  bad "cargo tree printed nothing for zeroship-worker - refusing to read that as clean"
fi

SCAN_DIRS=""
for crate in $(printf '%s\n' "$CLOSURE" | awk '{print $1}' | sort -u); do
  [ -d "crates/$crate/src" ] && SCAN_DIRS="$SCAN_DIRS crates/$crate/src"
done

files_scanned=0
uses_total=0
mints_total=0
if [ -n "$SCAN_DIRS" ]; then
  while IFS= read -r rs; do
    files_scanned=$((files_scanned + 1))
    cut_line="$(grep -n '^[[:space:]]*#\[cfg(test)\]' "$rs" | head -1 | cut -d: -f1)"
    if [ -n "$cut_line" ]; then
      body="$(head -n "$((cut_line - 1))" "$rs" | grep -v '^[[:space:]]*//')"
    else
      body="$(grep -v '^[[:space:]]*//' "$rs")"
    fi
    while IFS= read -r op; do
      [ -n "$op" ] || continue
      hits="$(printf '%s\n' "$body" | grep -cF "$op")"
      [ "$hits" -gt 0 ] || continue
      uses_total=$((uses_total + hits))
      if [ "$op" = "pg_create_logical_replication_slot(" ]; then
        mints_total=$((mints_total + hits))
      fi
      printf '       %s: %s x%s\n' "$rs" "$op" "$hits"
    done <<EOF
$REPLICATION_OPS
EOF
  done < <(find $SCAN_DIRS -name '*.rs' -type f | LC_ALL=C sort)
fi

echo "       ${uses_total} REPLICATION-gated statement(s), ${mints_total} of them slot creations"

# The floor counts FILES SCANNED, never statements found. The statement count is
# this gate's FINDING and is allowed to reach zero - that is the end state the
# CDC relay exists to produce. What must never reach zero is the enumeration: a
# renamed source directory or a silently empty closure would otherwise report
# "no replication statements" and let arm 5 demand the grant be dropped.
if ! gate_arm closure_scan "$files_scanned" 200; then
  FAIL=$((FAIL + 1))
fi

# ---------------------------------------------------------------------------
# Arm 2: the attribute the platform migrations actually put on the role.
#
# Ruled on per ALTER ROLE statement, in filename order, so a later migration
# that re-states the attributes is the one that counts. `role().create()` in
# db/migrations-ts/20260702000100_schema_roles_extensions.ts cannot set
# REPLICATION - the DSL has no such option, which is why the migration reaches
# for `raw()` and says so in its `reason`.
# ---------------------------------------------------------------------------
echo
echo "== the REPLICATION attribute the migrations grant zeroship_worker =="

alter_statements=0
grant_replication=0
while IFS= read -r line; do
  [ -n "$line" ] || continue
  alter_statements=$((alter_statements + 1))
  sql="${line#*:}"
  if printf '%s' "$sql" | grep -qE '[[:space:]]REPLICATION([[:space:]]|"|,|$)'; then
    grant_replication=1
    good "grants REPLICATION: $line"
  else
    grant_replication=0
    good "does not grant REPLICATION: $line"
  fi
done < <(grep -rn 'ALTER ROLE zeroship_worker' db/migrations-ts/ 2>/dev/null | LC_ALL=C sort)

if [ "$alter_statements" -eq 0 ]; then
  bad "no ALTER ROLE zeroship_worker statement in db/migrations-ts/ - the role's attributes are set somewhere this gate cannot see"
fi
if [ ! -f "$MIGRATION" ]; then
  bad "$MIGRATION is gone; the gate's named subject moved without this file changing"
fi

if ! gate_arm role_attributes "$alter_statements" 1; then
  FAIL=$((FAIL + 1))
fi

# ---------------------------------------------------------------------------
# Arm 3: what the worker demands of its own role before it boots.
#
# Ruled on per refusal arm of `db_posture::validate`, because the question is
# whether the replication predicate is STILL one of them - a check deleted
# outright and a check inverted must not look the same to this gate.
# ---------------------------------------------------------------------------
echo
echo "== the worker's boot-time demand for REPLICATION =="

posture_arms=0
boot_requires=0
if [ -f "$POSTURE" ]; then
  validate_body="$(awk '/^fn validate\(/{inside=1} inside{print} inside && /^}/{exit}' "$POSTURE")"
  posture_arms="$(printf '%s\n' "$validate_body" | grep -c 'return Err(')"
  if printf '%s\n' "$validate_body" | grep -q '!posture\.replication'; then
    boot_requires=1
    good "db_posture::validate refuses a worker role WITHOUT REPLICATION"
  else
    good "db_posture::validate does not demand REPLICATION"
  fi
else
  bad "$POSTURE is gone; the worker's boot posture check cannot be read"
fi

if ! gate_arm boot_predicates "$posture_arms" 6; then
  FAIL=$((FAIL + 1))
fi

# ---------------------------------------------------------------------------
# Arm 4: whether the abandoned-slot reaper is still started and supervised.
# ---------------------------------------------------------------------------
echo
echo "== the abandoned-slot reaper in the worker's production main =="

reaper_facts=0
reaper_start=0
reaper_supervised=0
if [ -f "$WORKER_MAIN" ]; then
  reaper_facts=$((reaper_facts + 1))
  if grep -q 'slot_reaper::start(' "$WORKER_MAIN"; then
    reaper_start=1
    good "production main starts the reaper"
  else
    good "production main does not start a reaper"
  fi
  reaper_facts=$((reaper_facts + 1))
  if grep -q 'run_server_with_slot_reaper(server.run(), slot_reaper_task)' "$WORKER_MAIN"; then
    reaper_supervised=1
    good "production main supervises the reaper alongside the HTTP server"
  else
    good "production main supervises no reaper"
  fi
else
  bad "$WORKER_MAIN is gone; the gate cannot read the worker's production entry point"
fi
reaper_live=0
[ "$reaper_start" -eq 1 ] && [ "$reaper_supervised" -eq 1 ] && reaper_live=1

if ! gate_arm reaper_supervision "$reaper_facts" 2; then
  FAIL=$((FAIL + 1))
fi

# ---------------------------------------------------------------------------
# Arm 5: THE COUPLING. Nothing above fails on its own reading.
# ---------------------------------------------------------------------------
echo
echo "== use, grant, boot check and reaper are one decision =="

joined=0

joined=$((joined + 1))
if [ "$uses_total" -gt 0 ] && [ "$grant_replication" -eq 1 ]; then
  good "the worker executes REPLICATION-gated statements and the role is granted REPLICATION"
elif [ "$uses_total" -eq 0 ] && [ "$grant_replication" -eq 0 ]; then
  good "the worker executes no REPLICATION-gated statement and the role is not granted REPLICATION"
elif [ "$uses_total" -gt 0 ]; then
  bad "the worker executes $uses_total REPLICATION-gated statement(s) but no migration grants zeroship_worker REPLICATION - the worker cannot serve them"
else
  bad "zeroship_worker is granted REPLICATION and the worker executes no REPLICATION-gated statement - drop the attribute in $MIGRATION (AGENTS.md: privilege follows the process)"
fi

joined=$((joined + 1))
if [ "$uses_total" -gt 0 ] && [ "$boot_requires" -eq 1 ]; then
  good "the boot check demands the attribute the worker uses"
elif [ "$uses_total" -eq 0 ] && [ "$boot_requires" -eq 0 ]; then
  good "the boot check demands no attribute the worker no longer uses"
elif [ "$uses_total" -gt 0 ]; then
  bad "the worker executes $uses_total REPLICATION-gated statement(s) but $POSTURE no longer refuses a role without REPLICATION - the boot check stopped protecting the path"
else
  bad "$POSTURE still demands REPLICATION and the worker executes no REPLICATION-gated statement - invert the predicate so a worker that still holds the attribute refuses to boot"
fi

joined=$((joined + 1))
if [ "$mints_total" -gt 0 ] && [ "$reaper_live" -eq 1 ]; then
  good "the worker creates replication slots and supervises a reaper for the abandoned ones"
elif [ "$mints_total" -eq 0 ]; then
  good "the worker creates no replication slot, so no reaper is required of it"
else
  bad "the worker creates replication slots ($mints_total site(s)) but supervises no abandoned-slot reaper - a crashed worker's slot would retain WAL with nothing to remove it"
fi

if ! gate_arm coupling "$joined" 3; then
  FAIL=$((FAIL + 1))
fi

echo
if ! gate_arms_finish; then
  FAIL=$((FAIL + 1))
fi

if [ "$FAIL" -ne 0 ]; then
  echo "worker replication privilege gate: FAILED ($FAIL)" >&2
  exit 1
fi
echo "worker replication privilege gate: OK"
