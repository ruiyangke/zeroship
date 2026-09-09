#!/usr/bin/env bash
# Decision 5: the core and every other non-vendor crate NEVER embed a vendor.
#
# OPERATOR DECISION, 2026-09-01. Not "should avoid" - never. This gate exists
# because the mechanism the design relies on cannot enforce it, and because the
# crate split will otherwise LAUNDER the violations rather than surface them.
#
# WHY CARGO CANNOT DO THIS FOR US, which is the whole point:
#
#   The manifest fence (a crate that does not declare `compio-postgres` cannot
#   write `compio_postgres::Error`, E0433) stops the SPELLING of a vendor path.
#   It does not stop POSSESSION of a vendor VALUE arriving by public field or by
#   type inference - no token appears at the call site to be fenced. That is
#   exactly how `SchemaError.source: compio_postgres::Error` (#97) got in.
#
#   Worse for the split: `git mv`-ing a file that NAMES a vendor into a new crate
#   does not fail. Cargo simply wants the dependency declared, you declare it,
#   and the build goes GREEN having made the violation permanent and official.
#   Structural edges fail loudly; vendor embedding fails silently. So this gate
#   must run BEFORE each move, not after.
#
# WHAT IT RULES ON. Two arms, deliberately different questions:
#
#   1. NAMED   - a non-vendor production file writes `compio_postgres` or
#                `rusqlite`. Cheap, exact, and the only arm that can be complete.
#   2. BASELINE- every entry in the known-violation list below is still real. A
#                gate whose excuse list outlives the thing it excuses is how a
#                census goes stale, and this project has four recorded instances.
#
# The value-flow question - who HOLDS a vendor value without naming it - is
# genuinely harder and lives in `tests/lib/vendor_value_flow_census.sh`, which
# this gate deliberately does not duplicate. Run it too; it is a FLOOR, not a
# verdict.
#
# STRING LITERALS ARE BLANKED BEFORE MATCHING, since 2026-09-04. A vendor token
# inside a string is prose, not a type reference: `exec.rs:90` is a `hint:`
# message reading "The SQLite backend does not expose a compio_postgres::Pool...",
# and `crud/system_fields_pass.rs:399`/`:428` are error messages beginning
# "UPDATE patch attempted to overwrite...". Each line is stripped of `"..."`
# spans before the vendor pattern is applied.
#
# THIS PARAGRAPH USED TO SAY THE OPPOSITE - that counting them was an accepted
# limit, that "the per-file COUNTS here are an upper bound" but "the file LIST is
# exact, because every file named also has at least one real mention". The second
# half had stopped being true. Measured 2026-09-04 across all 78 files under
# ROOTS, blanking strings changes exactly ONE file's count, `exec.rs` 1 -> 0, and
# that was the whole of its production evidence: a baseline entry was being held
# alive by an error message, and arm 2 - whose entire job is to refuse an excuse
# that has outlived its defect - was printing "entry earns its place" about it.
# A caveat about counts turned out to be a caveat about the verdict.
#
# WHAT THE BLANKING STILL CANNOT DO. It is per-line and quote-counting, so a
# string spanning several lines, a raw string written `r#"..."#`, and an escaped
# `\"` inside a literal are all beyond it. Those need a lexer. Measured under
# ROOTS on 2026-09-04: 208 raw-string lines, 0 of them naming a vendor; 0 lines
# naming a vendor and carrying `\"`; and exactly ONE vendor-naming line with an
# odd quote count, `zeroship-plugin-db/service.rs:122`, which is a `///` doc
# comment and is dropped by the comment arm before the blanking is reached. So
# none of the three is live. If one lands this UNDERCOUNTS, which is the
# direction arm 1 must not have, so open the lines before trusting a zero.
#
# Run the detector's own positive/control pair: this script --self-test.

set -uo pipefail
cd "$(dirname "$0")/.."

# shellcheck source=tests/lib/gate_arms.sh
. "$(dirname "$0")/lib/gate_arms.sh"
# shellcheck source=tests/lib/module_gating.sh
. "$(dirname "$0")/lib/module_gating.sh"
gate_arms_init vendor_embedding

