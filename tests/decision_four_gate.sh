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
# WHAT IT RULES ON. Four arms, four different questions:
#
#   1. production_files - how many files the production-region extractor could
#      read at all. The anti-vacuity anchor: this number can never legitimately
#      collapse while the crate exists.
#   2. statement_sites - every production SQL site found is on the BASELINE
#      below, and the totals have not risen past their ceilings.
#   3. baseline_rows   - every baseline row still describes a live site. A gate
#      whose excuse list outlives the thing it excuses is how a census goes
#      stale, and this repository has four recorded instances.
#   4. dispatch_shape  - every destructuring of `BackendHandle`, `TxConnection`
#      or `TxCanceller` is an EXHAUSTIVE `match`, or carries an allowlist row
#      with a reason. Added 2026-09-04. Arms 1-3 ask what the engine WRITES;
#      this one asks whether the compiler can still bill a new backend for the
#      branches it has to answer, which is the other half of decision 4's
#      "a new variant in the three dispatch enums, and nothing else".
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
#   - ARMS 1-3 rule on `crates/zeroship-data-engine/src` and nothing else. The
#     adapter, the vendor tiers and the migration engine are all SUPPOSED to
#     hold statement text and are not scanned. ARM 4 also reads
#     `crates/zeroship-plugin-db/src`, because the three dispatch enums are
#     destructured on both sides of the adapter seam and a collapse over there
#     costs a new backend exactly as much.
#   - ARM 4 IS A SOURCE-SHAPE CHECK. It rules on how a dispatch is SPELLED and
#     can no more tell a correct arm from a wrong one than arm 2 can tell SQL
#     from prose. Its own limits are listed above `AWK_DISPATCH` below.
#
# Run the detector's own positive/control set: this script --self-test.
# Point it at a copied tree to watch it fail:
#   this script --root <dir> [--adapter-root <dir>]
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
# The dispatch-shape arm alone spans TWO roots. Decision 4's three enums are
# DEFINED in the engine and DESTRUCTURED on both sides of the adapter seam, and
# an arm that watched only the engine would report a clean shape while
# `zeroship-plugin-db` collapsed a dispatch. Arms 1-3 stay engine-only: their
# question is "does the ENGINE hold statement text", and the adapter is supposed
# to.
ADAPTER_ROOT="crates/zeroship-plugin-db/src"
SELF_TEST=0
while [ "$#" -gt 0 ]; do
  case "$1" in
    --root)
      shift
      [ "$#" -ge 1 ] || { echo "  x REFUSED: --root needs a directory" >&2; exit 1; }
      ROOT="$1"; shift ;;
    --adapter-root)
      shift
      [ "$#" -ge 1 ] || { echo "  x REFUSED: --adapter-root needs a directory" >&2; exit 1; }
      ADAPTER_ROOT="$1"; shift ;;
    --self-test) SELF_TEST=1; shift ;;
    *) echo "usage: $0 [--root <dir>] [--adapter-root <dir>] [--self-test]" >&2; exit 1 ;;
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

