#!/usr/bin/env bash
#
# Refuse a plaintext secret column stored beside its own hash.
#
# THE SHAPE THIS EXISTS FOR. `zeroship.apps` carried `api_key` AND
# `api_key_hash`. The hash is what a credential check would compare against;
# the plaintext beside it makes the hash pointless, because anything that can
# read the table already has the secret. The pair is worse than either half
# alone: it reads as "we hash our credentials" while shipping the credential.
#
# It also gated nothing. The only code that ever compared against the hash was
# the gateway's `check_api_key`, which lost its call site when RPC v1 replaced
# it with the compiled per-resource `EffectivePolicy`. Nobody removed the
# columns, so a reader arriving later found what looked like an authentication
# mechanism and was wrong twice over.
#
# WHAT THIS ARM DOES NOT RULE ON, and it matters here more than usual.
# `zeroship.apps.api_key` IS STILL IN THE CORPUS. Only the hash beside it is
# gone. This gate refuses the PAIR, and a plaintext column with no hash sibling
# is invisible to it - so a green here is not a statement that the platform
# stores no plaintext secrets. Removing `apps.api_key` needs `AppRecord::api_key`
# to go first, which has production readers; until then, read this gate as
# "no column is decorated with a hash that contradicts it", nothing wider.
#
# HOW IT DECIDES. The corpus is the authority, applied in file order: columns
# enter through a `create` block or `.column(x).add(...)`, and leave through
# `.column(x).drop(...)` or a whole-table `.drop(...)`. Raw `ALTER TABLE ... ADD
# COLUMN` is parsed too, because one migration adds columns that way and a
# parser blind to it would report a smaller, quieter corpus than exists.
#
# Arm 2 is the instrument's own control. Arm 1 alone cannot distinguish "the
# corpus is clean" from "the detector matches nothing" - the founding failure
# in tests/lib/gate_arms.sh. Arm 2 runs the SAME detector over a planted fixture
# carrying the exact shape and requires it to be caught.
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# shellcheck source=lib/gate_arms.sh
. "$ROOT/tests/lib/gate_arms.sh"

CORPUS="$ROOT/db/migrations-ts"
FAIL=0

gate_arms_init secret_beside_its_hash