PASS=0
FAIL=0
ok()  { printf '  ok   %s\n' "$1"; PASS=$((PASS + 1)); }
bad() { printf '  FAIL %s\n' "$1"; FAIL=$((FAIL + 1)); }

VENDORS='compio_postgres|rusqlite'

# vendor_hits <file> <test-boundary>   -> count of production lines naming a vendor
# vendor_hit_lines <file> <boundary>   -> those lines, numbered, for the diagnosis
#
# ONE definition, called by arm 1, arm 2 and the self-test alike. It was four
# inline copies of the same awk until 2026-09-04, and the self-test's three were
# already a DIFFERENT predicate from the two arms' by the time the string
# blanking landed - controls proving a rule the gate does not apply. That is the
# shape tests/lib/module_gating.sh exists to end: of the four copies of the
# module-gating helper, three carried the bug and the fourth did not.
#
# A line is production when it is before the test boundary and is not a `//`
# comment; `"..."` spans are blanked first, so a vendor named in prose inside a
# string is not a type reference. See the header for what that blanking misses.
vendor_hits() {
  awk -v b="$2" -v pat="$VENDORS" '
    NR < b && $0 !~ /^[[:space:]]*\/\// {
      s = $0; gsub(/"[^"]*"/, "", s); if (s ~ pat) n++
    }
    END { print n+0 }' "$1"
}
vendor_hit_lines() {
  awk -v b="$2" -v pat="$VENDORS" '
    NR < b && $0 !~ /^[[:space:]]*\/\// {
      s = $0; gsub(/"[^"]*"/, "", s)
      if (s ~ pat) printf "       %d: %s\n", NR, $0
    }' "$1"
}

# ROOTS. Decision 5 says "the core and every OTHER non-vendor crate", and the
# first version of this gate scanned exactly one crate - which reported green
# while `zeroship-schema/src/error.rs:31` held `pub source: compio_postgres::Error`
# one crate BELOW the one being scanned. That is #97, and it is the floor the
# proposal places `data-core` on (#101), so a vendor in it is the violation at
# its most load-bearing point.
#
# Twenty workspace crates name a vendor. MOST OF THEM LEGITIMATELY DO - the
# migrate dialect backends are dialect implementations, and the services talk to
# their own driver. Scanning all twenty would report noise and train people to
# ignore this gate. The roots below are the crates the data-plane split requires
# to be vendor-free, and adding one is a design decision, not housekeeping.
#
# `zeroship-data-orm/src` JOINED THE ROOTS ON 2026-09-03, in the commit that
# created it. It had to: four of the six baseline entries below were files that
# left `zeroship-plugin-db/src` that day, and a root list pinned to the old tree
# would have left every one of them unscanned while their baseline keys went
# dead - arm 2 refuses a key it cannot resolve, so the gate would have failed
# loudly rather than quietly, but only after the four files had stopped being
# ruled on at all. It is also the one non-vendor crate that names BOTH vendors by
# design (`BackendHandle` is a closed sum over them), which is exactly why its
# entries are baselined rather than absent.
ROOTS="
crates/zeroship-plugin-db/src
crates/zeroship-data-orm/src
crates/zeroship-data-sql/src
"

# Files that are ALLOWED to name a vendor: the vendor tiers themselves, plus the
# CDC tier, which the target explicitly gives its own `compio-postgres`
# dependency (it is a service peer of the migration server, not a data-plane
# layer). Everything else in the crate is non-vendor and is ruled on.
#
# The KEY for a file is `<crate>/<path after src/>` - e.g.
# `zeroship-plugin-db/error.rs`. Qualifying by crate is not decoration: with two
# roots, `error.rs` names a file in BOTH, and an unqualified key would let one
# crate's baseline entry silently excuse the other's violation.
file_key() {
  local p="${1#crates/}"
  printf '%s/%s\n' "${p%%/src/*}" "${p#*/src/}"
}