# ===========================================================================
# THE DISPATCH-SHAPE EXTRACTOR (arm 4). A DIFFERENT QUESTION FROM ARMS 1-3.
# ===========================================================================
# Decision 4 says a new database costs "a new variant in the three dispatch
# enums (`BackendHandle`, `TxConnection`, `TxCanceller`), and nothing else".
# That is only true where the compiler can BILL the author for the new variant,
# and it can do that at an exhaustive `match` and nowhere else. `matches!`, an
# `if let`, a `let ... else` and a `_ =>` arm all answer for a variant nobody has
# written yet, silently, and their answer is whatever the existing code assumed.
#
# WHY THIS CANNOT BE A RUNTIME TEST, which is why it is here. `9905acca1` fixed
# six such sites, one of them live: `backend_publishes_committed_changes` was
# `matches!(backend, BackendHandle::Sqlite(_))`, so a third backend would read
# `false`, the engine would publish on its behalf, and a backend that publishes
# its own commits would deliver every change TWICE. No test can exercise that: a
# third variant does not exist, and `BackendHandle::Postgres` needs a live pool.
# The compile error a new variant produces at an exhaustive `match` IS the
# regression test - and nothing stopped a later edit collapsing one back. This
# arm is that guard.
#
# WHAT IT CANNOT DO, and the first item is the important one:
#
#   - IT IS A SOURCE-SHAPE CHECK. It rules on how a dispatch is SPELLED, never
#     on whether the arm is right. A `match` with two arms that both do the
#     wrong thing passes here, exactly as `9905acca1`'s own doc comment says:
#     "it cannot check that the answer is CORRECT".
#   - It is blind to a dispatch that never names a variant: a helper taking
#     `&BackendHandle` and returning a bool, a trait object, a `dyn` port. Those
#     move the decision somewhere this extractor does not look.
#   - It reads ALL regions, production and test alike, because the shape
#     question does not stop at the test boundary and because the one
#     `matches!` in the tree is itself `#[cfg(test)]`. That is deliberate and it
#     is why the allowlist below has test-region rows.
#   - `Enum::Variant(..)` in EXPRESSION position - a construction, or the same
#     text inside a string literal - is not a destructuring and is not ruled on.
#     The count of those is printed on every run so the residue is visible
#     rather than merely excluded; 25 of them today, all verified by reading.
#   - It assumes rustfmt's layout: one match arm per line, arms of one `match`
#     sharing an indentation column. A `_ =>` written ABOVE the enum arms of its
#     own `match` reads as belonging to no match and is missed.
#
# THE ACCESSOR FORM IS INCLUDED, and that was a decision. `if let Some(sq) =
# backend.as_sqlite() { .. } else { .. }` is a two-way dispatch wearing an
# `Option` as a disguise: the `else` silently becomes "every other backend".
# Measured 2026-09-04, it is ABSENT from production - the only two `.as_sqlite()`
# / `.as_postgres()` call sites in either root are a type-shape check in
# `backend/mod.rs` and an assertion in `tx_scope.rs`, both inside `#[cfg(test)]`
# modules. Fencing a form while it has two uses costs two allowlist rows;
# fencing it after it spreads costs an argument about each one. The definitions
# (`pub fn as_postgres`) are not call sites and do not match.

