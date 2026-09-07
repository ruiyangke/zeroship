#!/usr/bin/env bash
# ---------------------------------------------------------------------------
# The role ladder and the Cedar bands describe the SAME integers, and every
# policy file on disk is one the authorizer loads.
#
# WHY THE TWO HALVES BELONG IN ONE GATE. The ladder is data - rows in
# `zeroship.organization_roles`, seeded by a migration - and the bands are
# source, `permit ... when { context.effective_rank >= N }`. Nothing in either
# artifact references the other. A migration that moves `admin` from 30 to 35
# leaves every band compiling, every test passing, and every admin unable to
# seat a member; a band written against a rank no row carries is a permit that
# can never fire and reads, to anyone auditing, as authority that exists.
#
# Both failures are silent in the same way: an allow-list that nothing matches
# and an allow-list that is absent produce the identical outcome, a denial with
# no policy id recorded.
#
# THE THIRD ARM IS THE SAME SHAPE ONE LEVEL UP. `build.rs` parse-checks every
# `.cedar` under deploy/policies and LOADS none of them; the loaded set is the
# `PLATFORM_POLICY_SOURCES` array. A file committed, reviewed and never added
# to that array is valid Cedar authorizing exactly nothing.
#
# WHAT ELSE ALREADY BINDS THIS, and why this gate is still worth having:
# `zeroship-authz`'s `every_policy_file_on_disk_is_wired_into_the_loaded_set`
# makes the same comparison in Rust. It lives in the same crate as the array it
# checks, so one commit can delete the array entry and the test together and
# stay green. This is an independent instrument, in a different language, that a
# crate-local edit cannot silence - and it is the only one of the two that reads
# the migration.
#
# WHAT THIS GATE DOES NOT DO. It compares NUMBERS, not meanings. A band naming
# rank 30 for an action that should need 40 is green here; that is
# `crates/zeroship-authz/tests/platform_policies_test.rs`, which drives each
# band against a real principal at a real rank.
# ---------------------------------------------------------------------------
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"
# shellcheck source=tests/lib/gate_arms.sh
. "$ROOT/tests/lib/gate_arms.sh"
gate_arms_init organization_policy_ladder

POLICY_DIR="deploy/policies"
ENGINE="crates/zeroship-authz/src/engine.rs"
# The creator-facing statement of the ladder. A reader cannot understand two
# independent rank axes without seeing the numbers, so the doc prints them - and
# a printed number in prose is a maintenance obligation nothing enforces unless
# something re-measures it. Arm 4 is that something.
LADDER_DOC="docs/reference/control.md"

PASS=0
FAIL=0
pass() { PASS=$((PASS + 1)); echo "  ok   $1"; }
fail() { FAIL=$((FAIL + 1)); echo "  FAIL $1"; }

for path in "$POLICY_DIR" "$ENGINE" "$LADDER_DOC"; do
  [ -e "$path" ] || { echo "gate cannot run: $path is missing" >&2; exit 1; }
done