is_vendor_tier() {
  case "$(file_key "$1")" in
    zeroship-data-orm/backend/postgres/*|zeroship-data-orm/backend/sqlite/*) return 0 ;;
    zeroship-plugin-db/backend/postgres.rs) return 0 ;;
    zeroship-plugin-db/backend/pg_*.rs|zeroship-plugin-db/backend/sqlite/*) return 0 ;;
    zeroship-plugin-db/replication.rs|zeroship-plugin-db/slot_reaper.rs) return 0 ;;
    zeroship-plugin-db/wal_consumer.rs|zeroship-plugin-db/change_stream_pg.rs) return 0 ;;
    *) return 1 ;;
  esac
}

# Map a baseline key back to a path, across roots.
key_to_path() {
  local crate="${1%%/*}" rest="${1#*/}"
  printf 'crates/%s/src/%s\n' "$crate" "$rest"
}

# --------------------------------------------------------------------------
# KNOWN VIOLATIONS, dated. This list may only SHRINK.
#
# Adding an entry is an operator decision and needs a reason on the line. Every
# entry is re-checked by arm 2: if the violation is gone, the gate FAILS and
# tells you to delete the line, so the list cannot quietly outlive the defect.
# --------------------------------------------------------------------------
# Measured at ea529977c: ELEVEN files, 21 production mentions. Decision 5's
# scope is an order of magnitude wider than #97 alone, which named exactly one
# public field. Every entry carries the task that DELETES it - an entry with no
# owner is a permanent exception, and this list does not have those.
#
# RE-MEASURED 2026-09-02 from this gate's own arm-2 output: SEVEN files, 11
# production mentions.
#
# The notes below CARRIED A PER-FILE COUNT COLUMN until 2026-09-02, and it had
# already rotted: `exec.rs` said 5 while the gate reported 1. The column is
# deleted rather than corrected. Arm 2 prints the live count for every entry on
# every run, so a number written here can only ever duplicate that output or
# contradict it - and a stale one reads exactly like a measured one. What a
# comment CAN say that the gate cannot is why the entry exists and what deletes
# it, so that is all these lines say now.
#
# RE-KEYED 2026-09-03: FOUR OF THE SIX CHANGED CRATE, NONE CHANGED SUBSTANCE.
# `exec.rs`, `backend/cancel.rs`, `auth/bootstrap.rs` and `tx_lanes.rs` are the
# ENGINE tier and left for `zeroship-data-orm`. This is the third occurrence
# of the "a move relocates an entry, it does not clear one" case the closing
# note below describes, and the first where four moved at once. `lib.rs` and
# `service.rs` are the adapter's and stayed.
BASELINE_FILES="
zeroship-data-orm/auth/bootstrap.rs
zeroship-plugin-db/lib.rs
zeroship-plugin-db/service.rs
"
# exec.rs                 ENTRY RETIRED 2026-09-04, and it had already been dead
#                         for a day. It read "Pool + Vec<Row> - the unsettled row
#                         vocabulary, blocked on the neutral-row decision", and
#                         the 2026-09-03 engine cut moved every driver-facing
#                         line out. What kept the entry alive was this gate's own
#                         string-literal blind spot: the ONLY production hit left
#                         was the hint message at exec.rs:90, so arm 2 printed
#                         "still violates (1 occurrence)" about a file that no
#                         longer does. Measured 2026-09-04: nothing before the
#                         test boundary names `Pool` or `Row` as a type; the four
#                         surviving mentions are comments and that one message.
#                         The value-flow question - who HOLDS a vendor value
#                         without naming it - is a different gate's, as the
#                         header says; this list is about naming.
# backend/mod.rs          ENTRY RETIRED 2026-09-02, the second way described
#                         below and exactly as predicted: `zeroship-data-postgres`
#                         now exists, the six PostgreSQL files and the two PG
#                         extension traits moved into it, and what is left in
#                         backend/mod.rs names no driver at all. `BackendHandle`
#                         still names both backends, but through the re-exported
#                         `PostgresBackend` / `SqliteBackend` types rather than
#                         `compio_postgres::` directly. No code was written to
#                         clear this - the crate boundary cleared it.
# backend/cancel.rs       CancelToken, Pool. Was transaction/cancel.rs; moved into
#                         the vendor tier by #122. See the note below on why a
#                         move does not clear an entry, only relocates it.
# auth/bootstrap.rs       session setup reaching the driver. Follows #110's cut.
# context.rs              ENTRY DELETED 2026-09-02. It held `pool: Option<Rc<Pool>>`
#                         plus `set_pool(Rc<Pool>)`. The field was redundant with
#                         `backend` - the same Rc, stored twice - and the setter
#                         became `set_postgres_backend`, with the connect moved
#                         into `PostgresBackend::connect`. #166. The file now
#                         names no vendor at all, which is what retired it.
# drop_namespace.rs       ENTRY DELETED 2026-09-02, and NOT because the code
# backend/lock_guard.rs   changed. Each module is gated at its single
#                         declaration - lib.rs:259 and backend/mod.rs:82 - so
#                         neither is in a shipped binary, and neither was ever a
#                         file this gate should have ruled on.
#                         `module_is_test_gated` skips them in arm 1 now, which
#                         is what makes the entries wrong: an entry arm 1 cannot
#                         reach can never be RETIRED by fixing the file, only
#                         held green forever by arm 2. Both files still contain
#                         `use compio_postgres::...` - do not read either
#                         deletion as the coupling having gone. Arm 2 now
#                         refuses such an entry outright, so this pairing cannot
#                         be reintroduced by hand.
# lib.rs                  the thin adapter still links the driver. #109.
# service.rs              unjudged file; no destination decided yet.
#
# **THE ENTRIES RETIRE TWO DIFFERENT WAYS, and conflating them reads the list
# as more alarming than it is.** Three came off on 2026-09-02 the first way:
# transaction/driver.rs, transaction/mod.rs and transaction/cancel.rs, when #122
# separated the SC-1 protocol from the vendor lane. driver.rs and mod.rs stopped
# naming a vendor at all; cancel.rs MOVED, so its entry did not disappear, it
# became backend/cancel.rs above.
#
# The rest come off the second way: by the crate split itself. That prediction
# has now been TESTED once and held: on 2026-09-02 `backend/` became
# zeroship-data-postgres, and backend/mod.rs left this list without a line of
# code being written to make it. backend/cancel.rs is the same case still
# pending - it is in the tier ALLOWED to name a driver and is listed only
# because it has not moved yet. Do not chase it as a defect.