AWK_DISPATCH=$(cat <<'AWK'
BEGIN { ENUMS = "BackendHandle|TxConnection|TxCanceller"
        PATHRE = "(" ENUMS ")::[A-Z]"
        SELFRE = "Self::[A-Z]"
        fnname = "<file scope>" }
{
  line = $0
  ind = line; sub(/[^ \t].*$/, "", ind); indent = length(ind)
  t = line; sub(/^[ \t]+/, "", t); sub(/[ \t]+$/, "", t)

  if (t ~ /^\/\//) next                        # a comment dispatches on nothing
  # Leaving a block forgets the match arms recorded inside it, so a `_ =>` in a
  # LATER match at the same column is not attributed to an earlier one.
  if (t ~ /^\}/) { for (k in armind) if (k + 0 > indent) delete armind[k] }

  # The nearest preceding `fn` names the site. Line numbers are the wrong key
  # for an allowlist - they move whenever anything above them is edited, and
  # this file is edited constantly - so the allowlist is keyed on the function.
  if (t ~ /^(pub([ \t]*\([^)]*\))?[ \t]+)?(default[ \t]+)?(const[ \t]+)?(async[ \t]+)?(unsafe[ \t]+)?(extern[ \t]+"[^"]*"[ \t]+)?fn[ \t]+[A-Za-z0-9_]+/) {
    fnname = t
    sub(/^.*[^A-Za-z0-9_]fn[ \t]+/, "", fnname); sub(/^fn[ \t]+/, "", fnname)
    sub(/[^A-Za-z0-9_].*$/, "", fnname)
  }

  # Inside `impl <Enum>` the arms are spelled `Self::`, which is the majority
  # form in backend_handle.rs and cancel.rs. An extractor keyed only on the type
  # name would have reported those files as holding no dispatch at all.
  if (t ~ ("^impl([ \t]+[A-Za-z0-9_:<>, ]+[ \t]+for)?[ \t]+(" ENUMS ")[ \t]*(\\{|$)")) {
    selfenum = 1; implind = indent; next
  }
  if (selfenum && t ~ /^\}/ && indent <= implind) selfenum = 0

  has = (line ~ PATHRE) || (selfenum && line ~ SELFRE)

  # A pattern is on the LEFT of `=` (a binding) or of `=>` (an arm). The same
  # text on the RIGHT is a CONSTRUCTION: `let handle = BackendHandle::Sqlite(x)`
  # destructures nothing, and counting it would bury the real sites in noise.
  lhs = t; sub(/=[^>].*$/, "", lhs); sub(/=$/, "", lhs)
  arml = t; sub(/=>.*$/, "", arml)
  lhs_has  = (lhs  ~ PATHRE) || (selfenum && lhs  ~ SELFRE)
  arml_has = (arml ~ PATHRE) || (selfenum && arml ~ SELFRE)

  # AN `if let` PATTERN IS NOT BOUNDED BY THE FIRST `=` ON THE LINE, and reusing
  # `lhs` for it was a live hole: `let rows = if let BackendHandle::Sqlite(sq) =
  # route.backend() {` binds at the FIRST `=`, so `lhs` was `let rows` and the
  # dispatch fell through to `construct-or-prose` and was never ruled on. Found
  # by mutating a real `match` into exactly that line and watching this gate stay
  # green. The pattern is what lies between the `let` and ITS OWN `=`.
  iflet = ""
  if (t ~ /(^|[^A-Za-z0-9_])(if|while)[ \t]+let[ \t]/) {
    iflet = t
    sub(/^.*(if|while)[ \t]+let[ \t]+/, "", iflet)
    sub(/=[^>].*$/, "", iflet); sub(/=$/, "", iflet)
  }
  iflet_has = (iflet != "") && ((iflet ~ PATHRE) || (selfenum && iflet ~ SELFRE))

  emitted = 0
  if (has) {
    if (line ~ /matches!/) { print NR "\t" fnname "\tmatches!\t" t; emitted = 1 }
    else if (iflet_has) {
      print NR "\t" fnname "\tif-let\t" t; emitted = 1 }
    # A refutable `let` REQUIRES an `else`, so the compiler guarantees this is a
    # `let ... else` without the extractor having to find the keyword - which
    # matters because rustfmt moves `else {` to the next line when it does not
    # fit.
    else if (t ~ /^let[ \t]/ && lhs_has) { print NR "\t" fnname "\tlet-else\t" t; emitted = 1 }
    else if (t ~ /=>/ && arml_has) {
      # One SITE per match, not per arm: the first enum arm at a column opens
      # it, the rest belong to it. Counting arms would make a two-variant enum
      # score double a one-variant one for the same single decision.
      if (!(indent in armind)) { print NR "\t" fnname "\tmatch\t" t }
      armind[indent] = 1
      emitted = 1
    }
    else { print NR "\t" fnname "\tconstruct-or-prose\t" t; emitted = 1 }
  }
  else if (t ~ /^_[ \t]*(if[^=]*)?=>/ && (indent in armind)) {
    print NR "\t" fnname "\twildcard\t" t; emitted = 1
  }
  if (line ~ /\.as_sqlite\(\)|\.as_postgres\(\)/ && !emitted) {
    print NR "\t" fnname "\taccessor\t" t
  }
}
AWK
)

# dispatch_sites - `<tag>/<rel><TAB><line><TAB><fn><TAB><class><TAB><text>`.
#
# The `engine/` and `adapter/` tags qualify the key. Both roots hold a `lib.rs`
# and a `context.rs`-shaped file, and an unqualified key would let one crate's
# allowlist row excuse the other crate's collapse - the same defect
# tests/vendor_embedding_gate.sh's `file_key` exists to prevent.
dispatch_sites() {
  local tag root f rel
  for spec in "engine|$ROOT" "adapter|$ADAPTER_ROOT"; do
    tag="${spec%%|*}"; root="${spec#*|}"
    [ -d "$root" ] || continue
    while IFS= read -r f; do
      rel="${f#"$root"/}"
      awk "$AWK_DISPATCH" "$f" | sed "s|^|$tag/$rel\t|"
    done < <(find "$root" -name '*.rs' -type f | LC_ALL=C sort)
  done
}