# ---------------------------------------------------------------------------
# The ladder, read from the migration that seeds it
# ---------------------------------------------------------------------------
#
# The seed is a `sql.raw` INSERT - the platform ceiling grants no DML capability
# key, so raw is the sanctioned path - written as
# `('viewer',10,0,'...')`. Reading the migration rather than a live database is
# deliberate: this gate must run with no PostgreSQL, and the migration is the
# authority the database is only a copy of.
#
# The FILE is found by content, not by name. A gate pinned to
# `20260906000000_organization_entity_model.ts` would go quietly vacuous the
# day the ladder is amended by a later migration, which is precisely when it
# most needs to run.
ladder_rows() {
  grep -rhoE "\('(viewer|developer|billing|admin|owner)',[0-9]+,[0-9]+," db/migrations-ts/*.ts \
    | tr -d "()'" | sed 's/,$//'
}

# `context.effective_rank >= N` and `context.billing_rank >= N`, per axis.
policy_thresholds() {
  grep -rhoE "context\.$1 >= [0-9]+" "$POLICY_DIR" | grep -oE '[0-9]+' | sort -un
}

echo "organization policy ladder gate"

# ---------------------------------------------------------------------------
# Arm 1: every seeded role's rank is a rank some band names
# ---------------------------------------------------------------------------
ROWS="$(ladder_rows | sort -u)"
n_roles=0
[ -n "$ROWS" ] && n_roles=$(printf '%s\n' "$ROWS" | grep -c .)

EFFECTIVE="$(policy_thresholds effective_rank)"
BILLING="$(policy_thresholds billing_rank)"

unbanded=""
while IFS=, read -r role rank billing_rank; do
  [ -n "$role" ] || continue
  printf '%s\n' "$EFFECTIVE" | grep -qx "$rank" \
    || unbanded="$unbanded
       $role (rank $rank, billing_rank $billing_rank)"
done <<< "$ROWS"

# MEASURED 2026-09-06: the ladder seeds viewer, developer, billing, admin and
# owner. Floor 4 so that removing one role is not a gate failure while losing
# the whole extraction is.
if ! gate_arm ladder_roles "$n_roles" 4; then
  fail "the ladder extraction found $n_roles role(s) in db/migrations-ts/.
       Nothing below is a statement about the bands."
elif [ -z "$unbanded" ]; then
  pass "all $n_roles ladder role(s) have a band naming their rank"
else
  fail "these roles hold a rank no policy band names:$unbanded
       A seat whose rank matches no threshold reaches nothing, and it fails as
       an ordinary non-match: no policy id is recorded, so the audit row of the
       denial looks exactly like the denial of someone with no seat at all."
fi

# ---------------------------------------------------------------------------
# Arm 2: every rank a band names is a rank some row carries
# ---------------------------------------------------------------------------
n_thresholds=0
orphaned=""
for value in $EFFECTIVE; do
  n_thresholds=$((n_thresholds + 1))
  printf '%s\n' "$ROWS" | cut -d, -f2 | grep -qx "$value" \
    || orphaned="$orphaned
       context.effective_rank >= $value"
done
for value in $BILLING; do
  n_thresholds=$((n_thresholds + 1))
  # billing_rank 0 is "no money authority" and correctly has no band; the
  # comparison runs the other way here, so a 0 in the ladder is never orphaned.
  printf '%s\n' "$ROWS" | cut -d, -f3 | grep -qx "$value" \
    || orphaned="$orphaned
       context.billing_rank >= $value"
done

# MEASURED 2026-09-06: the bands name four distinct effective ranks and two
# billing ranks.
if ! gate_arm policy_thresholds "$n_thresholds" 4; then
  fail "only $n_thresholds rank threshold(s) were found in $POLICY_DIR.
       The extraction is broken; the clean verdict above means nothing."
elif [ -z "$orphaned" ]; then
  pass "all $n_thresholds band threshold(s) name a rank the ladder carries"
else
  fail "these bands compare against a rank no ladder row holds:$orphaned
       Such a permit can never fire, and it reads to an auditor as authority
       that exists. Either the ladder moved without the band, or the band was
       written against a rank that was never seeded."
fi

# ---------------------------------------------------------------------------
# Arm 3: every .cedar file on disk is in the loaded set, and vice versa
# ---------------------------------------------------------------------------
n_policies=0
unloaded=""
empty=""
while IFS= read -r file; do
  n_policies=$((n_policies + 1))
  rel="${file#deploy/policies/}"
  grep -q "deploy/policies/$rel" "$ENGINE" \
    || unloaded="$unloaded
       $file"
  # A wired file that permits nothing is the same defect wearing the fix's
  # uniform: it is in the array, it parses, and it authorizes no action.
  grep -q '^permit' "$file" || empty="$empty
       $file"
done < <(find "$POLICY_DIR" -name '*.cedar' -print | sort)

# The trailing `\b` is load-bearing and was added after it bit: `engine.rs`
# also `include_str!`s `deploy/policies/zeroship.cedarschema`, and without the
# boundary this pattern matched its first eleven-and-a-bit characters and
# reported `deploy/policies/zeroship.cedar` as a path engine.rs cites and disk
# does not have. A prefix match reported as a whole path is a phantom the gate
# invented, not one it found.
phantom=""
while IFS= read -r cited; do
  [ -f "$cited" ] || phantom="$phantom
       $cited"
done < <(grep -oE 'deploy/policies/[a-z_/]+\.cedar\b' "$ENGINE" | sort -u)

# MEASURED 2026-09-06: the self-service baseline plus the organization bands.
# Floor 4 is under that and far above zero, which is what a `find` against a
# moved directory produces.
if ! gate_arm policy_sources "$n_policies" 4; then
  fail "found $n_policies .cedar file(s) under $POLICY_DIR. The enumeration is
       broken, so 'all of them are wired' is a statement about nothing."
elif [ -n "$unloaded" ]; then
  fail "these policy files are on disk and in no PLATFORM_POLICY_SOURCES entry:$unloaded
       build.rs parse-checks the directory and loads NOTHING. A file that is not
       in that array is valid Cedar authorizing exactly nothing - committed,
       reviewed, and inert."
elif [ -n "$phantom" ]; then
  fail "$ENGINE includes these paths, which do not exist:$phantom"
elif [ -n "$empty" ]; then
  fail "these wired policy files contain no permit statement:$empty
       Being in the array is not the same as granting anything."
else
  pass "all $n_policies policy file(s) are wired, present and carry a permit"
fi

# ---------------------------------------------------------------------------
# Arm 4: the ladder the creator doc prints is the ladder the migration seeds
# ---------------------------------------------------------------------------
#
# `docs/reference/control.md` prints the five roles and both of their integers,
# because the two-axis model is not explicable without them. That makes it a
# THIRD copy of the ladder, and the only one no compiler and no database ever
# reads - which is precisely the copy that goes stale. This arm re-measures it
# against the migration on every run, so the doc is gated rather than asserted.
n_doc_rows=0
mismatched=""
missing=""
while IFS='|' read -r _ role rank billing _; do
  role=$(printf '%s' "$role" | tr -d ' ')
  rank=$(printf '%s' "$rank" | tr -d ' ')
  billing=$(printf '%s' "$billing" | tr -d ' ')
  [ -n "$role" ] || continue
  n_doc_rows=$((n_doc_rows + 1))
  printf '%s\n' "$ROWS" | grep -qx "$role,$rank,$billing" \
    || mismatched="$mismatched
       $LADDER_DOC prints $role = ($rank, $billing); the migration seeds \
$(printf '%s\n' "$ROWS" | grep "^$role," || echo 'no such role')"
done < <(grep -E '^\| (viewer|developer|billing|admin|owner) \| [0-9]+ \| [0-9]+ \|' "$LADDER_DOC")

# Both directions: a role the migration seeds and the doc never mentions is a
# seat a creator cannot find out exists.
while IFS=, read -r role _ _; do
  [ -n "$role" ] || continue
  grep -qE "^\| $role \| [0-9]+ \| [0-9]+ \|" "$LADDER_DOC" \
    || missing="$missing
       $role"
done <<< "$ROWS"

if ! gate_arm doc_ladder "$n_doc_rows" 4; then
  fail "the ladder table in $LADDER_DOC yielded $n_doc_rows row(s). Either the
       table was reformatted out of this shape or it was removed; a doc this arm
       cannot read is a doc this arm is not checking."
elif [ -n "$mismatched" ]; then
  fail "the creator doc and the migration disagree about the ladder:$mismatched
       The doc is the copy nothing executes, so it is the one that drifts."
elif [ -n "$missing" ]; then
  fail "these seeded roles appear in no row of $LADDER_DOC:$missing
       A seat a creator cannot read about is a seat they will not use."
else
  pass "all $n_doc_rows documented role(s) match the seeded ladder, both ways"
fi

gate_arms_finish || FAIL=$((FAIL + 1))
echo "  organization policy ladder gate: $PASS passed, $FAIL failed"
[ "$FAIL" -eq 0 ] || exit 1
