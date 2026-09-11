#!/usr/bin/env bash
# Keep the worker's normal dependency closure, platform role attributes and
# startup refusal in agreement. Logical decoding belongs to the relay service.
# The source scan excludes standalone drivers and trailing test modules; it is
# a structural guard, complemented by real relay and worker database tests.
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT" || exit 1

# shellcheck source=tests/lib/gate_arms.sh
. "$ROOT/tests/lib/gate_arms.sh"
gate_arms_init worker_replication_privilege

FAIL=0
bad() { printf '  FAIL %s\n' "$1"; FAIL=$((FAIL + 1)); }
good() { printf '  ok   %s\n' "$1"; }

MIGRATION="db/migrations-ts/20260910000100_cdc_relay_authority.ts"
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
    if ! printf '%s\n' "$validate_body" | grep -q 'if posture\.replication || posture\.bypass_rls'; then
      bad "worker boot must explicitly refuse both replication and RLS bypass"
    fi
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
