#!/usr/bin/env bash
# ============================================================================
# DECISION 4: NO SQL AND NO DIALECT KNOWLEDGE IN THE ENGINE.
#
# `docs/proposals/2026-08-31-data-crate-shape.md` decision 4 reads: "Adding a
# database must require a new crate, a new variant in the three dispatch enums
# (`BackendHandle`, `TxConnection`, `TxCanceller`), and nothing else: no
# statement text, no `match` on dialect to choose SQL, no vendor error
# handling."
#
# OPERATOR DECISION, 2026-09-04, ratifying options (a) + (c) from that
# document's Open list: keep the property as written - no statement text in the
# engine - and MECHANISE it. This is (c).
#
# WHY IT NEEDED A GATE. Decision 5 is enforced by `tests/vendor_embedding_gate.sh`
# and decision 4 was enforced by nothing, and the asymmetry showed. The vendor
# gate rules on vendor TYPE NAMES; its own header records SQL-shaped string
# literals as a false-POSITIVE source, which is the opposite direction from what
# decision 4 needs. So decision 4's inventory was a reading, re-derived by hand
# at least twice, and it was wrong in BOTH directions when this gate first
# measured it:
#
#   * it billed `crates/zeroship-data-engine/src/auth/bootstrap.rs` as "eleven
#     production statements ... in an always-compiled module", naming `pg_roles`,
#     `CREATE ROLE`, `SET LOCAL ROLE`, `GRANT ... ON ALL SEQUENCES`,
#     `ALTER DEFAULT PRIVILEGES` and `REVOKE`. THE MODULE IS ALWAYS COMPILED; FIVE
#     OF THE SIX STATEMENTS ARE NOT. `pg_roles` (:85), `CREATE ROLE` (:92),
#     `SET LOCAL ROLE` (:140), `GRANT ... ON ALL SEQUENCES` (:278) and
#     `ALTER DEFAULT PRIVILEGES` (:289) each sit inside an item carrying
#     `#[cfg(any(test, feature = "test-helpers"))]`, and `test-helpers` is
#     reached only through a `[dev-dependencies]` edge
#     (`crates/zeroship-plugin-db/Cargo.toml:135`, against the plain
#     `[dependencies]` entry at `:43`), so none of them is in a shipped binary.
#   * the sixth, `REVOKE`, is real - but only in the form the body did NOT
#     describe. `revoke_reserved_system_table_privileges_sql` (:308),
#     `revoke_worker_unmask_audit_privileges_sql` (:347) and
#     `grant_worker_unmask_audit_append_privileges_sql` (:407) are BUILDERS
#     rendering PL/pgSQL `DO $$` blocks over `pg_class` / `pg_namespace`. They
#     carry no cfg at all, and they are 23 of the 37 sites this gate counts.
#   * it billed `backend_handle.rs` at six statements. There are seven: the
#     `"BEGIN"` its SQLite session-open arm issues was not counted.
#
# A gate is the answer to that, not another reading.
#
# WHAT IT RULES ON. Three arms, three different questions:
#
#   1. production_files - how many files the production-region extractor could
#      read at all. The anti-vacuity anchor: this number can never legitimately
#      collapse while the crate exists.
#   2. statement_sites - every production SQL site found is on the BASELINE
#      below, and the totals have not risen past their ceilings.
#   3. baseline_rows   - every baseline row still describes a live site. A gate
#      whose excuse list outlives the thing it excuses is how a census goes
#      stale, and this repository has four recorded instances.
#
# A SHRINK-ONLY BASELINE, NOT A FREEZE. The residue is known debt with named
# owners (proposal Opens 5 and 7). Additions are refused; REMOVALS ARE ACCEPTED
# and print a note asking for the ceiling to be lowered. A gate that pinned
# today's count would go red the moment somebody did the work it exists to
# encourage, and would then be relaxed until it meant nothing.
#
# THE CLASSES, and they are the reason this gate can be argued with and survive.
# `SAVEPOINT` is ANSI and costs a new backend nothing; the `pg_class` catalog
# query costs it everything. Treating them identically is how a gate gets
# weakened. Every baseline row carries one of:
#
#   vendor    PostgreSQL-only by construction. A new backend cannot use a word
#             of it. This is what decision 4's heading is actually about.
#   per-arm   the text is ordinary ANSI, but it lives inside a `match self`
#             backend arm, so a new backend must write its own copy. The "three
#             logical operations written twice" residue.
#   shared    dialect-neutral AND issued once for every backend, through
#             `TxConnection::exec`. A new backend inherits it and writes
#             nothing. Compliant under option (b), still debt under the ratified
#             (a).
#   prose     the detector matched and a human read it: an error MESSAGE that
#             happens to open with a SQL word. Not statement text. Recorded
#             rather than filtered, so the next reader inherits the finding.
#
# Two ceilings follow from that: the total, and `vendor + per-arm` - the surface
# a new backend has to pay for. The second is the sharper number and the one to
# watch.
#
# WHAT THIS GATE CANNOT DO, stated so nobody reads it as complete:
#
#   - IT CANNOT TELL WHETHER A STRING IS REALLY SQL. It matches a SQL verb at
#     the start of a string literal, a nested `'...'` literal, or a continuation
#     line. `crud/system_fields_pass.rs`'s "UPDATE patch attempted to overwrite
#     ..." is an error message and is baselined as `prose` for exactly that
#     reason. There will be more of those, and there will be false NEGATIVES
#     too: SQL assembled from fragments, a verb that is not in VERBS, a
#     statement built by `push_str` a word at a time, or any statement whose
#     first word arrives from a variable. A clean run is not proof the engine
#     holds no SQL.
#   - It cannot tell dialect-neutral from vendor-specific BY ITSELF. The class
#     on each baseline row is a judgement a human made by reading, exactly like
#     `tests/sync_claim_gate.sh`'s `prose` verdict, and a rewrite could quietly
#     turn one class into another. Re-read a row before trusting it.
#   - The production-region extractor is a TEXT extractor, not a compiler. It
#     understands a `#[cfg(...)]` written on one line above an item whose body
#     closes at the attribute's own indentation - which is what rustfmt emits -
#     and it understands a module gated at its `mod x;` declaration. It does not
#     evaluate cfg algebra: any `not(` makes it treat the arm as shipped, which
#     is deliberate (`"test-helpers"` CONTAINS `test`, and matching that naively
#     is how a sibling census went to zero rows while printing that as calmly as
#     a real number).
#   - It rules on `crates/zeroship-data-engine/src` and nothing else. The
#     adapter, the vendor tiers and the migration engine are all SUPPOSED to
#     hold statement text and are not scanned.
#
# Run the detector's own positive/control set: this script --self-test.
# Point it at a copied tree to watch it fail: this script --root <dir>.
# ============================================================================
set -uo pipefail
cd "$(dirname "$0")/.."

