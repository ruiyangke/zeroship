#!/usr/bin/env bash
# ============================================================================
# A COMMENT SAYING TWO CRATES AGREE MUST BE ON THE LEDGER THAT SAYS WHAT HOLDS
# THEM THERE.
#
# THE DEFECT, found ten times in this tree and twice fixed in the week this
# gate was written: a doc comment ASSERTS that two artifacts in two crates stay
# in agreement, and NOTHING enforces it. The comment answers the auditor's
# question before anyone goes and checks, so the pair is never checked.
#
#   crates/zeroship-data-sql/src/mask_codec.rs  and its peer in
#   zeroship-migrate-backend both cited a round-trip guard in
#   crates/zeroship-data-orm/src/tests/postgres/protection.rs. Database tests now
#   run by default, but a builder round trip alone cannot detect disagreement
#   with a separate parser. The codec contract is bound by `mod cross_codec_parity`.
#
#   crates/zeroship-data-sql/src/compile.rs and zeroship-migrate-backend/src/schema.rs
#   both said `raw_column_name` "must stay byte-identical" AND that "two hashing
#   implementations in two crates could not be checked to agree by any
#   compiler". That is a statement of the hazard wearing the uniform of a guard.
#   Now bound by `mod raw_column_parity`.
#
# WHAT THIS GATE IS. A RATCHET, not a semantic judge. It cannot tell whether an
# agreement really holds - that needs an assertion in Rust, which is the thing
# it is pushing people to write. It rules on ONE question: has a cross-crate
# agreement claim appeared that nobody has ruled on? Every such claim in the
# data-plane and migration roots is on the LEDGER below with a verdict, and a
# claim that is not on the ledger is a refusal.
#
# WHY THE LEDGER IS KEYED BY (FILE, NAMED CRATE) AND NOT BY LINE. Line numbers
# in these files drift under ordinary editing - `zeroship-data-sql/src/compile.rs`
# moved one of these claims by 125 lines during the sweep that produced this
# ledger. A line-keyed ledger would go red on a comment reflow, be relaxed, and
# stop meaning anything. The pair "this file claims agreement with that crate"
# survives reflows and renames of the surrounding prose, and it is the unit a
# reader has to rule on anyway.
#
# THE REVERSE DIRECTION MATTERS AS MUCH. Arm 2 refuses a ledger row that the
# tree no longer produces. That is how a deleted claim gets its ledger row
# deleted rather than left behind as an exemption nobody remembers granting -
# and, because the ledger is what arm 1 measures itself against, a stale row is
# also how the enumeration could quietly stop matching without arm 1 noticing.
#
# WHAT THIS GATE DOES NOT DO, stated so nobody reads it as complete:
#
#   - It does not verify that a `bound=` row's guard actually PROVES the
#     agreement. Arm 3 checks the guard is still THERE (a named module or
#     function in a named file), not that it is correct or that it still covers
#     the same corpus. A guard emptied of its assertions passes arm 3.
#   - It does not find agreement claims that name no crate, and the worked
#     example of that is now HISTORY rather than a live row. Until 2026-09-04
#     crates/zeroship-migrate-server/src/session.rs claimed parity with a
#     `PgSession` impl, and with `SeamBind` / `SeamRow` / `SeamError` beside it.
#     All four names occurred nowhere else in the tree - they were never written.
#     That claim reached this ledger only because a NEIGHBOURING line happened to
#     say `compio_postgres`; had it not, the gate would have been blind to a
#     comment asserting agreement with four symbols that did not exist. The
#     comment is rewritten and its row is gone, so the blind spot no longer has a
#     live illustration - which is the point of writing it down here.
#
#     Note also what a SUBSTRING grep does to that class: `PgSession` survives
#     inside `CompioPgSession`, so `grep PgSession` reports hits and the phantom
#     reads as real. It took `grep -P '(?<!Compio)PgSession'` to see it.
#   - It reads the data-plane and migration roots ONLY (see ROOTS). The same
#     shape exists elsewhere in the workspace and is not measured.
#   - `prose` rows are a judgement a human made once, by reading. A claim
#     mis-filed as prose stays mis-filed until someone re-reads it.
#
# Run the detector's own positive/control pair: this script --self-test.
# ============================================================================
set -uo pipefail
cd "$(dirname "$0")/.."