in_baseline() {
  printf '%s\n' "$BASELINE_FILES" | grep -qx -- "$1"
}

# A module can be gated where it is DECLARED rather than where it is defined,
# and the production-region filter below cannot see that: the attribute is in
# the PARENT file.
#
#     // lib.rs:259
#     #[cfg(any(test, feature = "test-helpers"))]
#     pub mod drop_namespace;
#
# `drop_namespace.rs` carries no cfg of its own, so this gate counted its
# `use compio_postgres::Pool` and carried a baseline entry for a module that is
# in no shipped binary.
#
# `module_is_test_gated` LIVED HERE, AS ONE OF FOUR HAND-COPIES, UNTIL
# 2026-09-04. This one and the two censuses shared a defect the fourth copy
# (tests/decision_four_gate.sh) had already fixed: the walk's `#[cfg(` arm
# matched ANYWHERE in the line and was tested BEFORE the comment arm, so a
# COMMENT that merely QUOTED `#[cfg(test)]` above a `mod x;` declaration read as
# a gate. It cost this gate two files - `zeroship-data-orm/backend/mod.rs`
# and `crud/unmask.rs`, each disabled by prose written to explain a visibility
# decision. Neither names a vendor, so nothing was concealed; the two gates
# simply disagreed about which files exist to rule on, which is the shape a
# census fails in. The one definition now lives in tests/lib/module_gating.sh
# with its own positive/negative controls, and `--self-test` below runs them.
#
# The roots are passed EXPLICITLY. The four copies each hard-coded a different
# search root, so a shared helper that picked one would silently change what the
# other three rule on.

# --------------------------------------------------------------------------
# Arm 1: no non-vendor production file names a vendor.
# --------------------------------------------------------------------------
echo "== non-vendor files must not name a vendor =="