# shellcheck source=tests/lib/gate_arms.sh
. "$(dirname "$0")/lib/gate_arms.sh"
# shellcheck source=tests/lib/module_gating.sh
. "$(dirname "$0")/lib/module_gating.sh"
gate_arms_init decision_four

PASS=0
FAIL=0
pass() { PASS=$((PASS + 1)); echo "  ok   $1"; }
fail() { FAIL=$((FAIL + 1)); echo "  FAIL $1"; }

# The root is an ARGUMENT, never an environment variable: a gate whose target
# depends on how the process was launched cannot be reasoned about from its
# invocation (operator rule, 2026-08-20). `--root` exists so the detector can be
# driven over a COPY of the tree - which is how the RED demonstration is done
# without editing `crates/`.
ROOT="crates/zeroship-data-engine/src"
SELF_TEST=0
while [ "$#" -gt 0 ]; do
  case "$1" in
    --root)
      shift
      [ "$#" -ge 1 ] || { echo "  x REFUSED: --root needs a directory" >&2; exit 1; }
      ROOT="$1"; shift ;;
    --self-test) SELF_TEST=1; shift ;;
    *) echo "usage: $0 [--root <dir>] [--self-test]" >&2; exit 1 ;;
  esac
done

# ---------------------------------------------------------------------------
# THE VERB VOCABULARY, ordered LONGEST FIRST because the matcher takes the first
# verb whose text is a prefix of the candidate. `ROLLBACK` is a prefix of
# `ROLLBACK TO SAVEPOINT`, and the wrong order silently re-keys the savepoint
# family onto the bare rollback - which would merge two baseline rows into one
# and let a new statement inherit an existing row's excuse.
# ---------------------------------------------------------------------------
VERBS='ROLLBACK TO SAVEPOINT|RELEASE SAVEPOINT|INSERT INTO|DELETE FROM|SAVEPOINT|SET LOCAL|TRUNCATE|ROLLBACK|SET ROLE|EXECUTE|RELEASE|REVOKE|SELECT|UPDATE|CREATE|COMMIT|VALUES|DO $$|BEGIN|GRANT|ALTER|DROP'