# shellcheck source=tests/lib/gate_arms.sh
. "$(dirname "$0")/lib/gate_arms.sh"
gate_arms_init sync_claim

PASS=0
FAIL=0
pass() { PASS=$((PASS + 1)); echo "  ok   $1"; }
fail() { FAIL=$((FAIL + 1)); echo "  FAIL $1"; }

# ---------------------------------------------------------------------------
# THE ROOTS. The data plane and the migration engine - the two sides of every
# fork this defect has been found in. Both halves of each already-fixed pair
# live in here, which is the point: a root set that contained only one side
# could not see a cross-boundary claim at all.
# ---------------------------------------------------------------------------
ROOTS="crates/zeroship-data-cdc-server/src
crates/zeroship-data-orm/src
crates/zeroship-data-sql/src
crates/zeroship-data-v8/src
crates/zeroship-migrate/src
crates/zeroship-migrate-backend/src
crates/zeroship-migrate-core/src
crates/zeroship-migrate-ir/src
crates/zeroship-migrate-mysql/src
crates/zeroship-migrate-node/src
crates/zeroship-migrate-policy/src
crates/zeroship-migrate-postgres/src
crates/zeroship-migrate-server/src
crates/zeroship-migrate-sqlite/src
crates/zeroship-data-cdc-wire/src
libs/compio-postgres/src"

# ---------------------------------------------------------------------------
# THE ENUMERATION, as functions so --self-test can drive the same code the
# real run uses.
#
# A CLAIM is a comment line carrying an agreement phrase. A CROSS-CRATE claim
# additionally names, within two comment lines either side, a crate that is not
# the file's own and that EXISTS on disk. Both filters earn their place:
#
#   - the +/-2 line window, rather than the whole comment block, because a
#     module-doc block that happens to list eight crate names produced eight
#     spurious pairs when measured over the block (91 pairs, against 33 for the
#     window). A pair the reader cannot see from the claim is not a claim.
#   - the on-disk existence check, because `zeroship_schema_migrations` and
#     `zeroship_audit_unmask` are TABLE names that match the crate-token regex
#     exactly. Filtering by what is really a crate removed both.
#
# NO `2>/dev/null` ON ANYTHING THAT FEEDS A COUNT. A command that failed and a
# command that found nothing print the same number, and the zero branch here is
# "no unledgered claims", i.e. a PASS.
# ---------------------------------------------------------------------------

# The agreement phrases, matched case-insensitively. Extend deliberately: a
# phrase added here can only ADD ledger rows, never remove one, so a widening
# shows up as an arm-1 refusal naming the new pairs rather than as silence.
#
# `must equal` joined the list on 2026-09-04, found by anchoring arm 2's lookup
# (see there). `crates/zeroship-data-sql/src/ident.rs:44` had rewritten its own
# claim away from "byte-for-byte identical" - which matches nothing here, the
# hyphenation differs - to the conditional obligation "`cap_ident_name(n)` must
# equal `plan::author::cap_ident_name(zeroship_migrate::shipping_vendors(), n)`".
# The claim got MORE precise and left the enumeration in the same edit.
# Measured: it adds exactly one pair, ident.rs <-> zeroship_migrate, which the
# ledger already carried, so arm 1 is unchanged and arm 2 goes clean.
CLAIM_PHRASES='byte-identical|byte identical|must agree|must match|must equal|kept in sync|stay in sync|stays in sync|in lockstep|must not drift|cannot drift|identical to|mirror of|copy of|one spelling|spelled the same|keep them that way|must stay'