# ---------------------------------------------------------------------------
# THE DISPATCH ALLOWLIST. One row per NON-`match` destructuring that is allowed:
#
#     <tag>/<path under that root>  <enclosing fn>  <class>  <one-line reason>
#
# A row must carry a reason, and the reason must be about THIS site. "It is
# fine" is not one. Arm 4 also refuses a row that matches nothing, so the list
# cannot outlive what it excuses.
#
# Measured 2026-09-04: 48 sites, 39 of them exhaustive `match` and 9 here.
#
# THE BRIEF FOR THIS ARM PREDICTED ONE ROW - `pool_initialised` - AND THE
# EXTRACTOR FOUND NINE. The eight it did not predict are not new defects; they
# are forms the hand inventory did not look for. Three are `let ... else`, which
# nobody had named as a dispatch shape at all, and THOSE THREE ARE THE ONLY
# NON-`match` DESTRUCTURINGS IN PRODUCTION CODE in either root.
# ---------------------------------------------------------------------------
DISPATCH_ALLOW='
adapter/context.rs	pool_initialised	matches!	predicate about ONE named variant, and #[cfg(test)] besides
engine/backend_handle.rs	run_planned_postgres_read	let-else	inside a fn that already took &PostgresBackend; else = lane_vendor_mismatch
engine/backend_handle.rs	read_raw_column_bytes	let-else	inside the Postgres arm of an exhaustive match; else = lane_vendor_mismatch
engine/backend_handle.rs	read_raw_column_text	let-else	inside the Postgres arm of an exhaustive match; else = lane_vendor_mismatch
engine/exec.rs	sqlite_exec_helpers_use_tx_connection_when_present	if-let	test asserting the SQLite lane specifically
engine/exec.rs	sec1_app_b_query_must_not_route_through_app_a_parked_tx	if-let	test asserting the SQLite lane specifically
engine/exec.rs	dropping_in_flight_sqlite_query_restores_tx_slot	if-let	test asserting the SQLite lane specifically
engine/backend/mod.rs	_shape_check	accessor	compile-time type-shape check, #[cfg(test)]
adapter/tx_scope.rs	set_mask_policy_installs_through_an_adapter_opened_cold_backend	accessor	test assertion that the opened backend is the sqlite one
'
# THE THREE `let ... else` ROWS ARE THE ONES TO RE-READ, because they are the
# only production entries and they are excused by CONTEXT rather than by shape.
# Each sits where the backend has already been decided - two inside the
# `BackendHandle::Postgres(pg)` arm of an exhaustive `match`, one inside a
# function whose signature is `pg: &PostgresBackend` - and asks the narrower
# question "is the lane's session the Postgres one", whose `else` is a genuine
# vendor mismatch rather than an unwritten backend. That reasoning is exactly
# `pool_initialised`'s and exactly NOT
# `backend_publishes_committed_changes`'s. If one of them is ever hoisted out of
# its arm, the row stops being true and nothing here will say so.

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
  local tmp status=0 got saved_root saved_adapter
  tmp="$(mktemp -d)"
  saved_root="$ROOT"
  saved_adapter="$ADAPTER_ROOT"
  trap 'rm -rf "$tmp"; ROOT="$saved_root"; ADAPTER_ROOT="$saved_adapter"' RETURN
  mkdir -p "$tmp/src" "$tmp/adapter"
  ROOT="$tmp/src"
  ADAPTER_ROOT="$tmp/adapter"

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

  # -------------------------------------------------------------------------
  # ARM 4's extractor. Every case here is a shape that either MUST be a site or
  # MUST NOT be one, and the pair matters: a classifier that called everything a
  # site and one that called nothing a site both pass a positive-only set.
  # -------------------------------------------------------------------------
  _d4_dispatch() {   # <rust source on stdin> -> `<line>\t<fn>\t<class>` rows
    cat > "$tmp/src/lib.rs"
    dispatch_sites | cut -f2,3,4
  }
  _d4_case() {       # <label> <expected rows, newline separated>
    local label="$1" want="$2" have
    have="$(_d4_dispatch)"
    if [ "$have" = "$want" ]; then
      echo "  ok   dispatch: $label"
    else
      echo "  FAIL dispatch: $label"
      echo "       want [$want]"
      echo "       have [$have]"
      status=1
    fi
  }

  _d4_case "an exhaustive match is ONE site, not one per arm" \
    "$(printf '3\tf\tmatch')" <<'RS'