# ---------------------------------------------------------------------------
# THE EXTRACTOR. One awk pass per file: decide the production region, then find
# statement sites inside it.
#
# NO `2>/dev/null` ON ANYTHING THAT FEEDS A COUNT anywhere below. A command that
# failed and a command that found nothing print the same number, and the zero
# branch here is "no new SQL", i.e. a PASS.
# ---------------------------------------------------------------------------
AWK_SITES=$(cat <<'AWK'
BEGIN { nv = split(VERBLIST, V, "|"); SEP = "\001" }
{ L[NR] = $0 }
END {
  # --- production region -----------------------------------------------
  # An item carrying a test-ish cfg is not in a shipped binary, so nothing in
  # it is production statement text. The attribute's own indentation bounds the
  # item: rustfmt closes it with `}` at that column.
  skip = 0; skipind = ""; pend = 0; pendind = ""
  for (i = 1; i <= NR; i++) {
    line = L[i]
    if (skip) {
      if (line ~ ("^" skipind "\\}[;,]?[ \t]*$")) skip = 0
      continue
    }
    if (pend) {
      # attributes, doc comments and blanks still belong to the gated item
      if (line ~ /^[ \t]*#\[/ || line ~ /^[ \t]*\/\// || line ~ /^[ \t]*$/) continue
      s = line; sub(/[ \t]+$/, "", s)
      pend = 0
      if (s ~ /;$/ && s !~ /\{/) continue        # a one-line item, e.g. `mod x;`
      skip = 1; skipind = pendind
      continue
    }
    if (line ~ /^[ \t]*#\[cfg\(/) {
      cfg = line
      ind = line; sub(/[^ \t].*$/, "", ind)
      sub(/^[ \t]*#\[cfg\(/, "", cfg)
      sub(/\)\][ \t]*$/, "", cfg)
      # A `not(...)` wrapper is the SHIPPED arm and must be kept. Testing for
      # `test` before stripping it is how a sibling census took itself to zero
      # rows: `"test-helpers"` contains `test`, so `#[cfg(not(feature =
      # "test-helpers"))]` reads as test-gated unless `not(` is handled first.
      if (cfg !~ /not\(/ &&
          (cfg ~ /(^|[^A-Za-z_-])test([^A-Za-z_-]|$)/ ||
           cfg ~ /test-helpers/ || cfg ~ /live-db-tests/)) {
        pend = 1; pendind = ind
        continue
      }
    }
    prod[i] = 1
  }

  # --- statement sites --------------------------------------------------
  for (i = 1; i <= NR; i++) {
    if (!(i in prod)) continue
    line = L[i]
    t = line
    sub(/^[ \t]+/, "", t)
    if (t ~ /^\/\//) continue                    # a comment is not a statement
    # Mark every plausible START OF STATEMENT TEXT: the beginning of the line
    # (a `\`-continuation inside a multi-line literal), and any position just
    # after a quote, an open paren or a comma. A verb anywhere ELSE is prose:
    # `DbError::internal("db.transaction: SAVEPOINT did not open a frame")` is
    # a message, and it must not be counted as a statement.
    t = SEP t
    gsub(/["'(,][ \t]*/, "&" SEP, t)
    n = split(t, P, SEP)
    seen = SEP
    for (j = 2; j <= n; j++) {
      for (k = 1; k <= nv; k++) {
        v = V[k]
        if (substr(P[j], 1, length(v)) != v) continue
        nxt = substr(P[j], length(v) + 1, 1)
        if (nxt ~ /[A-Za-z0-9_]/) continue        # SELECT_LIMIT is an identifier
        if (index(seen, SEP v SEP)) break         # one site per verb per line
        seen = seen v SEP
        print i "\t" v
        break
      }
    }
  }
}
AWK
)

# A module can be gated where it is DECLARED rather than where it is defined -
# `#[cfg(test)] mod tests;` in the parent - and the in-file extractor above
# cannot see that, because the attribute is in another file.
#
# THE CORRECTED WALK THAT LIVED HERE MOVED TO tests/lib/module_gating.sh ON
# 2026-09-04, and three other copies were folded onto it. This gate's version
# was the only one of four that stopped at a plain `//` comment; the other three
# tested a `*"#[cfg("*` substring BEFORE the comment arm, so a COMMENT that
# quoted `#[cfg(test)]` above a `mod x;` declaration read as a gate and removed
# the module from the scan. The two gates therefore DISAGREED by two files about
# which files exist to rule on - the shape a census fails in. The shared helper
# keeps the correction and adds the controls that pin it; `--self-test` runs
# them, and so does tests/vendor_embedding_gate.sh --self-test.
#
# The residual false NEGATIVE is `#[cfg(test)]`, then a plain comment, then
# `mod x;`. That reads as production and gets scanned, which surfaces as a noisy
# refusal rather than as silence - the safe direction for a gate.

# production_files <root> - every .rs file whose production region this gate
# rules on, one per line. A module gated at its declaration is skipped BEFORE it
# is counted, so the arm's number reports what it decided and not what it saw.
#
# The root is passed THROUGH to the skip predicate rather than read from the
# global `$ROOT`, so `--root <copy>` drives one tree end to end.
production_files() {
  local f
  while IFS= read -r f; do
    module_is_test_gated "$f" "$1" && continue
    printf '%s\n' "$f"
  done < <(find "$1" -name '*.rs' -type f | LC_ALL=C sort)
}

# sql_sites <root> - `<key><TAB><line><TAB><verb>` rows, where <key> is the path
# under the root. Keying by the path under the root and not by the absolute path
# is what lets --root drive the same baseline over a copied tree.
sql_sites() {
  local root="$1" f rel
  while IFS= read -r f; do
    rel="${f#"$root"/}"
    awk -v VERBLIST="$VERBS" "$AWK_SITES" "$f" | sed "s|^|$rel\t|"
  done < <(production_files "$root")
}

# ---------------------------------------------------------------------------
# THE BASELINE. One row per (file, verb) that production code holds today:
#
#     <path under root>  <VERB>  <class>  <owner>
#
# Measured 2026-09-04 by this gate's own arm 2 output: 37 sites across 5 files,
# 15 rows. Taken at cc84ad6f9 and re-taken unchanged at 3e1be07df, across the
# three commits that reworked the masked raw-column derivation in two of the
# five files. THIS LIST MAY ONLY SHRINK. Adding a row is an
# operator decision and needs the reason on the line; arm 3 re-checks every row
# against the tree, so a row cannot outlive the statement it excuses.
#
# THERE IS DELIBERATELY NO PER-ROW COUNT COLUMN. `tests/vendor_embedding_gate.sh`
# carried one and it had already rotted when it was deleted - one file said 5
# while the gate reported 1 - because a number written in a comment reads
# exactly like a measured one. The ceilings below are the only numbers here, and
# both are printed live on every run.
#
# OWNERS come from the proposal's Open list. A row with `unowned` is residue
# nobody has scheduled, which is a different and worse thing than debt with a
# task against it; that is why the column exists.
# ---------------------------------------------------------------------------
BASELINE='
auth/bootstrap.rs	DO $$	vendor	Open 7 (delete auth/)
auth/bootstrap.rs	BEGIN	vendor	Open 7 (delete auth/)
auth/bootstrap.rs	SELECT	vendor	Open 7 (delete auth/)
auth/bootstrap.rs	EXECUTE	vendor	Open 7 (delete auth/)
auth/bootstrap.rs	REVOKE	vendor	Open 7 (delete auth/)
auth/bootstrap.rs	GRANT	vendor	Open 7 (delete auth/)
backend_handle.rs	SELECT	per-arm	Open 5 (render/sqlite.rs)
backend_handle.rs	INSERT INTO	per-arm	Open 5 (render/sqlite.rs)
backend_handle.rs	VALUES	per-arm	Open 5 (render/sqlite.rs)
backend_handle.rs	BEGIN	per-arm	unowned
tx_lanes.rs	ROLLBACK	per-arm	unowned
transaction/driver.rs	SAVEPOINT	shared	unowned
transaction/driver.rs	ROLLBACK TO SAVEPOINT	shared	unowned
transaction/driver.rs	RELEASE SAVEPOINT	shared	unowned
crud/system_fields_pass.rs	UPDATE	prose	none - false positive
'
# auth/bootstrap.rs   The three `_sql` BUILDERS, and only those: everything else
#                     in this file that issues SQL is behind
#                     `#[cfg(any(test, feature = "test-helpers"))]` and is in no
#                     shipped binary. `revoke_reserved_system_table_privileges_sql`,
#                     `revoke_worker_unmask_audit_privileges_sql` and
#                     `grant_worker_unmask_audit_append_privileges_sql` render
#                     PL/pgSQL `DO $$` blocks over `pg_class` / `pg_namespace`.
#                     There is no portable spelling of any of it. Open 7 deletes
#                     the module rather than porting it.
# backend_handle.rs   The three logical operations decision 4's body names -
#                     `read_raw_column_bytes`, `read_raw_column_text`,
#                     `append_unmask_audit` - each written twice, once per arm,
#                     plus the SQLite `"BEGIN"` in `open_dedicated_session` that
#                     the body's count of six omitted. Open 5 moves the SQLite
#                     halves behind a `render/sqlite.rs` seam.
# tx_lanes.rs         `destroy_tx_connection`'s SQLite arm sends a detached
#                     `ROLLBACK` when SC-1 withdraws a session. Same shape as
#                     the rows above - ANSI text inside a backend arm - and no
#                     Open covers it.
# transaction/driver.rs
#                     The `IssueSavepoint` family, issued through
#                     `exec_on_session` -> `TxConnection::exec`, which is one
#                     text for every backend. This is the row the class column
#                     exists for: under decision 4 as ratified it is still
#                     statement text in the engine, and under option (b) it
#                     would have been compliant as written. Do not chase it as
#                     though it were the `pg_class` query.
# crud/system_fields_pass.rs
#                     NOT SQL. `"UPDATE patch attempted to overwrite immutable
#                     system field ..."` is the refusal text of the system-field
#                     guard. Baselined rather than filtered so the judgement is
#                     recorded; if this line is ever reworded so it no longer
#                     opens with a verb, arm 3 will say so and the row goes.

# THE CEILINGS. Both measured 2026-09-04 at 3e1be07df.
#
# These are ONE-DIRECTIONAL. Exceeding one is a refusal; coming in under one is
# a PASS with a note asking for the number to be lowered in the same commit that
# did the work. That asymmetry is the whole design: the residue is debt with
# owners, and a gate that failed when the debt shrank would be relaxed within a
# week.
SITE_CEILING=37
# `vendor + per-arm`: the surface a NEW BACKEND has to pay for. `shared` and
# `prose` are excluded on purpose - the savepoint family costs a new backend
# nothing, and the system-fields message is not SQL. This is the sharper of the
# two numbers and the one to watch.
BACKEND_COST_CEILING=33

# --------------------------------------------------------------------------
# --self-test: does the detector detect, and does it DISCRIMINATE?
# --------------------------------------------------------------------------
self_test() {
  echo "decision four gate self-test"
  local tmp status=0 got saved_root
  tmp="$(mktemp -d)"
  saved_root="$ROOT"
  trap 'rm -rf "$tmp"; ROOT="$saved_root"' RETURN
  mkdir -p "$tmp/src"
  ROOT="$tmp/src"

  # POSITIVE: a plain production statement.
  cat > "$tmp/src/lib.rs" <<'RS'
pub fn q(app: &str) -> String {
    format!("SELECT id FROM \"{app}\".\"users\" WHERE id = $1")
}
RS
  got="$(sql_sites "$tmp/src")"
  if [ "$got" = "$(printf 'lib.rs\t2\tSELECT')" ]; then
    echo "  ok   a production statement literal is extracted"
  else
    echo "  FAIL the extraction found [$got]; the detector sees nothing"
    status=1
  fi

  # NEGATIVE CONTROL, one variable: the SAME text, inside a test module. Without
  # this the positive proves only that the regex ran - a detector that ignored
  # cfg entirely would pass the positive too and bury the baseline in test SQL.
  cat > "$tmp/src/lib.rs" <<'RS'
#[cfg(test)]
mod tests {
    fn q(app: &str) -> String {
        format!("SELECT id FROM \"{app}\".\"users\" WHERE id = $1")
    }
}
RS
  got="$(sql_sites "$tmp/src")"
  if [ -z "$got" ]; then
    echo "  ok   the same statement inside #[cfg(test)] is not production"
  else
    echo "  FAIL a test-module statement was counted as production: [$got]"
    status=1
  fi

  # The `test-helpers` half, which is a DIFFERENT gate from cfg(test) and is
  # where every statement in auth/bootstrap.rs actually lives.
  cat > "$tmp/src/lib.rs" <<'RS'
#[cfg(any(test, feature = "test-helpers"))]
pub fn q(app: &str) -> String {
    format!("SELECT id FROM \"{app}\".\"users\" WHERE id = $1")
}
RS
  got="$(sql_sites "$tmp/src")"
  if [ -z "$got" ]; then
    echo "  ok   a statement behind feature = \"test-helpers\" is not production"
  else
    echo "  FAIL a test-helpers statement was counted as production: [$got]"
    status=1
  fi

  # THE REVERSE CONTROL, and it is the one that has bitten a sibling census:
  # `"test-helpers"` CONTAINS `test`, so a naive matcher treats the SHIPPED arm
  # of the visibility ladder as test-gated and goes blind to the whole crate.
  cat > "$tmp/src/lib.rs" <<'RS'
#[cfg(not(feature = "test-helpers"))]
pub fn q(app: &str) -> String {
    format!("SELECT id FROM \"{app}\".\"users\" WHERE id = $1")
}
RS
  got="$(sql_sites "$tmp/src")"
  if [ "$got" = "$(printf 'lib.rs\t3\tSELECT')" ]; then
    echo "  ok   a #[cfg(not(feature = \"test-helpers\"))] item IS production"
  else
    echo "  FAIL the shipped arm of a cfg ladder was dropped: [$got]"
    status=1
  fi

  # A verb in the MIDDLE of a message is prose, not a statement. Without this
  # the detector would report every error string mentioning a SQL word, the
  # baseline would fill with noise, and the gate would be ignored.
  cat > "$tmp/src/lib.rs" <<'RS'
pub fn e() -> String {
    String::from("db.transaction: SAVEPOINT did not open a frame")
}
RS
  got="$(sql_sites "$tmp/src")"
  if [ -z "$got" ]; then
    echo "  ok   a verb mid-message is not a statement site"
  else
    echo "  FAIL a prose mention was counted as a statement: [$got]"
    status=1
  fi

  # A comment is not code. The vendor gate learned this one the expensive way.
  cat > "$tmp/src/lib.rs" <<'RS'
// SELECT id FROM users WHERE id = $1
pub fn q() {}
RS
  got="$(sql_sites "$tmp/src")"
  if [ -z "$got" ]; then
    echo "  ok   a statement inside a comment is not a statement site"
  else
    echo "  FAIL a comment was counted as a statement: [$got]"
    status=1
  fi

  # Longest-verb-first. `ROLLBACK TO SAVEPOINT` must not key as `ROLLBACK`:
  # the wrong order merges two baseline rows and lets one excuse the other.
  cat > "$tmp/src/lib.rs" <<'RS'
pub fn q(name: &str) -> String {
    format!("ROLLBACK TO SAVEPOINT {name}")
}
RS
  got="$(sql_sites "$tmp/src")"
  if [ "$got" = "$(printf 'lib.rs\t2\tROLLBACK TO SAVEPOINT')" ]; then
    echo "  ok   the longest matching verb wins"
  else
    echo "  FAIL verb keying is wrong: [$got]"
    status=1
  fi

  # An uppercase identifier that merely STARTS with a verb is not one.
  cat > "$tmp/src/lib.rs" <<'RS'
pub const SELECT_LIMIT: usize = 10;
pub const MAX_SAVEPOINT_DEPTH: usize = 8;
RS
  got="$(sql_sites "$tmp/src")"
  if [ -z "$got" ]; then
    echo "  ok   a constant whose name starts with a verb is not a site"
  else
    echo "  FAIL an identifier was counted as a statement: [$got]"
    status=1
  fi

  # The SKIP predicate has its own controls. A wrong answer there is invisible
  # from here: a file `production_files` declines to enumerate contributes
  # nothing to arm 2 and prints exactly what a clean file prints.
  module_gating_self_test || status=1

  return "$status"
}

if [ "$SELF_TEST" -eq 1 ]; then
  self_test
  exit $?
fi

[ -d "$ROOT" ] || { echo "gate cannot run: root $ROOT is missing"; exit 1; }

echo "decision four gate (root: $ROOT)"

FILES="$(production_files "$ROOT")"
N_FILES=0
[ -n "$FILES" ] && N_FILES="$(printf '%s\n' "$FILES" | grep -c .)"

SITES="$(sql_sites "$ROOT")"
N_SITES=0
[ -n "$SITES" ] && N_SITES="$(printf '%s\n' "$SITES" | grep -c .)"

# --- Arm 1: the extractor could read the crate ----------------------------
#
# THE ANTI-VACUITY ANCHOR. Everything else this gate says is conditional on
# having read the files. 29 production files today (33 `.rs` files, four of them
# modules gated at their declaration: `auth/util.rs`, `transaction/probe.rs`,
# `transaction/reducer/tests.rs`, `test_support/mod.rs`). Floor 20: far enough
# below that ordinary deletion does not reach it, close enough that a moved root
# or a broken `find` does. Unlike the two arms below, THIS number has no
# legitimate reason to fall - the crate is not shrinking to nothing - so it is
# the one floor here that is a floor in the ordinary sense.
if ! gate_arm production_files "$N_FILES" 20; then
  fail "the walk found $N_FILES production file(s) under $ROOT. Everything below
       is meaningless: fix the enumeration, do not lower the floor."
else
  pass "$N_FILES production file(s) enumerated under $ROOT"
fi

# --- Arm 2: no new statement text, and the totals have not risen ----------
#
# THE FLOOR IS A TRIPWIRE FOR A BROKEN EXTRACTOR, NOT A CLAIM THAT SQL MUST
# EXIST. 37 sites today; floor 4. If the residue is genuinely cleared to under
# four, THIS GATE HAS DONE ITS JOB AND MUST BE DELETED IN THE SAME COMMIT that
# clears it - a decision-4 gate over a compliant crate has nothing to rule on.
# What must never happen is the floor being lowered to accommodate an extractor
# that stopped matching, which looks identical from here.
new_sites=""
n_ruled=0
n_vendor=0
n_per_arm=0
n_shared=0
n_prose=0
while IFS= read -r row; do
  [ -n "$row" ] || continue
  n_ruled=$((n_ruled + 1))
  key="$(printf '%s' "$row" | cut -f1)"
  lineno="$(printf '%s' "$row" | cut -f2)"
  verb="$(printf '%s' "$row" | cut -f3)"
  class="$(printf '%s\n' "$BASELINE" | grep -F "$key	$verb	" | cut -f3)"
  case "$class" in
    vendor)  n_vendor=$((n_vendor + 1)) ;;
    per-arm) n_per_arm=$((n_per_arm + 1)) ;;
    shared)  n_shared=$((n_shared + 1)) ;;
    prose)   n_prose=$((n_prose + 1)) ;;
    *)       new_sites="$new_sites
    $key:$lineno  $verb" ;;
  esac