# known_crates <out-file> - every crate/lib directory name, underscored.
known_crates() {
  { ls -1 crates; ls -1 libs; } | sed 's/-/_/g' | LC_ALL=C sort -u > "$1"
}

# claim_pairs <known-file> <root>... - emit `<file>\t<crate>` rows, sorted and
# deduplicated.
claim_pairs() {
  local known="$1"; shift
  find "$@" -name '*.rs' -type f | LC_ALL=C sort | while IFS= read -r f; do
    awk -v FILE="$f" -v KNOWN="$known" -v PHRASES="$CLAIM_PHRASES" '
      BEGIN { while ((getline k < KNOWN) > 0) if (k != "") known[k] = 1 }
      { L[NR] = $0 }
      END {
        own = FILE
        sub(/^crates\//, "", own); sub(/^libs\//, "", own); sub(/\/.*$/, "", own)
        gsub(/-/, "_", own)
        for (i = 1; i <= NR; i++) {
          if (L[i] !~ /^[ \t]*(\/\/\/|\/\/!|\/\/)/) continue
          # tolower() rather than a case-insensitive regex: gawk IGNORECASE is
          # not POSIX, and the first draft, being case-SENSITIVE, silently
          # dropped every claim that began a sentence ("Identical to ...").
          if (tolower(L[i]) !~ PHRASES) continue
          lo = i - 2; if (lo < 1) lo = 1
          hi = i + 2; if (hi > NR) hi = NR
          win = ""
          for (j = lo; j <= hi; j++)
            if (L[j] ~ /^[ \t]*(\/\/\/|\/\/!|\/\/)/) win = win " " L[j]
          s = win
          while (match(s, /zeroship_[a-z_]+|zeroship-[a-z-]+|compio_postgres|compio-postgres/)) {
            tok = substr(s, RSTART, RLENGTH)
            s = substr(s, RSTART + RLENGTH)
            gsub(/-/, "_", tok)
            if (tok == own) continue
            if (!(tok in known)) continue
            print FILE "\t" tok
          }
        }
      }
    ' "$f"
  done | LC_ALL=C sort -u
}

# ---------------------------------------------------------------------------
# THE LEDGER. One row per cross-crate agreement claim, `<file> <crate> <verdict>`.
#
#   bound=<guard-file>#<needle>   something enforces it. Arm 3 checks the needle
#                                 still appears in the guard file.
#   unbound                       ruled on by reading; NOTHING enforces it. On
#                                 the ledger so it is recorded rather than
#                                 rediscovered, and so a NEW one is refused.
#   prose                         the phrase describes one artifact's behaviour
#                                 ("byte-identical output", "the same refusal"),
#                                 not an obligation between two. Nothing to bind.
#
# Verdicts assigned 2026-09-04 by reading each claim. RE-READ A ROW BEFORE
# TRUSTING IT: `prose` is the one verdict this gate cannot re-derive, and it is
# also the one a rewrite could quietly turn into a real claim.
# ---------------------------------------------------------------------------
LEDGER='
crates/zeroship-data-v8/src/tests/postgres/workflow.rs	zeroship_migrate_server	bound=crates/zeroship-data-v8/src/tests/postgres/workflow.rs#async fn workflow_journal_redeploy_grants_do_not_reopen_without_reprovision
crates/zeroship-data-orm/src/backend/postgres/pg_session_sql.rs	zeroship_migrate_server	unbound
crates/zeroship-data-orm/src/exec.rs	compio_postgres	prose
crates/zeroship-data-orm/src/backend/sqlite/vector.rs	zeroship_migrate_core	bound=crates/zeroship-data-orm/src/tests/sqlite/search.rs#fn engine_shadow_relation
crates/zeroship-migrate-backend/src/backend.rs	zeroship_migrate_postgres	prose
crates/zeroship-migrate-backend/src/constraint_definition.rs	zeroship_migrate	prose
crates/zeroship-migrate-backend/src/ddl.rs	zeroship_migrate	prose
crates/zeroship-migrate-backend/src/dml.rs	zeroship_migrate_sqlite	prose
crates/zeroship-migrate-backend/src/mask_codec.rs	zeroship_data_sql	bound=crates/zeroship-data-sql/src/mask_codec.rs#mod cross_codec_parity
crates/zeroship-migrate-backend/src/schema.rs	zeroship_data_sql	bound=crates/zeroship-data-sql/src/compile.rs#mod raw_column_parity
crates/zeroship-migrate-core/src/apply/executor.rs	zeroship_migrate_sqlite	prose
crates/zeroship-migrate-core/src/ops/squash.rs	zeroship_migrate_postgres	prose
crates/zeroship-migrate-core/src/render/backends/mod.rs	zeroship_migrate_backend	prose
crates/zeroship-migrate-core/src/render/dml.rs	zeroship_migrate_postgres	prose
crates/zeroship-migrate-core/src/render/fold.rs	zeroship_migrate	bound=crates/zeroship-migrate/tests/fold_live/fold_roundtrip_pg.rs#async fn assert_roundtrip
crates/zeroship-migrate-core/src/render/lower.rs	zeroship_migrate	bound=crates/zeroship-migrate/tests/ir_contract/ir_author_render_parity.rs#fn create_table_render_is_byte_identical_pg
crates/zeroship-migrate-core/src/render/vendor.rs	zeroship_migrate_postgres	prose
crates/zeroship-migrate-core/src/schema/query.rs	zeroship_migrate	prose
crates/zeroship-migrate-core/src/schema/query.rs	zeroship_migrate_backend	prose
crates/zeroship-migrate-mysql/src/backend/backfill_sql.rs	zeroship_migrate_postgres	prose
crates/zeroship-migrate-postgres/src/backend/journal_sql.rs	zeroship_migrate_backend	prose
crates/zeroship-migrate-postgres/src/backend/session.rs	zeroship_migrate_backend	prose
crates/zeroship-migrate-postgres/src/role.rs	zeroship_migrate_backend	prose
crates/zeroship-migrate-postgres/src/role.rs	zeroship_migrate_ir	prose
crates/zeroship-migrate-sqlite/src/backend/actor.rs	zeroship_migrate_backend	bound=crates/zeroship-migrate-sqlite/src/backend/actor.rs#step::BindValue::Bytes
crates/zeroship-migrate-sqlite/src/backend/audit_unmask_sql.rs	zeroship_migrate_server	bound=crates/zeroship-data-v8/tests/audit_table_parity.rs#POSTGRES_CREATOR
crates/zeroship-migrate-sqlite/src/backend/audit_unmask_sql.rs	zeroship_data_v8	bound=crates/zeroship-data-v8/tests/audit_table_parity.rs#SQLITE_CREATOR
crates/zeroship-migrate-sqlite/src/dml.rs	zeroship_migrate_backend	prose
crates/zeroship-migrate-sqlite/src/schema.rs	zeroship_data_sql	bound=crates/zeroship-data-sql/src/compile.rs#mod sqlite_now_parity
crates/zeroship-data-sql/src/mask_codec.rs	zeroship_migrate_backend	bound=crates/zeroship-data-sql/src/mask_codec.rs#mod cross_codec_parity
crates/zeroship-data-sql/src/compile.rs	zeroship_migrate_backend	bound=crates/zeroship-data-sql/src/compile.rs#mod raw_column_parity
'

# WHY THE ROW ABOVE FOR `actor.rs` NAMES A VARIANT AND NOT THE FUNCTION.
# `SqliteBind::from_bind` matches `BindValue` exhaustively with no `_` arm, so
# the COMPILER is the guard: a new `BindValue` variant fails the build. A needle
# of `fn from_bind` would survive someone adding a wildcard arm - which is
# exactly how that guard would be lost - so the needle is the LAST variant the
# match names. Delete the arms in favour of a wildcard and this goes red.

# --------------------------------------------------------------------------
# --self-test: does the detector actually detect, and does it discriminate?
# --------------------------------------------------------------------------
self_test() {
  echo "sync claim gate self-test"
  local tmp status=0 found
  tmp="$(mktemp -d)"
  trap 'rm -rf "$tmp"' RETURN
  mkdir -p "$tmp/crates/zeroship-alpha/src" "$tmp/crates/zeroship-beta/src"
  printf 'zeroship_alpha\nzeroship_beta\n' > "$tmp/known"

  # POSITIVE: a claim naming another crate that exists.
  cat > "$tmp/crates/zeroship-alpha/src/lib.rs" <<'RS'
/// Byte-identical to `zeroship_beta::thing::PREFIX`; nothing checks it.
pub const PREFIX: &str = "x";
RS
  found="$( (cd "$tmp" && claim_pairs "$tmp/known" crates/zeroship-alpha/src) )"
  if [ "$found" = "$(printf 'crates/zeroship-alpha/src/lib.rs\tzeroship_beta')" ]; then
    echo "  ok   a cross-crate agreement claim is extracted"
  else
    echo "  FAIL the extraction found [$found]; the arm detects nothing"
    status=1
  fi

  # NEGATIVE CONTROL, one variable: the SAME comment naming the SAME crate,
  # with the agreement phrase removed. Without this the positive proves only
  # that the crate-token regex ran - an extractor that emitted a pair for every
  # crate mention would pass the positive too, and would bury the ledger.
  cat > "$tmp/crates/zeroship-alpha/src/lib.rs" <<'RS'
/// Documented in `zeroship_beta::thing`, which owns the other half.
pub const PREFIX: &str = "x";
RS
  found="$( (cd "$tmp" && claim_pairs "$tmp/known" crates/zeroship-alpha/src) )"
  if [ -z "$found" ]; then
    echo "  ok   the same citation WITHOUT an agreement phrase is not a claim"
  else
    echo "  FAIL a bare crate citation was reported as a claim: [$found]"
    status=1
  fi

  # A claim about the file's OWN crate is not a cross-crate claim. Without this
  # filter every "byte-identical to the arm above" in a large crate lands on the
  # ledger, and a ledger of self-references is one nobody reads.
  cat > "$tmp/crates/zeroship-alpha/src/lib.rs" <<'RS'
/// Byte-identical to `zeroship_alpha::other::PREFIX` in this same crate.
pub const PREFIX: &str = "x";
RS
  found="$( (cd "$tmp" && claim_pairs "$tmp/known" crates/zeroship-alpha/src) )"
  if [ -z "$found" ]; then
    echo "  ok   a claim about the file's own crate is not a cross-crate claim"
  else
    echo "  FAIL a same-crate claim was reported: [$found]"
    status=1
  fi

  # A token shaped like a crate that is NOT one on disk (a table name) must not
  # produce a pair. `zeroship_schema_migrations` and `zeroship_audit_unmask` are
  # both real strings in this tree and both match the token regex.
  cat > "$tmp/crates/zeroship-alpha/src/lib.rs" <<'RS'
/// Must match the `zeroship_schema_migrations` journal table's spelling.
pub const T: &str = "x";
RS
  found="$( (cd "$tmp" && claim_pairs "$tmp/known" crates/zeroship-alpha/src) )"
  if [ -z "$found" ]; then
    echo "  ok   a crate-shaped token that is not a crate is not a pair"
  else
    echo "  FAIL a table name was reported as a crate: [$found]"
    status=1
  fi

  return "$status"
}

if [ "${1:-}" = "--self-test" ]; then
  self_test
  exit $?
fi

for r in $ROOTS; do
  [ -d "$r" ] || { echo "gate cannot run: root $r is missing"; exit 1; }
done

echo "sync claim gate"

KNOWN="$(mktemp)"
trap 'rm -f "$KNOWN"' EXIT
known_crates "$KNOWN"

PAIRS="$(claim_pairs "$KNOWN" $ROOTS)"
N_PAIRS=0
[ -n "$PAIRS" ] && N_PAIRS="$(printf '%s\n' "$PAIRS" | grep -c .)"

LEDGER_ROWS="$(printf '%s\n' "$LEDGER" | grep -c .)"

# --- Arm 1: every claim in the tree is on the ledger -----------------------
#
# THE FLOOR. 34 pairs at the time of writing, across the `.rs` files under
# ROOTS.
#
# THAT NUMBER IS A SNAPSHOT AND IT ROTS. It read 33 for part of 2026-09-04 and
# was 34 by the end of the same day: repointing the citations the crate rename
# broke put `zeroship-migrate` next to an agreement phrase in `render/fold.rs`
# and `render/lower.rs`, so two REAL cross-crate claims became visible to the
# enumeration that had not been. Nothing was added to the tree - the detector
# simply started seeing what was already there. Expect this to happen again;
# read the gate's own output for today's count, not this line.
#
# Set at 24, well under that: comment edits move this number by ones,
# while the failure it guards - the phrase list stops matching, a root moves,
# the crate-token regex breaks - takes it toward zero, not to 23. Do not raise
# it to today's count: every deleted claim would then be a gate failure, and
# deleting an unbound claim by binding or removing it is the OUTCOME this gate
# exists to encourage.
CLAIM_PAIR_FLOOR=24

unledgered=""
n_ruled=0
while IFS= read -r row; do
  [ -n "$row" ] || continue
  n_ruled=$((n_ruled + 1))
  file="${row%%	*}"
  crate="${row#*	}"
  if ! printf '%s\n' "$LEDGER" | grep -qF "$file	$crate	"; then
    unledgered="$unledgered
    $file  <->  $crate"
  fi
done <<EOF
$PAIRS
EOF

if ! gate_arm claim_pairs "$n_ruled" "$CLAIM_PAIR_FLOOR"; then
  fail "the claim enumeration ruled on $n_ruled pair(s), under its floor of
       $CLAIM_PAIR_FLOOR. Whatever it reports about the ledger is meaningless:
       fix the enumeration, do not lower the floor."
elif [ -z "$unledgered" ]; then
  pass "all $n_ruled cross-crate agreement claim(s) are on the ledger"
else
  fail "these comments assert that two crates agree, and no ledger row rules on
       them:$unledgered

       A comment claiming an agreement that nothing enforces is the defect this
       gate exists for - it answers the auditor's question before anyone checks.
       Do ONE of these, then add the row to LEDGER in this file:
         * write the assertion and record it as bound=<file>#<needle>;
         * rule that nothing enforces it and record it as unbound, so the next
           reader inherits the finding instead of rediscovering it;
         * rule that the phrase describes ONE artifact and record it as prose.
       Do not delete the phrase to silence this - that loses the finding."
fi

# --- Arm 2: every ledger row still matches something -----------------------
#
# The reverse direction, and it doubles as arm 1's positive control: a ledger
# row matching nothing is either an exemption nobody removed or - the case that
# matters - proof that the enumeration stopped matching, which arm 1's floor
# would only catch once the collapse was near-total.
#
# `-qxF`, NOT `-qF`, AND THAT ONE LETTER WAS HIDING A REAL FINDING. A `$PAIRS`
# row is exactly `<file><TAB><crate>`, so an unanchored match lets a row be
# satisfied by any crate whose name it PREFIXES. Arm 1 above already guards
# against this by matching `$file\t$crate\t` with the terminator; this arm had
# no terminator to use, and no anchor either. Measured 2026-09-04: one ledger
# row - `crates/zeroship-data-sql/src/ident.rs` <-> `zeroship_migrate` - was being
# vouched for by the `zeroship_migrate_core` pair, and the claim it records
# ("`cap_ident_name(n)` must equal `plan::author::cap_ident_name(...)`",
# ident.rs:47-49) was invisible to the enumeration because `must equal` was not
# a claim phrase. That is the "enumeration stopped matching" case in this arm's
# own comment, and the prefix match is what kept it quiet.
stale=""
n_ledger=0
while IFS= read -r row; do
  [ -n "$row" ] || continue
  n_ledger=$((n_ledger + 1))
  file="$(printf '%s' "$row" | cut -f1)"
  crate="$(printf '%s' "$row" | cut -f2)"
  printf '%s\n' "$PAIRS" | grep -qxF "$file	$crate" || stale="$stale
    $file  <->  $crate"
done <<EOF
$LEDGER
EOF

# Floor 24 for the same reason as arm 1, and deliberately the same number: the
# two counts move together, so a divergence between them is the interesting
# signal and neither floor should be the thing that fires first.
if ! gate_arm ledger_liveness "$n_ledger" 24; then
  fail "the ledger has collapsed to $n_ledger row(s). It is the reference arm 1
       measures against, so arm 1's result above says nothing."
elif [ -z "$stale" ]; then
  pass "all $n_ledger ledger row(s) still match a claim in the tree"
else
  fail "these ledger rows match no claim in the tree:$stale
       Either the comment was deleted - remove the row in the same commit - or
       the enumeration stopped matching it, in which case arm 1's clean result
       above means nothing."
fi

# --- Arm 3: every claimed binding is still there ---------------------------
#
# WHAT THIS RULES ON, exactly: the guard a `bound=` row names is still present
# in the file it names. NOT that it is correct, NOT that it still covers the
# same corpus. `mask_flip.rs` was cited for months as the mask-codec guard and
# was present the whole time; presence was never the problem there. This arm
# catches the cheaper failure - a guard deleted or renamed while the comment
# that cites it stays - which is the ws_subscription_stub shape.
#
# Floor 4 against 8 bound rows today. Binding an unbound claim RAISES this, so
# the floor only has to survive the reverse: it would take losing half the
# guards to reach it, and that is not ordinary editing.
missing=""
n_bound=0
while IFS= read -r row; do
  [ -n "$row" ] || continue
  verdict="$(printf '%s' "$row" | cut -f3)"
  case "$verdict" in
    bound=*) ;;
    unbound|prose) continue ;;
    *)
      missing="$missing
    unknown verdict '$verdict' on: $(printf '%s' "$row" | cut -f1)"
      continue
      ;;
  esac
  n_bound=$((n_bound + 1))
  spec="${verdict#bound=}"
  guard_file="${spec%%#*}"
  needle="${spec#*#}"
  if [ ! -f "$guard_file" ]; then
    missing="$missing
    $guard_file is GONE (cited by $(printf '%s' "$row" | cut -f1))"
  elif ! grep -qF -- "$needle" "$guard_file"; then
    missing="$missing
    '$needle' is no longer in $guard_file"
  fi
done <<EOF
$LEDGER
EOF

if ! gate_arm bound_guards "$n_bound" 4; then
  fail "only $n_bound ledger row(s) claim a binding, under the floor of 4. Either
       guards were deleted, or the verdict parsing stopped matching."
elif [ -z "$missing" ]; then
  pass "all $n_bound named binding(s) are still present"
else
  fail "a comment cites a binding that is no longer there:$missing
       The claim is now unbound and the comment says otherwise, which is worse
       than never having had the guard. Restore it, or change the row to
       'unbound' and say so in the comment too."
fi

gate_arms_finish || FAIL=$((FAIL + 1))

echo "  sync claim gate: $PASS passed, $FAIL failed"
echo "  ($N_PAIRS cross-crate claim pair(s) enumerated, $LEDGER_ROWS ledger row(s),"
echo "   $n_bound of them claiming a named binding)"
[ "$FAIL" -eq 0 ] || exit 1