fn f(b: &BackendHandle) -> bool {
    match b {
        BackendHandle::Sqlite(_) => true,
        BackendHandle::Postgres(_) => false,
    }
}
RS

  _d4_case "a matches! is a site" \
    "$(printf '2\tf\tmatches!')" <<'RS'
fn f(b: &BackendHandle) -> bool {
    matches!(b, BackendHandle::Sqlite(_))
}
RS

  # THE REGRESSION. `let rows = if let <Pattern> = ...` binds at the FIRST `=`
  # on the line, so a classifier that bounds the pattern there sees `let rows`
  # and drops the dispatch into the residue. This gate did exactly that until
  # 2026-09-04, and stayed GREEN when a real `match` was mutated into this line.
  _d4_case "if let behind a let-binding is still an if-let site" \
    "$(printf '2\tf\tif-let')" <<'RS'
fn f(b: &BackendHandle) -> u8 {
    let rows = if let BackendHandle::Sqlite(sq) = b { one(sq) } else { two() };
    rows
}
RS

  _d4_case "a let ... else destructuring is a site" \
    "$(printf '2\tf\tlet-else')" <<'RS'
fn f(c: TxConnection) {
    let TxConnection::Postgres(client) = c else {
        return;
    };
}
RS

  _d4_case "a _ => arm in a match that has enum arms is a site" \
    "$(printf '3\tf\tmatch\n4\tf\twildcard')" <<'RS'
fn f(b: &BackendHandle) -> u8 {
    match b {
        BackendHandle::Sqlite(_) => 1,
        _ => 2,
    }
}
RS

  # THE CONTROL FOR THE CASE ABOVE, one variable changed: a `_ =>` in a match
  # that dispatches on something else is not this gate's business. Without it,
  # the wildcard rule would fire on every `_ =>` in the workspace.
  _d4_case "a _ => arm in an unrelated match is NOT a site" \
    "" <<'RS'
fn f(n: u8) -> u8 {
    match n {
        1 => 1,
        _ => 2,
    }
}
RS

  # A construction is not a destructuring. This is the largest residue class -
  # 25 lines in the real tree - and counting it would bury the nine real
  # exceptions in noise.
  _d4_case "constructing a variant is residue, not a site" \
    "$(printf '2\tf\tconstruct-or-prose')" <<'RS'
fn f(x: Rc<SqliteBackend>) -> BackendHandle {
    let handle = BackendHandle::Sqlite(x);
    handle
}
RS

  # `Self::` inside `impl <Enum>` is the MAJORITY arm spelling in
  # backend_handle.rs and cancel.rs. An extractor keyed only on the type name
  # reports those files as holding no dispatch at all.
  _d4_case "Self:: arms inside impl <Enum> are recognised" \
    "$(printf '4\tf\tmatch')" <<'RS'
impl BackendHandle {
    fn f(&self) -> u8 {
        match self {
            Self::Sqlite(_) => 1,
            Self::Postgres(_) => 2,
        }
    }
}
RS

  # ... and the control: the same spelling OUTSIDE such an impl belongs to some
  # other enum and must not be claimed.
  _d4_case "Self:: outside an impl <Enum> is not claimed" \
    "" <<'RS'
impl SomethingElse {
    fn f(&self) -> u8 {
        match self {
            Self::Sqlite(_) => 1,
            Self::Postgres(_) => 2,
        }
    }
}
RS

  _d4_case "an .as_sqlite() call site is an accessor dispatch" \
    "$(printf '2\tf\taccessor')" <<'RS'
fn f(b: &BackendHandle) -> bool {
    b.as_sqlite().is_some()
}
RS

  _d4_case "a comment describing a matches! is not a site" \
    "" <<'RS'
// It was `matches!(backend, BackendHandle::Sqlite(_))` until 2026-09-04.
fn f() {}
RS

  unset -f _d4_dispatch _d4_case
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

# --- Arm 4: every dispatch on the three enums is an exhaustive `match` -----
#
# THE FLOOR IS ON SITES EXAMINED, NOT ON THE ALLOWLIST. A floor on the allowlist
# would be a floor on the EXCEPTIONS: it would go red when somebody removed one,
# and stay green while the extractor stopped finding dispatches at all. 48 sites
# today (39 `match`, 9 allowlisted); floor 20, far enough below that ordinary
# refactoring does not reach it and close enough that a broken regex does.
DSITES="$(dispatch_sites)"