done <<EOF
$SITES
EOF

backend_cost=$((n_vendor + n_per_arm))

if ! gate_arm statement_sites "$n_ruled" 4; then
  fail "the site enumeration ruled on $n_ruled site(s), under its floor of 4.
       Whatever it reports about new statement text says nothing: fix the
       extractor, do not lower the floor."
elif [ -n "$new_sites" ]; then
  fail "production SQL statement text in the engine that no baseline row covers:$new_sites

       Decision 4 says a new database costs a new crate and three enum variants
       and NOTHING ELSE - no statement text, no dialect match, no vendor error
       handling. Statement text here is a bill the next backend has to pay.
       Do ONE of these:
         * put the statement in the vendor tier (zeroship-data-postgres /
           zeroship-data-sqlite) behind the seam that already exists;
         * if it is genuinely not SQL - an error message that opens with a verb -
           add the row to BASELINE in this file with class 'prose' and say so;
         * if it is debt somebody has scheduled, add the row with its class and
           the Open that deletes it.
       Do not reword the string to dodge the detector: that loses the finding."
elif [ "$n_ruled" -gt "$SITE_CEILING" ]; then
  fail "$n_ruled statement site(s), over the ceiling of $SITE_CEILING. Every site
       is on the baseline, so no NEW surface appeared - but an existing one grew.
       This ceiling only ever goes down."
