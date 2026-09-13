#!/usr/bin/env bash
# No Rust site may read an identity column as a `Uuid`.
#
# THE WHOLE POINT IS THAT THIS DEFECT COMPILES. `zeroship.apps.id`,
# `zeroship.users.id` and every foreign-key copy of them are `text` holding a
# typed id (`app_<base36>`, `usr_<base36>`). A `row.get::<_, Uuid>("app_id")`
# against one of those columns type-checks perfectly and raises
# `WrongType { postgres: Text, rust: "Uuid" }` at runtime, on first contact with
# a database. `cargo check` is blind to it, `cargo test` is blind to it unless a
# live database is attached, and a crate can be comprehensively broken while
# reporting zero errors - which is exactly how it happened here.
#
# So the expectation is derived from the MIGRATION CORPUS rather than from a
# list kept by hand: arm 1 reads which identity columns `db/migrations-ts/`
# declares as `t.text()`, and arm 2 refuses any Rust read of one of those names
# into a `Uuid`. When a column's declared type changes, the expectation moves
# with it and no one has to remember this file exists.
#
# WHAT THIS DOES NOT COVER, and the omission is the important half: the BIND
# side. `db.query(sql, &[&some_uuid])` passes parameters positionally, so
# nothing in the text says which column `$1` lands in, and a bind of a `Uuid`
# against a text column fails exactly the same way. That direction needs a live
# database, and `tests/platform_migration_corpus_gate.sh` plus the live suites
# are where it gets caught. Read a green here as "no uuid READS survive",
# never as "the typed-id flip is complete".

set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"
# shellcheck source=lib/gate_arms.sh
. "$ROOT/tests/lib/gate_arms.sh"

gate_arms_init typed_id_column_read

MIGRATIONS="db/migrations-ts"
FAIL=0

# Column names whose whole semantic domain is an app id or a user id. `id` is
# included because it is the domain ROOT on `apps` and `users`; it also names
# unrelated primary keys, which is why arm 2 pairs it with a table check below.
DOMAIN='app_id|owner_app|user_id|creator_id|actor_user_id|global_user_id|principal_id|person_id|created_by|added_by|changed_by|updated_by|submitted_by|granted_by|invited_by|consumed_by|personal_owner_id|owner_id'

declared=$(mktemp)
trap 'rm -f "$declared"' EXIT

# ---- arm 1: which identity columns does the corpus declare as text? --------
for f in "$MIGRATIONS"/*.ts; do
  awk -v D="$DOMAIN" '
    $0 ~ ("^ *(" D "): t\\.text\\(\\)") {
      match($0, /^ *[a-z_]+/); col = substr($0, RSTART, RLENGTH); gsub(/ /, "", col)
      print col
    }
  ' "$f"
done | sort -u > "$declared"
# The two domain roots are spelled `id` on their own tables.
echo "id" >> "$declared"
sort -u -o "$declared" "$declared"

declared_count=$(wc -l < "$declared")
gate_arm declared_columns "$declared_count" 8

# ---- arm 2: no Rust read of one of those names into a Uuid ----------------
# Both spellings the tree uses:
#   row.get::<_, Uuid>("app_id")
#   let app_id: Uuid = row.get("app_id");
scan_uuid_reads() {
  local names="$1" root="$2"
  {
    grep -rnE "get::<_, ?(uuid::)?Uuid>\\(\"($names)\"\\)" --include='*.rs' "$root" 2>/dev/null
    grep -rnE ": ?(uuid::)?Uuid = row\\.get\\(\"($names)\"\\)" --include='*.rs' "$root" 2>/dev/null
    grep -rnE ": ?Option<(uuid::)?Uuid> = row\\.get\\(\"($names)\"\\)" --include='*.rs' "$root" 2>/dev/null
  } | sort -u
}

names=$(tr '\n' '|' < "$declared" | sed 's/|$//')
hits=$(scan_uuid_reads "$names" crates)
hit_count=0
if [ -n "$hits" ]; then
  hit_count=$(printf '%s\n' "$hits" | wc -l)
fi

# The number this arm RULED ON is the count of Rust files it searched, not the
# count of violations: a scan that found nothing because it searched nothing
# prints what a clean tree prints.
searched=$(find crates -name '*.rs' -type f | wc -l)
gate_arm rust_reads "$searched" 500

if [ "$hit_count" -ne 0 ]; then
  echo "==> $hit_count Rust site(s) read an identity column as a Uuid:" >&2
  printf '%s\n' "$hits" >&2
  echo >&2
  echo "  These columns are text holding a typed id. Each of these compiles and" >&2
  echo "  raises WrongType at runtime. Read with AppId::parse / UserId::parse" >&2
  echo "  over row.get::<_, &str>(...) instead." >&2
  FAIL=1
fi

# ---- arm 3: the anti-vacuity control --------------------------------------
# Arm 2 passes trivially if the pattern stops matching - a rename, a reformat,
# a changed turbofish spelling. Plant each spelling in a temp file the same
# scanner reads, and require it to be found.
control_dir=$(mktemp -d)
trap 'rm -f "$declared"; rm -rf "$control_dir"' EXIT
cat > "$control_dir/planted.rs" <<'PLANT'
fn a() { let _ = row.get::<_, Uuid>("app_id"); }
fn b() { let app_id: Uuid = row.get("app_id"); }
fn c() { let app_id: Option<Uuid> = row.get("app_id"); }
PLANT
control_hits=$(scan_uuid_reads "$names" "$control_dir" | wc -l)
gate_arm scanner_control "$control_hits" 3
if [ "$control_hits" -lt 3 ]; then
  echo "SCANNER BLIND: planted $((3)) violations, the scan found $control_hits." >&2
  echo "  The pattern stopped matching. Fix the pattern, do not lower the floor." >&2
  FAIL=1
fi

gate_arms_finish || FAIL=1
if [ "$FAIL" -ne 0 ]; then
  echo "typed-id column read gate: FAILED" >&2
  exit 1
fi
echo "typed-id column read gate: ok"