# Emit `<table> <column>` for every column the corpus leaves in place, given a
# directory of migration files. Statements are normalised to one per line first,
# because the DSL chains `.column(x)` across lines as freely as it writes them
# on one.
resolve_columns() {
  local dir="$1"
  # shellcheck disable=SC2012
  local files
  files="$(ls "$dir"/*.ts 2>/dev/null | sort)"
  [ -n "$files" ] || return 0

  # shellcheck disable=SC2086
  cat $files \
    | tr '\n' ' ' \
    | sed 's/;/;\n/g' \
    | awk '
        function drop_table(t,   k) {
          for (k in col) if (index(k, t SUBSEP) == 1) delete col[k];
        }
        {
          stmt = $0;

          # Raw ALTER TABLE ... ADD COLUMN <name>, one migration uses it.
          if (stmt ~ /ALTER TABLE/ && stmt ~ /ADD COLUMN/) {
            rest = stmt;
            if (match(rest, /zeroship"?\."?[A-Za-z_][A-Za-z0-9_]*/)) {
              raw_tbl = substr(rest, RSTART, RLENGTH);
              sub(/^zeroship"?\."?/, "", raw_tbl);
              while (match(rest, /ADD COLUMN [A-Za-z_][A-Za-z0-9_]*/)) {
                c = substr(rest, RSTART + 11, RLENGTH - 11);
                col[raw_tbl, c] = 1;
                rest = substr(rest, RSTART + RLENGTH);
              }
            }
            next;
          }

          if (match(stmt, /table\("[A-Za-z_][A-Za-z0-9_]*"/) == 0) next;
          tbl = substr(stmt, RSTART + 7, RLENGTH - 8);

          if (stmt ~ /\)[[:space:]]*\.drop\(/ && stmt !~ /\.column\(/) {
            drop_table(tbl);
            next;
          }

          if (match(stmt, /\.column\("[A-Za-z_][A-Za-z0-9_]*"\)/) > 0) {
            c = substr(stmt, RSTART + 9, RLENGTH - 11);
            if (stmt ~ /\.add\(/)  col[tbl, c] = 1;
            if (stmt ~ /\.drop\(/) delete col[tbl, c];
            next;
          }

          if (stmt ~ /\.create\(/ && match(stmt, /columns:[[:space:]]*\{/) > 0) {
            rest = substr(stmt, RSTART);
            while (match(rest, /[A-Za-z_][A-Za-z0-9_]*:[[:space:]]*t\./)) {
              c = substr(rest, RSTART, RLENGTH);
              sub(/:[[:space:]]*t\.$/, "", c);
              col[tbl, c] = 1;
              rest = substr(rest, RSTART + RLENGTH);
            }
          }
        }
        END {
          for (k in col) {
            split(k, p, SUBSEP);
            print p[1], p[2];
          }
        }
      '
}

# Report `<table>.<column>` for every plaintext column that has a `_hash`
# sibling on the same table, then one final `ruled <n>` line carrying the number
# of columns this pass DECIDED on. The count rides on stdout, not stderr: awk
# writes its own diagnostics to stderr, and a warning arriving where a count was
# expected is how the first run of this gate refused itself.
find_pairs() {
  local dir="$1"
  resolve_columns "$dir" | awk '
    { have[$1 SUBSEP $2] = 1; tbl[NR] = $1; c[NR] = $2; n = NR }
    END {
      ruled = 0;
      for (i = 1; i <= n; i++) {
        # Rule on the PLAINTEXT half: for each column, ask whether a column
        # named after it plus `_hash` sits on the same table. Counting the
        # hash halves instead would double-count nothing and miss a hash whose
        # plaintext was never declared, which is the safe shape.
        if (c[i] ~ /_hash$/) continue;
        ruled++;
        if ((tbl[i] SUBSEP c[i] "_hash") in have) print tbl[i] "." c[i];
      }
      printf "ruled %d\n", ruled;
    }
  '
}

ruled_count() { printf '%s\n' "$1" | sed -n 's/^ruled \([0-9][0-9]*\)$/\1/p' | tail -1; }
pair_lines()  { printf '%s\n' "$1" | grep -v '^ruled [0-9]*$' | grep -v '^$'; }

# --- arm 1: the committed platform corpus ---------------------------------
CORPUS_OUT="$(find_pairs "$CORPUS")"
RULED="$(ruled_count "$CORPUS_OUT")"
PAIRS="$(pair_lines "$CORPUS_OUT")"

gate_arm corpus_columns "${RULED:-0}" 200 || FAIL=$((FAIL + 1))

if [ -z "$PAIRS" ]; then
  echo "  ok   no platform column is stored beside its own hash"
else
  echo "FAIL: a plaintext secret column is stored beside its own hash:" >&2
  printf '%s\n' "$PAIRS" >&2
  echo "  Store the hash or the secret, never both. Whatever reads the table" >&2
  echo "  already holds the plaintext, so the digest protects nothing." >&2
  FAIL=$((FAIL + 1))
fi

# --- arm 2: the detector's own control ------------------------------------
#
# Plant the refused shape and require a catch. Without this, arm 1 going green
# is equally consistent with a parser that stopped matching the DSL - which is
# how four gates in this tree were found examining nothing.
FIXTURE="$(mktemp -d)"
trap 'rm -rf "$FIXTURE"' EXIT HUP INT TERM

cat > "$FIXTURE/20000101000000_planted.ts" <<'PLANT'
import { table, t } from "@zeroship/migrate";
export default {
  name: "planted",
  schema() {
    table("planted_pair", { schema: "zeroship" }).create({
      columns: {
        id: t.text().notNull(),
        session_key: t.text().notNull(),
        session_key_hash: t.text().notNull(),
        unrelated: t.text(),
      },
      primaryKey: ["id"],
    });
    table("planted_clean", { schema: "zeroship" }).create({
      columns: {
        id: t.text().notNull(),
        token_hash: t.text().notNull(),
      },
      primaryKey: ["id"],
    });
  },
};
PLANT

# A second file, so the control also proves the resolver carries state ACROSS
# files: this one removes the plaintext half, and the planted pair must then
# stop being reported. A detector that only ever read one file would keep
# flagging it, and a detector that ignored drops would too.
cat > "$FIXTURE/20000101000100_planted_fixed.ts" <<'PLANT'
import { table } from "@zeroship/migrate";
export default {
  name: "planted_fixed",
  schema() {
    table("planted_pair", { schema: "zeroship" }).column("session_key").drop({ ifExists: true });
  },
};
PLANT

# Case: both files present - the pair was created and then repaired, so clean.
FIXED="$(pair_lines "$(find_pairs "$FIXTURE")")"
# Control: only the file that creates the pair - must be caught.
rm "$FIXTURE/20000101000100_planted_fixed.ts"
PLANT_OUT="$(find_pairs "$FIXTURE")"
CAUGHT="$(pair_lines "$PLANT_OUT")"
PLANT_RULED="$(ruled_count "$PLANT_OUT")"

gate_arm planted_control "${PLANT_RULED:-0}" 3 || FAIL=$((FAIL + 1))

if [ "$CAUGHT" = "planted_pair.session_key" ]; then
  echo "  ok   the detector catches a planted plaintext-beside-hash pair"
else
  echo "FAIL: the detector did NOT catch the planted pair; arm 1 rules on nothing." >&2
  echo "      expected 'planted_pair.session_key', got: ${CAUGHT:-<nothing>}" >&2
  FAIL=$((FAIL + 1))
fi

if [ -z "$FIXED" ]; then
  echo "  ok   dropping the plaintext half clears the finding across files"
else
  echo "FAIL: the detector ignores a later drop, so it reports repaired shapes." >&2
  echo "      expected nothing, got: $FIXED" >&2
  FAIL=$((FAIL + 1))
fi

gate_arms_finish || FAIL=$((FAIL + 1))

echo "  secret-beside-its-hash gate: $FAIL failure(s)"
[ "$FAIL" -eq 0 ] || exit 1
