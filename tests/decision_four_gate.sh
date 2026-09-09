#!/usr/bin/env bash
# Shared ORM execution must use driver contracts. SQL rendering belongs to
# data-sql or built-in adapters; remaining shared SQL has an explicit baseline.
# Source extraction is heuristic; --self-test exercises its positive controls.
set -uo pipefail
cd "$(dirname "$0")/.."

. "$(dirname "$0")/lib/gate_arms.sh"
. "$(dirname "$0")/lib/module_gating.sh"
gate_arms_init decision_four

PASS=0
FAIL=0
pass() { PASS=$((PASS + 1)); echo "  ok   $1"; }
fail() { FAIL=$((FAIL + 1)); echo "  FAIL $1"; }

ROOT="crates/zeroship-data-orm/src"
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

VERBS='ROLLBACK TO SAVEPOINT|RELEASE SAVEPOINT|INSERT INTO|DELETE FROM|SAVEPOINT|SET LOCAL|TRUNCATE|ROLLBACK|SET ROLE|EXECUTE|RELEASE|REVOKE|SELECT|UPDATE|CREATE|COMMIT|VALUES|DO $$|BEGIN|GRANT|ALTER|DROP'

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


production_files() {
  local f
  while IFS= read -r f; do
    case "${f#"$1"/}" in backend/postgres/*|backend/sqlite/*) continue ;; esac
    module_is_test_gated "$f" "$1" && continue
    printf '%s\n' "$f"
  done < <(find "$1" -name '*.rs' -type f | LC_ALL=C sort)
}

sql_sites() {
  local root="$1" f rel
  while IFS= read -r f; do
    rel="${f#"$root"/}"
    awk -v VERBLIST="$VERBS" "$AWK_SITES" "$f" | sed "s|^|$rel\t|"
  done < <(production_files "$root")
}

BASELINE='
error.rs	COMMIT	shared	ANSI transaction intent
error.rs	ROLLBACK	shared	ANSI transaction intent
error.rs	UPDATE	prose	validation refusal text
auth/bootstrap.rs	DO $$	vendor	Open 7 (delete auth/)
auth/bootstrap.rs	BEGIN	vendor	Open 7 (delete auth/)
auth/bootstrap.rs	SELECT	vendor	Open 7 (delete auth/)
auth/bootstrap.rs	EXECUTE	vendor	Open 7 (delete auth/)
auth/bootstrap.rs	REVOKE	vendor	Open 7 (delete auth/)
auth/bootstrap.rs	GRANT	vendor	Open 7 (delete auth/)
transaction/driver.rs	SAVEPOINT	shared	unowned
transaction/driver.rs	ROLLBACK TO SAVEPOINT	shared	unowned
transaction/driver.rs	RELEASE SAVEPOINT	shared	unowned
crud/system_fields_pass.rs	UPDATE	prose	none - false positive
'
SITE_CEILING=31
BACKEND_COST_CEILING=23

self_test() {
  echo "decision four gate self-test"
  local tmp status=0 got saved_root
  tmp="$(mktemp -d)"
  saved_root="$ROOT"
  trap 'rm -rf "$tmp"; ROOT="$saved_root"' RETURN
  mkdir -p "$tmp/src" "$tmp/adapter"
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

  cat > "$tmp/src/lib.rs" <<'RS'
pub fn q() {
    let _: compio_postgres::Row;
}
RS
  if [ -n "$(driver_sites "$tmp/src/lib.rs")" ]; then
    echo "  ok   concrete driver dependency detected"
  else
    echo "  FAIL concrete driver dependency missed"; status=1
  fi
  cat > "$tmp/src/lib.rs" <<'RS'
// compio_postgres::Row is converted inside the driver.
#[cfg(test)]
mod tests {
    use compio_postgres::Row;
}
pub fn q(session: &Session) {}
RS
  if [ -z "$(driver_sites "$tmp/src/lib.rs")" ]; then
    echo "  ok   test imports and documentation excluded"
  else
    echo "  FAIL driver detector included test imports or documentation"; status=1
  fi
  return "$status"
}

AWK_DRIVER="${AWK_SITES%%  # --- statement sites*}"'
  for (i = 1; i <= NR; i++) {
    if (!(i in prod) || L[i] ~ /^[ \t]*\/\//) continue
    if (L[i] ~ /compio_postgres|rusqlite|PostgresBackend|SqliteBackend|backend::(postgres|sqlite)|\.get(_rc)?::<|\.as_(postgres|sqlite)\(/) print i "\t" L[i]
  }
}'
driver_sites() { awk "$AWK_DRIVER" "$1"; }

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

if ! gate_arm production_files "$N_FILES" 20; then
  fail "the walk found $N_FILES production file(s) under $ROOT. Everything below
       is meaningless: fix the enumeration, do not lower the floor."
else
  pass "$N_FILES production file(s) enumerated under $ROOT"
fi

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

       Put SQL rendering in data-sql or vendor adapters. If this is prose,
       classify the exact baseline row and explain why it is not SQL.
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

shared_files=0
shared_bad=""
while IFS= read -r f; do
  rel="${f#"$ROOT"/}"
  case "$rel" in
    orm.rs|orm/*|crud/*|transaction/*|exec.rs|backend_handle.rs|tx_lanes.rs|driver.rs)
      shared_files=$((shared_files + 1))
      hits="$(driver_sites "$f")"
      [ -z "$hits" ] || shared_bad="$shared_bad
$f: $hits"
      ;;
  esac
done <<EOF
$FILES
EOF
if ! gate_arm driver_independence "$shared_files" 20; then
  fail "shared execution file enumeration is empty or incomplete"
elif [ -n "$shared_bad" ]; then
  fail "shared execution names concrete drivers or downcasts:$shared_bad"
else
  pass "shared execution uses driver contracts"
fi

echo
gate_arms_finish || FAIL=$((FAIL + 1))

echo "  decision four gate: $PASS passed, $FAIL failed"
echo "  ($N_SITES site(s) over $N_FILES production file(s); ceilings"
echo "   $SITE_CEILING total / $BACKEND_COST_CEILING vendor+per-arm)"
[ "$FAIL" -eq 0 ] || exit 1