elif [ "$backend_cost" -gt "$BACKEND_COST_CEILING" ]; then
  fail "vendor + per-arm sites: $backend_cost, over the ceiling of
       $BACKEND_COST_CEILING. That is the surface a NEW BACKEND has to write for
       itself, and it only ever goes down."
else
  pass "all $n_ruled statement site(s) are baselined \
(vendor $n_vendor, per-arm $n_per_arm, shared $n_shared, prose $n_prose)"
  if [ "$n_ruled" -lt "$SITE_CEILING" ]; then
    echo "  note SITE_CEILING is $SITE_CEILING and the tree holds $n_ruled."
    echo "       Lower it to $n_ruled in the commit that did the work; the"
    echo "       ratchet is slack by $((SITE_CEILING - n_ruled)) until you do."
  fi
  if [ "$backend_cost" -lt "$BACKEND_COST_CEILING" ]; then
    echo "  note BACKEND_COST_CEILING is $BACKEND_COST_CEILING and the tree"
    echo "       holds $backend_cost. Lower it to $backend_cost."
  fi
fi

# --- Arm 3: every baseline row still describes a live site ----------------
#
# THE REVERSE DIRECTION, and it doubles as arm 2's positive control: a row
# matching nothing is either an exemption nobody remembers granting or - the
# case that matters - proof that the extractor stopped matching, which arm 2's
# floor would only catch once the collapse was near-total.
#
# Floor 4 against 15 rows, for the same reason as arm 2's: rows come off this
# list as the debt is paid, and the last one coming off is the signal to delete
# the gate, not to lower the number.
stale=""
n_rows=0
bad_class=""
while IFS= read -r row; do
  [ -n "$row" ] || continue
  n_rows=$((n_rows + 1))
  key="$(printf '%s' "$row" | cut -f1)"
  verb="$(printf '%s' "$row" | cut -f2)"
  class="$(printf '%s' "$row" | cut -f3)"
  case "$class" in
    vendor|per-arm|shared|prose) ;;
    *) bad_class="$bad_class
    $key  $verb  has class '$class', which is not one of vendor/per-arm/shared/prose" ;;
  esac
  printf '%s\n' "$SITES" | grep -qF "$key	" || {
    stale="$stale
    $key  $verb  (the FILE holds no statement site at all)"
    continue
  }
  printf '%s\n' "$SITES" | cut -f1,3 | grep -qxF "$key	$verb" || stale="$stale
    $key  $verb"
done <<EOF
$(printf '%s\n' "$BASELINE" | grep -v '^[[:space:]]*$')
EOF

if ! gate_arm baseline_rows "$n_rows" 4; then
  fail "the baseline has collapsed to $n_rows row(s). It is the reference arm 2
       measures against, so arm 2's result above says nothing."
elif [ -n "$stale$bad_class" ]; then
  fail "baseline rows that no longer describe the tree:$stale$bad_class
       Either the statement went - delete the row in the same commit, and lower
       the ceilings while you are there - or the extractor stopped matching it,
       in which case arm 2's clean result above means nothing."
else
  pass "all $n_rows baseline row(s) still describe live statement text"
fi

echo
gate_arms_finish || FAIL=$((FAIL + 1))

echo "  decision four gate: $PASS passed, $FAIL failed"
echo "  ($N_SITES site(s) over $N_FILES production file(s); ceilings"
echo "   $SITE_CEILING total / $BACKEND_COST_CEILING vendor+per-arm)"
[ "$FAIL" -eq 0 ] || exit 1