n_ruled=0
n_new=0
while IFS= read -r f; do
  is_vendor_tier "$f" && continue
  # A module that is not compiled into any shipped binary cannot embed a vendor
  # into one. Skipped BEFORE n_ruled so the arm's count stays honest: it reports
  # what it decided, and it decided nothing about this file.
  module_is_test_gated "$f" $ROOTS && continue
  rel="$(file_key "$f")"
  n_ruled=$((n_ruled + 1))

  # Production region only: everything before the file's first column-0
  # #[cfg(test)]. A vendor named inside a test module is not an embedding.
  # The production region ends at the file's test MODULE, not at its first
  # item-level `#[cfg(test)]`. Taking the first cfg of ANY kind is silently
  # defeated by one innocuous line: a `#[cfg(test)] use crate::x;` near the top
  # hides every vendor mention below it.
  #
  # Measured 2026-09-01 on exec.rs: two lines at the top took its five mentions
  # to zero and this arm went from naming them to printing `ok`. Only the
  # baseline-liveness arm noticed, and only because that file is IN the
  # baseline - a file outside it would be blinded in total silence. Every crate
  # move adds exactly such imports, so this had to be fixed before the moves.
  boundary=$(awk '
    /^#\[cfg\(test\)\]/ { candidate = NR; next }
    candidate && NF {
      if ($0 ~ /^(pub )?mod /) { print candidate; exit }
      candidate = 0
    }
  ' "$f")
  [ -n "$boundary" ] || boundary=999999
  hits=$(vendor_hits "$f" "$boundary")
  [ "$hits" -eq 0 ] && continue

  if in_baseline "$rel"; then
    printf '  note %s names a vendor %d time(s) - KNOWN, in baseline\n' "$rel" "$hits"
    continue
  fi
  n_new=$((n_new + 1))
  bad "$rel names a vendor $hits time(s) in production and is NOT in the baseline"
  vendor_hit_lines "$f" "$boundary" | head -5
done < <(find $ROOTS -name '*.rs' 2>/dev/null | LC_ALL=C sort)

[ "$n_new" -eq 0 ] && ok "no new vendor embedding across $n_ruled non-vendor file(s)"

# FLOOR. 60+ non-vendor files today. Set well under it: deleting a few modules
# must not trip this, but a change that stops the walk finding files - a moved
# SRC root, a broken find - must.
if ! gate_arm named_vendor "$n_ruled" 25; then
  FAIL=$((FAIL + 1))
fi

# --------------------------------------------------------------------------
# Arm 2: every baseline entry is still a real violation.
# --------------------------------------------------------------------------
echo
echo "== baseline entries must still describe live violations =="

n_baseline=0
while IFS= read -r rel; do
  [ -n "$rel" ] || continue
  n_baseline=$((n_baseline + 1))
  f="$(key_to_path "$rel")"
  if [ ! -f "$f" ]; then
    bad "baseline names $rel, which does not exist - delete the entry"
    continue
  fi
  # An entry arm 1 SKIPS is unreachable, not satisfied. Arm 1 declines to rule
  # on a test-gated module, so nothing about that file can ever retire its
  # entry; the check below would hold it green off the file's own text forever,
  # and the two arms would disagree about which files exist to rule on. This is
  # not hypothetical - lock_guard.rs and drop_namespace.rs were both in that
  # state on 2026-09-02, and the gate was GREEN throughout, because each arm was
  # individually self-consistent. Keep the arms agreeing by construction.
  if module_is_test_gated "$f" $ROOTS; then
    bad "baseline names $rel, but it is test-gated at its declaration so arm 1 never rules on it - delete the entry"
    continue
  fi
  # The production region ends at the file's test MODULE, not at its first
  # item-level `#[cfg(test)]`. Taking the first cfg of ANY kind is silently
  # defeated by one innocuous line: a `#[cfg(test)] use crate::x;` near the top
  # hides every vendor mention below it.
  #
  # Measured 2026-09-01 on exec.rs: two lines at the top took its five mentions
  # to zero and this arm went from naming them to printing `ok`. Only the
  # baseline-liveness arm noticed, and only because that file is IN the
  # baseline - a file outside it would be blinded in total silence. Every crate
  # move adds exactly such imports, so this had to be fixed before the moves.
  boundary=$(awk '
    /^#\[cfg\(test\)\]/ { candidate = NR; next }
    candidate && NF {
      if ($0 ~ /^(pub )?mod /) { print candidate; exit }
      candidate = 0
    }
  ' "$f")
  [ -n "$boundary" ] || boundary=999999
  hits=$(vendor_hits "$f" "$boundary")
  if [ "$hits" -eq 0 ]; then
    bad "baseline still excuses $rel, but it names no vendor any more - DELETE the entry"
  else
    ok "$rel still violates ($hits occurrence(s)) - entry earns its place"
  fi
done <<EOF
$(printf '%s\n' "$BASELINE_FILES" | grep -v '^[[:space:]]*$')
EOF

if ! gate_arm baseline_liveness "$n_baseline" 1; then
  FAIL=$((FAIL + 1))
fi

# --------------------------------------------------------------------------
# Self-test: prove the detector fires, and prove it does not fire on a control.
# --------------------------------------------------------------------------
if [ "${1:-}" = "--self-test" ]; then
  echo
  echo "== self-test =="
  probe=$(mktemp -d)
  trap 'rm -rf "$probe"' EXIT

  # POSITIVE: a non-vendor-shaped file naming a vendor in production.
  printf 'fn f(e: &compio_postgres::Error) {}\n' > "$probe/positive.rs"
  h=$(vendor_hits "$probe/positive.rs" 999999)
  if [ "$h" -ge 1 ]; then ok "positive control: detector sees a production vendor mention"
  else bad "positive control FAILED: detector blind to an obvious mention"; fi

  # NEGATIVE: same token, but inside a test region and behind a comment. One
  # variable changed at a time is the whole point of a control.
  printf '#[cfg(test)]\nmod t { fn f(e: &compio_postgres::Error) {} }\n' > "$probe/negative.rs"
  nb=$(grep -n '^#\[cfg(test)\]' "$probe/negative.rs" | head -1 | cut -d: -f1)
  nh=$(vendor_hits "$probe/negative.rs" "$nb")
  if [ "$nh" -eq 0 ]; then ok "negative control: a mention inside a test region is not an embedding"
  else bad "negative control FAILED: test-region mention counted as production"; fi

  printf '// compio_postgres::Error in a comment\n' > "$probe/comment.rs"
  ch=$(vendor_hits "$probe/comment.rs" 999999)
  if [ "$ch" -eq 0 ]; then ok "negative control: a mention in a comment is not an embedding"
  else bad "negative control FAILED: comment counted as production"; fi

  # THE STRING-LITERAL PAIR. Both lines are production code, neither is a
  # comment, and they differ in ONE variable: whether the vendor token is inside
  # a `"..."` span. A blanking that ate the whole line would pass the negative
  # and fail the positive, so the two have to be read together.
  printf 'fn f() { panic!("no compio_postgres::Pool here"); }\n' > "$probe/instring.rs"
  sh=$(vendor_hits "$probe/instring.rs" 999999)
  if [ "$sh" -eq 0 ]; then ok "negative control: a vendor named inside a string is prose, not a type"
  else bad "negative control FAILED: a string literal is still counted as an embedding"; fi

  printf 'fn f() -> compio_postgres::Pool { panic!("unrelated message"); }\n' > "$probe/besidestring.rs"
  bh=$(vendor_hits "$probe/besidestring.rs" 999999)
  if [ "$bh" -ge 1 ]; then ok "positive control: a real type survives a string on the same line"
  else bad "positive control FAILED: blanking ate code outside the string span"; fi

  # The SKIP predicate has its own controls, because a wrong answer here is
  # invisible: a file this gate declines to rule on prints nothing at all.
  # tests/decision_four_gate.sh runs the same set over the same helper.
  if module_gating_self_test; then
    ok "module-gating controls: the shared skip predicate discriminates"
  else
    bad "module-gating controls FAILED - arm 1's skip set is wrong, so its verdict says nothing"
  fi
fi

echo
gate_arms_finish || FAIL=$((FAIL + 1))

echo
if [ "$FAIL" -gt 0 ]; then
  printf 'vendor_embedding_gate: %d passed, %d FAILED\n' "$PASS" "$FAIL"
  exit 1
fi
printf 'vendor_embedding_gate: %d passed\n' "$PASS"