d_examined=0
d_prose=0
d_match=0
d_bad=""
d_seen=""
while IFS= read -r row; do
  [ -n "$row" ] || continue
  dkey="$(printf '%s' "$row" | cut -f1)"
  dline="$(printf '%s' "$row" | cut -f2)"
  dfn="$(printf '%s' "$row" | cut -f3)"
  dclass="$(printf '%s' "$row" | cut -f4)"
  case "$dclass" in
    construct-or-prose) d_prose=$((d_prose + 1)); continue ;;
    match) d_examined=$((d_examined + 1)); d_match=$((d_match + 1)); continue ;;
  esac
  d_examined=$((d_examined + 1))
  if printf '%s\n' "$DISPATCH_ALLOW" | grep -qF "$dkey	$dfn	$dclass	"; then
    d_seen="$d_seen
$dkey	$dfn	$dclass"
    continue
  fi
  d_bad="$d_bad
    $dkey:$dline  $dfn  is a '$dclass', not an exhaustive match"
done <<EOF
$DSITES
EOF

# The reverse direction, and it doubles as the arm's positive control: a row
# matching nothing is either an exemption nobody remembers granting or proof the
# extractor stopped matching, which the floor alone would only catch once the
# collapse was near-total.
d_stale=""
d_rows=0
while IFS= read -r row; do
  [ -n "$row" ] || continue
  d_rows=$((d_rows + 1))
  akey="$(printf '%s' "$row" | cut -f1)"
  afn="$(printf '%s' "$row" | cut -f2)"
  acl="$(printf '%s' "$row" | cut -f3)"
  printf '%s\n' "$d_seen" | grep -qxF "$akey	$afn	$acl" || d_stale="$d_stale
    $akey  $afn  $acl"
done <<EOF
$(printf '%s\n' "$DISPATCH_ALLOW" | grep -v '^[[:space:]]*$')
EOF

if ! gate_arm dispatch_shape "$d_examined" 20; then
  fail "the dispatch extractor ruled on $d_examined site(s), under its floor of
       20. Whatever it reports about dispatch shape says nothing: fix the
       extractor, do not lower the floor."
elif [ -n "$d_bad" ]; then
  fail "dispatch on BackendHandle / TxConnection / TxCanceller that is not an
       exhaustive match and is not allowlisted:$d_bad

       A non-match form answers for a variant nobody has written yet, and
       answers silently. \`backend_publishes_committed_changes\` was
       \`matches!(backend, BackendHandle::Sqlite(_))\` until 2026-09-04: a third
       backend would have read false and every change event would have been
       delivered twice. Do ONE of these:
         * spell it as a \`match\` with one arm per variant and NO wildcard, so
           the compile error a new variant produces lands where the routing is
           decided;
         * if the question really is about ONE named variant - and \`false\` for
           an unwritten backend is therefore correct rather than assumed - add
           the row to DISPATCH_ALLOW in this file with the reason on the line.
       Do not add a \`_ =>\` arm to silence a compiler that is trying to bill
       you: that is the defect, spelled deliberately."
elif [ -n "$d_stale" ]; then
  fail "allowlist rows that match no site in the tree:$d_stale
       Either the site went - delete the row in the same commit - or the
       extractor stopped matching it, in which case the clean result above means
       nothing. Note the key is <tag>/<path> plus the ENCLOSING FN, so renaming
       the function is enough to strand a row."
else
  pass "all $d_examined dispatch site(s) accounted for \
($d_match exhaustive match, $d_rows allowlisted)"
  echo "  note $d_prose further \`Enum::Variant(..)\` occurrence(s) are"
  echo "       constructions or string literals, which destructure nothing and"
  echo "       are not ruled on. Read them if that number moves sharply."
fi

echo
gate_arms_finish || FAIL=$((FAIL + 1))

echo "  decision four gate: $PASS passed, $FAIL failed"
echo "  ($N_SITES site(s) over $N_FILES production file(s); ceilings"
echo "   $SITE_CEILING total / $BACKEND_COST_CEILING vendor+per-arm)"
[ "$FAIL" -eq 0 ] || exit 1
