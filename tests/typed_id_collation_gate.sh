#!/usr/bin/env bash
# Every platform column in the app or user identity domain must carry
# `COLLATE "C"`, and no collation map may name a table the corpus has already
# dropped.
#
# Both halves are silent when broken, which is why they are a gate rather than a
# convention:
#
#   * A MISSED COPY degrades a join instead of erroring. `apps.id` orders
#     bytewise; a copy left on the database's locale collation cannot serve that
#     join from its own index, and nothing reports it.
#   * A DROPPED TABLE in a collation map fails the apply outright, but only on a
#     fresh database - a developer's already-migrated database skips the file and
#     stays green while CI goes red.
#
# The domain is decided by COLUMN NAME, which is exact for the identity columns
# and over-matches on a handful of actor labels that are not entity ids. Those
# are listed in EXCLUDED with the reason each is not in the domain; the list is
# checked for staleness, so an entry that stops matching anything fails the gate
# rather than sitting there.

set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"
# shellcheck source=lib/gate_arms.sh
. "$ROOT/tests/lib/gate_arms.sh"

gate_arms_init typed_id_collation

MIGRATIONS="db/migrations-ts"
FAIL=0

# Column names whose whole semantic domain is an app id or a user id. `owner_app`
# is `billing_metrics`'s spelling of an app reference; the `*_by` names are actor
# columns that carry a user id.
DOMAIN='app_id|owner_app|user_id|creator_id|actor_user_id|global_user_id|principal_id|person_id|created_by|added_by|changed_by|updated_by|submitted_by|granted_by|invited_by|consumed_by|personal_owner_id|owner_id'

# (table|column) pairs the name match reaches that are NOT in the domain, and
# (table|column) pairs whose table does not exist at collation time. Each line is
# `table|column  # reason`.
EXCLUDED_RAW='
app_egress_rules|created_by       # a free-text actor label, no foreign key to users
workflow_rollout_config|updated_by # a free-text actor label, no foreign key to users
workflow_signals|consumed_by      # the workflow step that consumed the signal, not a user
app_net_grants|app_id             # table dropped in 20260820000100_app_egress_rules
app_net_grants|granted_by         # table dropped in 20260820000100_app_egress_rules
metering_exports|creator_id       # table dropped in 20260709000100_drop_metering_exports
net_policy_catalog|updated_by     # table dropped in 20260817000500_drop_net_policy_catalog
permission_tokens|owner_id        # table dropped in 20260817000000_drop_permission_tokens
platform_admin_roles|user_id      # table dropped in 20260817000400_drop_platform_admin_roles
platform_admin_roles|granted_by   # table dropped in 20260817000400_drop_platform_admin_roles
platform_policies|updated_by      # table dropped in 20260817000300_drop_platform_policies
'
EXCLUDED=$(echo "$EXCLUDED_RAW" | sed -E 's/[[:space:]]*#.*$//' | sed '/^[[:space:]]*$/d' | tr -d ' ')

declared=$(mktemp)
collated=$(mktemp)
trap 'rm -f "$declared" "$collated"' EXIT

# Every domain column declared as text, with the table that owns it. The two
# domain ROOTS are named outright: `apps.id` and `users.id` are spelled `id`,
# which is far too common a name to match on.
for f in "$MIGRATIONS"/*.ts; do
  awk -v D="$DOMAIN" '
    /table\("|zs\("/ {
      if (match($0, /table\("[a-z_]+"/)) tbl = substr($0, RSTART + 7, RLENGTH - 8)
      else if (match($0, /zs\("[a-z_]+"/)) tbl = substr($0, RSTART + 4, RLENGTH - 5)
    }
    $0 ~ ("^ *(" D "): t\\.text\\(\\)") {
      match($0, /^ *[a-z_]+/); col = substr($0, RSTART, RLENGTH); gsub(/ /, "", col)
      print tbl "|" col
    }
  ' "$f"
done | sort -u > "$declared"
printf 'apps|id\nusers|id\n' >> "$declared"
sort -u -o "$declared" "$declared"

# Every (table, column) any collation map names.
grep -hoE '^ *[a-z_]+: \[[^]]*\]' "$MIGRATIONS"/*.ts | tr -d ' ' | while IFS= read -r line; do
  t="${line%%:*}"
  cols="${line#*:[}"
  cols="${cols%]}"
  echo "$cols" | tr ',' '\n' | tr -d '"' | while IFS= read -r c; do
    [ -n "$c" ] && echo "$t|$c"
  done
done | sort -u > "$collated"

# ---- arm 1: every domain column is collated -------------------------------
missing=0
ruled=0
while IFS= read -r row; do
  ruled=$((ruled + 1))
  if echo "$EXCLUDED" | grep -qxF "$row"; then
    continue
  fi
  if ! grep -qxF "$row" "$collated"; then
    echo "NOT COLLATED: $row" >&2
    missing=$((missing + 1))
  fi
done < "$declared"
if [ "$missing" -ne 0 ]; then
  echo "==> $missing identity column(s) lack COLLATE \"C\"" >&2
  FAIL=1
fi
gate_arm coverage "$ruled" 60

# ---- arm 2: the exclusion list is not stale -------------------------------
# An entry that matches no declared column is an exclusion nobody can evaluate,
# and it hides the next real miss behind a name that no longer exists.
stale=0
excl_ruled=0
while IFS= read -r row; do
  [ -z "$row" ] && continue
  excl_ruled=$((excl_ruled + 1))
  if ! grep -qxF "$row" "$declared"; then
    echo "STALE EXCLUSION: $row matches no declared column" >&2
    stale=$((stale + 1))
  fi
done <<< "$EXCLUDED"
if [ "$stale" -ne 0 ]; then
  echo "==> $stale stale exclusion(s)" >&2
  FAIL=1
fi
gate_arm exclusions "$excl_ruled" 8

# ---- arm 3: the anti-vacuity control --------------------------------------
# Arm 1 passes trivially if the declared set is empty or the parser stops
# matching. Prove it sees the two domain roots and at least one copy of each.
control=0
for row in "apps|id" "users|id" "app_secrets|app_id" "organization_members|user_id"; do
  if grep -qxF "$row" "$declared"; then
    control=$((control + 1))
  else
    echo "SCANNER BLIND: expected $row among the declared identity columns" >&2
    FAIL=1
  fi
done
gate_arm scanner_control "$control" 4

gate_arms_finish || FAIL=1
if [ "$FAIL" -ne 0 ]; then
  echo "typed-id collation gate: FAILED" >&2
  exit 1
fi
echo "typed-id collation gate: ok"
