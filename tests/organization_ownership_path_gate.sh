#!/usr/bin/env bash
# ---------------------------------------------------------------------------
# An app reaches the person who answers for it through ONE path, and authority
# is never cached.
#
# WHY THIS EXISTS. `zeroship.app_members` is deleted. Ownership runs
# `apps.project_id -> projects.organization_id -> organization_members`, and
# there is no per-app membership table, so an app has one route to an
# organization. A second route is not a style problem. Billing picks ONE
# organization per app to invoice, and account status reads ONE organization's
# state to enforce; two routes that disagree means the party billed and the
# party suspended can differ, and nothing in a test fixture would show it
# because a fixture writes one organization and both routes agree.
#
# `apps.organization_id` IS NOT A SECOND ROUTE, AND THIS HEADER SAID THE COLUMN
# DID NOT EXIST UNTIL 2026-09-07. It exists, it is NOT NULL, and reading it is
# the ordinary way the billing and enforcement queries name an app's
# organization. What makes it a COPY rather than a CLAIM is
# `apps_project_ownership_fkey`, declared in
# `db/migrations-ts/20260906000200_apps_project_ownership_key.ts`: the pair
# `(project_id, organization_id)` is consumed by
# `projects_organization_identity_key`, so PostgreSQL refuses on every write to
# either side any app row whose organization disagrees with its project's. Arm 2
# therefore accepts the copy AND checks the key is still declared - without the
# key the column is exactly the forgeable second route this gate was built
# against, and the arm would be waving it through.
#
# THE CACHE HALF IS THE SAME PROPERTY SEEN FROM THE OTHER END. `EntityCache`
# held Cedar entities - including the membership that decides authority - for a
# TTL, keyed by principal and resource. It was deleted so that revoking a seat
# takes effect at the next request with no invalidation signal to miss. A cache
# in front of the resolve reintroduces exactly the window the deletion closed,
# and it does so invisibly: every test passes, because a test that seats a
# member and immediately acts never crosses the TTL.
#
# WHAT THIS GATE DOES NOT DO. It rules on SQL TEXT, not on what the database
# does with it. A statement that names `zeroship.projects` and then ignores the
# join is green here. It also cannot see SQL assembled from fragments this
# extractor never joins - see the extractor's own note below.
#
# Run the detectors' positive/control pair on their own: this script --self-test
# (it also runs on every ordinary invocation, so a detector that stopped
# matching is loud rather than green).
# ---------------------------------------------------------------------------
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"
# shellcheck source=tests/lib/gate_arms.sh
. "$ROOT/tests/lib/gate_arms.sh"
gate_arms_init organization_ownership_path

PASS=0
FAIL=0
pass() { PASS=$((PASS + 1)); echo "  ok   $1"; }
fail() { FAIL=$((FAIL + 1)); echo "  FAIL $1"; }

# ---------------------------------------------------------------------------
# The extractor
# ---------------------------------------------------------------------------
#
# SQL in this tree lives in Rust string literals that continue across source
# lines with a trailing backslash. A grep for a table name therefore sees ONE
# LINE of a statement and cannot tell whether the join it needs is three lines
# above. This joins each run of continuation lines back into one logical chunk
# so a verdict can be about the whole statement.
#
# ITS LIMIT, stated because a reader will otherwise assume more: a statement
# assembled from several `format!` fragments in different functions is several
# chunks here, and each is judged alone. That is why the two shared owner-join
# builders are checked BY NAME below rather than only through their callers.
#
# Comment stripping happens INSIDE this awk rather than in a preceding `sed`,
# so the whole corpus is one process instead of one per file. That is not
# tidiness: the per-file form took long enough that a run plus its mutation
# control did not fit in a single sitting, and a gate nobody runs twice is a
# gate whose green nobody has checked against a red.
chunks() {
  awk '
    FNR == 1 { if (buf != "") { print src "\t" prev "\t" buf; buf = "" } ; src = FILENAME }
    {
      line = $0
      sub(/\/\/.*/, "", line)
      sub(/[ \t\r]+$/, "", line)
      buf = buf " " line
      prev = FNR
      if (line ~ /\\$/) next
      print src "\t" FNR "\t" buf
      buf = ""
    }
    END { if (buf != "") print src "\t" prev "\t" buf }
  ' "$@"
}

# Production Rust only. A test that spells a forbidden shape in order to REFUSE
# it is not the defect - `account_reaper.rs` and `organizations.rs` both carry
# assertions naming `app_members`, and a corpus that could not tell those from
# a live query would either fail forever or be relaxed until it saw nothing.
production_sources() {
  find crates -path '*/src/*' -name '*.rs' -print | sort
}

# Does this chunk reach from an app to an organization or to a member?
reaches_an_organization() {
  local body="$1"
  case "$body" in *"zeroship.apps"*) ;; *) return 1 ;; esac
  case "$body" in
    *organization_id*|*organization_members*|*"role = 'owner'"*) return 0 ;;
    *) return 1 ;;
  esac
}

# The verdict: it must travel through the project.
travels_through_the_project() {
  local body="$1"
  case "$body" in *"zeroship.projects"*) ;; *) return 1 ;; esac
  case "$body" in *project_id*) ;; *) return 1 ;; esac
  return 0
}

# The other accepted verdict: it reads the FK-consumed copy ON THE APP ROW.
#
# Sound ONLY while `apps_project_ownership_fkey` exists - `ownership_key_is_pinned`
# below is what makes that a check rather than an assumption.
#
# IT RESOLVES THE ALIAS, and it has to. A bare `*organization_id*` test accepts
# `JOIN zeroship.organization_members m ON m.organization_id = a.owner_org`,
# which reaches an organization from an app by a column that no key pins - the
# exact shape this gate exists to refuse, waved through because the string it
# looks for appears on the OTHER side of the join. So the alias bound to
# `zeroship.apps` is extracted first and the copy must be qualified with it.
#
# A statement with no JOIN at all is accepted on a bare `organization_id`: there
# is only one table in scope, so the column cannot belong to anything else.
reads_the_pinned_copy() {
  local body="$1" after alias
  case "$body" in *"zeroship.apps"*) ;; *) return 1 ;; esac
  case "$body" in *organization_id*) ;; *) return 1 ;; esac

  # `apps.organization_id` needs no alias resolution.
  case "$body" in *"apps.organization_id"*) return 0 ;; esac

  after="${body##*zeroship.apps}"
  # shellcheck disable=SC2086  # deliberate word splitting: the next SQL token
  set -- $after
  alias="${1:-}"
  [ "$alias" = "AS" ] || [ "$alias" = "as" ] && alias="${2:-}"
  case "$alias" in
    ""|WHERE|where|JOIN|join|LEFT|left|INNER|inner|SET|set|ON|on|GROUP|group|ORDER|order|LIMIT|limit|UNION|union|\(*|\;*)
      alias="" ;;
  esac
  if [ -n "$alias" ]; then
    # The character before the alias must not be part of an identifier, or a
    # one-letter alias matches the TAIL of another table's alias: with `a`
    # bound to apps, a bare `*a.organization_id*` also accepts
    # `obs.organization_id` off `organization_billing_status`, which is a
    # different table's column and pinned by nothing.
    case "$body" in *[!A-Za-z0-9_]"$alias".organization_id*) return 0 ;; esac
  fi
  # No alias and no join: one table is in scope, so a bare column is the copy.
  case "$body" in
    *JOIN*|*join*) return 1 ;;
    *) return 0 ;;
  esac
}

# Is the composite key that pins the copy still declared?
#
# Read from the migration corpus, not from a live database: this gate runs
# without one. The two halves are the local columns and the parent they are
# consumed by; a key that named only `project_id` would leave the copy free.
ownership_key_is_pinned() {
  local body
  body="$(cat db/migrations-ts/*apps_project_ownership_key.ts 2>/dev/null)"
  case "$body" in *apps_project_ownership_fkey*) ;; *) return 1 ;; esac
  case "$body" in *'"project_id", "organization_id"'*) ;; *) return 1 ;; esac
  case "$body" in *'"id", "organization_id"'*) ;; *) return 1 ;; esac
  return 0
}

# A per-app membership table, in a SQL context. The bare identifier is excluded
# on purpose: `sql.contains("app_members")` is a test PROVING the table is gone,
# and counting it as a violation would make the gate red on its own evidence.
names_a_per_app_membership_table() {
  case "$1" in
    *"zeroship.app_members"*|*"FROM app_members"*|*"JOIN app_members"*|*"INTO app_members"*)
      return 0 ;;
    *) return 1 ;;
  esac
}

# The symbols the deleted authority cache was made of. Each is distinctive:
# `cache_get`/`cache_put` are NOT here, because `zeroship-data-orm`'s mask
# policy has its own per-thread cache under those names and banning them would
# be banning an unrelated, legitimate thing.
CACHE_SYMBOLS='EntityCache EntityCacheKey ENTITY_CACHE ENTITY_CACHE_TTL ENTITY_CACHE_CAPACITY lock_entity_cache resource_cache_key'

# $1 = symbol, $2 = root. THE ARM AND ITS CONTROL MUST CALL THE SAME FUNCTION.
# The first version of this gate spelled the search twice - `grep -rlw -- "$s"
# root --include='*.rs'` in the arm and a plain `grep -qw` in the control - and
# the arm's `--include` landed AFTER the `--` separator, where grep reads it as
# a filename. The control passed on an instrument the arm was not using.
cache_symbol_hits() {
  grep -rlw --include='*.rs' -- "$1" "$2"
}

# Arm 6's verdict on a heredoc opener that carries `||`: does the right-hand
# side ACT, or is it a no-op wearing the token the arm looks for? Defined here,
# beside the other detectors, because `self_test` exercises it and runs before
# any arm.
guard_acts() {
  local rhs="${1#*||}"
  # Leading whitespace and an opening group brace are noise; what follows is the
  # first thing that runs.
  rhs="${rhs#"${rhs%%[![:space:]]*}"}"
  rhs="${rhs#\{}"
  rhs="${rhs#"${rhs%%[![:space:]]*}"}"
  case "$rhs" in
    ""|true|true[[:space:]]*|true\;*|:|:[[:space:]]*|:\;*|/bin/true*) return 1 ;;
  esac
  case "$rhs" in
    exit*|return*|fail*|die*|echo*|printf*) return 0 ;;
  esac
  # Anything else is a command whose behaviour this gate cannot judge; treat it
  # as a guard rather than inventing a whitelist, and let the named no-ops above
  # carry the refusal.
  return 0
}

self_test() {
  local tmp status=0
  tmp="$(mktemp -d)"
  trap 'rm -rf "$tmp"' RETURN
  mkdir -p "$tmp/src"

  # POSITIVE: a two-line SQL literal that reaches an organization from an app
  # WITHOUT the project. Written across a continuation so it also proves the
  # joiner is what makes the verdict possible.
  cat > "$tmp/src/dirty.rs" <<'RS'
let sql = "SELECT m.user_id FROM zeroship.apps a \
           JOIN zeroship.organization_members m ON m.organization_id = a.organization_id";
RS
  local hit=0
  while IFS=$'\t' read -r _f _l body; do
    reaches_an_organization "$body" || continue
    hit=$((hit + 1))
    travels_through_the_project "$body" && status=1
  done < <(chunks "$tmp/src/dirty.rs")
  if [ "$hit" -eq 1 ] && [ "$status" -eq 0 ]; then
    echo "  ok   a joined statement skipping the project is caught"
  else
    echo "  FAIL the extractor saw $hit candidate chunk(s) and did not refuse the shortcut"
    status=1
  fi

  # NEGATIVE CONTROL, one variable: the same statement, through the project.
  # Without it the positive proves only that the verdict says NO to everything.
  cat > "$tmp/src/clean.rs" <<'RS'
let sql = "SELECT m.user_id FROM zeroship.apps a \
           JOIN zeroship.projects p ON p.id = a.project_id \
           JOIN zeroship.organization_members m ON m.organization_id = p.organization_id";
RS
  local clean_ok=0
  while IFS=$'\t' read -r _f _l body; do
    reaches_an_organization "$body" || continue
    travels_through_the_project "$body" && clean_ok=1
  done < <(chunks "$tmp/src/clean.rs")
  if [ "$clean_ok" -eq 1 ]; then
    echo "  ok   the same statement through the project is accepted"
  else
    echo "  FAIL the verdict refuses a correct statement; it discriminates nothing"
    status=1
  fi

  # The pinned copy, positive and control. The POSITIVE is a read of
  # `a.organization_id` off the app row; the CONTROL is a join to
  # `organization_members` on a column that is NOT the pinned copy, which must
  # still be refused - otherwise the arm accepts every statement that happens to
  # spell `organization_id` anywhere.
  if reads_the_pinned_copy 'SELECT a.organization_id FROM zeroship.apps a' \
     && ! reads_the_pinned_copy 'SELECT m.user_id FROM zeroship.apps a JOIN zeroship.organization_members m ON m.organization_id = a.owner_org' \
     && ! reads_the_pinned_copy 'SELECT x.id FROM zeroship.apps s JOIN zeroship.organization_billing_status obs ON obs.organization_id = s.owner_org'; then
    echo "  ok   a read of the pinned copy is accepted; another column and another alias's are not"
  else
    echo "  FAIL the pinned-copy verdict cannot tell apps.organization_id from any other column"
    status=1
  fi

  # The key that makes the copy sound must be FOUND, and the finder must be
  # capable of not finding it - a check that always says yes would let the copy
  # through on an unpinned schema, which is the whole hazard.
  if ownership_key_is_pinned; then
    echo "  ok   apps_project_ownership_fkey is declared over both columns"
  else
    echo "  FAIL apps_project_ownership_fkey was not found in db/migrations-ts/;"
    echo "       arm 2 must not accept apps.organization_id without it"
    status=1
  fi
  if ( cd "$tmp" && ownership_key_is_pinned ); then
    echo "  FAIL the key finder says yes with no migration corpus in sight"
    status=1
  else
    echo "  ok   the key finder says no when the corpus is absent"
  fi

  # The comment-stripping half: a shortcut written inside a comment must not be
  # reported, and a shortcut a comment merely mentions must not vouch for code.
  cat > "$tmp/src/commented.rs" <<'RS'
// let sql = "FROM zeroship.apps a JOIN zeroship.organization_members m ON m.organization_id = a.organization_id";
RS
  local commented=0
  while IFS=$'\t' read -r _f _l body; do
    reaches_an_organization "$body" && commented=$((commented + 1))
  done < <(chunks "$tmp/src/commented.rs")
  if [ "$commented" -eq 0 ]; then
    echo "  ok   a shortcut inside a comment is not reported as code"
  else
    echo "  FAIL comment stripping is not happening; every prose mention is a finding"
    status=1
  fi

  # The cache detector, on a file that spells every banned symbol.
  : > "$tmp/src/cached.rs"
  for sym in $CACHE_SYMBOLS; do echo "pub struct $sym;" >> "$tmp/src/cached.rs"; done
  local found=0 clean=0
  for sym in $CACHE_SYMBOLS; do
    [ -n "$(cache_symbol_hits "$sym" "$tmp/src")" ] && found=$((found + 1))
  done
  local declared
  declared=$(printf '%s\n' $CACHE_SYMBOLS | wc -l | tr -d ' ')
  # One-variable control: the same search over a directory holding a Rust file
  # that spells none of them must find nothing. Without it, a search that always
  # reported a hit would pass the positive.
  rm -f "$tmp/src/cached.rs"
  echo 'pub struct Unrelated;' > "$tmp/src/cached.rs"
  for sym in $CACHE_SYMBOLS; do
    [ -z "$(cache_symbol_hits "$sym" "$tmp/src")" ] && clean=$((clean + 1))
  done
  if [ "$found" -eq "$declared" ] && [ "$clean" -eq "$declared" ]; then
    echo "  ok   every banned cache symbol is found when present and not when absent ($found)"
  else
    echo "  FAIL the cache search found $found of $declared when present and was" \
         "clean on $clean of $declared when absent"
    status=1
  fi

  # The per-app membership detector, positive and control in one place.
  if names_a_per_app_membership_table 'FROM zeroship.app_members m' \
     && ! names_a_per_app_membership_table 'assert!(!sql.contains("app_members"))'; then
    echo "  ok   a SQL reference is caught and an absence assertion is not"
  else
    echo "  FAIL the per-app membership detector cannot tell SQL from an assertion"
    status=1
  fi

  # The arm-6 guard verdict. The POSITIVE cases here are the NO-OPS, because
  # those are what arm 6 was accepting: every one of them carries the `||` token
  # the old test looked for. Paired one-for-one with a guard that acts, so a
  # verdict that said "no" to everything could not pass.
  local guard_ok=1
  local noop acts
  for noop in \
    'psql_exec >/dev/null 2>&1 <<SQL || true' \
    'psql_exec <<SQL || :' \
    'psql_exec <<SQL || { true; }' \
    'psql_exec <<SQL ||'
  do
    guard_acts "$noop" && { echo "  FAIL arm 6 accepts a no-op guard: $noop"; guard_ok=0; }
  done
  for acts in \
    'psql_exec <<SQL || exit 1' \
    'psql_exec <<SQL || { fail "seed"; exit 1; }' \
    'psql_exec <<SQL || fail "seed failed"'
  do
    guard_acts "$acts" || { echo "  FAIL arm 6 refuses a real guard: $acts"; guard_ok=0; }
  done
  if [ "$guard_ok" = "1" ]; then
    echo "  ok   a '|| true' opener is refused and a '|| exit 1' opener is accepted"
  else
    status=1
  fi

  return "$status"
}

if [ "${1:-}" = "--self-test" ]; then
  echo "organization ownership path gate self-test"
  self_test
  exit $?
fi

echo "organization ownership path gate"
echo "  detector self-check"
if ! self_test; then
  echo "GATE CANNOT RUN: its own detectors failed their controls. Nothing below" >&2
  echo "  means anything until that is fixed." >&2
  exit 1
fi

# ---------------------------------------------------------------------------
# Arm 1: the corpus. Guards every arm below it.
# ---------------------------------------------------------------------------
#
# Arms 2 and 3 are ABSENCE checks over this same corpus, and an absence check
# over an empty corpus is the cleanest green there is. This arm is where that
# collapse becomes visible, which is why the two arms below declare what they
# ruled on rather than re-declaring the file count.
SOURCES="$(mktemp)"
trap 'rm -f "$SOURCES"' EXIT
production_sources > "$SOURCES"
n_sources=$(grep -c . "$SOURCES")
# MEASURED 2026-09-06: the workspace carries several hundred production Rust
# sources. The floor sits far below that because ordinary work moves the number
# by ones while the failure this guards - a renamed directory, a `find` that
# matches nothing - takes it to zero.
if ! gate_arm production_corpus "$n_sources" 200; then
  fail "the production source enumeration collapsed to $n_sources file(s).
       Every absence below is vacuous until that is fixed."
  gate_arms_finish || true
  exit 1
fi
pass "enumerated $n_sources production Rust source(s)"

# ---------------------------------------------------------------------------
# Arm 2: every app-to-organization statement travels through the project
# ---------------------------------------------------------------------------
CHUNKS="$(mktemp)"
trap 'rm -f "$SOURCES" "$CHUNKS"' EXIT
# One pass over the whole corpus, written down once. Both arms below read this
# file; re-running the extractor per arm would let the two arms disagree about
# what they examined while both printed a count.
# shellcheck disable=SC2046  # deliberate word splitting: one awk over every file
chunks $(cat "$SOURCES") > "$CHUNKS"

n_reaching=0
shortcuts=""
# The copy is only an accepted route while the database refuses to let it
# diverge. Checked ONCE, before the sweep, so a missing key turns every reader
# of the copy into a finding rather than being noticed nowhere.
pinned=0
ownership_key_is_pinned && pinned=1
while IFS=$'\t' read -r src line body; do
  reaches_an_organization "$body" || continue
  n_reaching=$((n_reaching + 1))
  if travels_through_the_project "$body"; then
    continue
  fi
  if [ "$pinned" = "1" ] && reads_the_pinned_copy "$body"; then
    continue
  fi
  shortcuts="$shortcuts
       $src:$line"
done < <(grep 'zeroship\.apps' "$CHUNKS" \
         | grep -E "organization_id|organization_members|role = 'owner'")

if [ "$pinned" != "1" ]; then
  shortcuts="$shortcuts
       db/migrations-ts/ (apps_project_ownership_fkey is NOT declared over
       (project_id, organization_id) -> projects(id, organization_id), so
       apps.organization_id is an unpinned claim and every statement above that
       reads it is a second, forgeable route)"
fi

# The two shared owner-join builders, by name. They are the ONE definition of
# "which owner is THE owner", and `app_owner_lateral` does not name
# `zeroship.apps` at all - it borrows the alias from the query it is spliced
# into - so the predicate above cannot see it. A builder that stopped going
# through the project would silently take every one of its callers with it.
OWNERS="crates/zeroship-control/src/organizations.rs"
for builder in app_owner_lateral app_owner_map; do
  n_reaching=$((n_reaching + 1))
  body=$(sed 's|//.*||' "$OWNERS" \
         | awk -v fn="pub fn $builder" 'index($0, fn) { grab = 1 } grab { printf "%s ", $0 } grab && /^}/ { exit }')
  if [ -z "$body" ]; then
    shortcuts="$shortcuts
       $OWNERS ($builder was not found - the extraction is broken, not the code)"
  elif ! travels_through_the_project "$body"; then
    shortcuts="$shortcuts
       $OWNERS ($builder)"
  fi
done

# MEASURED 2026-09-06: a handful of statements reach an organization from an
# app, plus the two builders. Floor 4 is well under that and well over the
# two builders alone, so losing the whole statement sweep is a refusal rather
# than a pass on the builders' evidence.
if ! gate_arm owner_path "$n_reaching" 4; then
  fail "only $n_reaching statement(s) were found reaching an organization from an
       app. The extractor stopped matching; the clean result says nothing."
elif [ -z "$shortcuts" ]; then
  pass "all $n_reaching app-to-organization statement(s) travel through the project or read the pinned copy"
else
  fail "these statements reach an organization or a member from an app by neither
       accepted route:$shortcuts
       There are two, and only because the second cannot disagree with the first:
       the join apps.project_id -> projects.organization_id, or a read of
       apps.organization_id, which apps_project_ownership_fkey consumes together
       with project_id. Anything else is a route that can drift, and then the
       party billed and the party enforced against differ."
fi

# ---------------------------------------------------------------------------
# Arm 3: no production SQL names a per-app membership table
# ---------------------------------------------------------------------------
n_sql_chunks=$(grep -c 'zeroship\.' "$CHUNKS")
resurrected=""
while IFS=$'\t' read -r src line body; do
  names_a_per_app_membership_table "$body" \
    && resurrected="$resurrected
       $src:$line"
done < <(grep -E 'zeroship\.app_members|FROM app_members|JOIN app_members|INTO app_members' "$CHUNKS")

# MEASURED 2026-09-06: many hundreds of chunks name a `zeroship.` object.
if ! gate_arm per_app_membership "$n_sql_chunks" 100; then
  fail "only $n_sql_chunks statement(s) naming a zeroship object were found.
       The SQL enumeration is broken, so its clean result is meaningless."
elif [ -z "$resurrected" ]; then
  pass "none of $n_sql_chunks zeroship statement(s) names a per-app membership table"
else
  fail "these production statements name a per-app membership table:$resurrected
       zeroship.app_members is deleted. Membership is organization-scoped and
       narrowed per project; an app has no members of its own."
fi

# ---------------------------------------------------------------------------
# Arm 4: no authority cache survives
# ---------------------------------------------------------------------------
n_symbols=0
survivors=""
for sym in $CACHE_SYMBOLS; do
  n_symbols=$((n_symbols + 1))
  hits=$(cache_symbol_hits "$sym" crates)
  [ -n "$hits" ] && survivors="$survivors
       $sym in $(echo "$hits" | tr '\n' ' ')"
done
# The list is what this arm rules on. Floor 5 of the seven names: a cleanup
# that legitimately merges two of them is fine, and a list emptied to nothing
# is the failure - an empty ban list finds nothing and prints success.
if ! gate_arm authority_cache "$n_symbols" 5; then
  fail "the banned-symbol list holds $n_symbols entry/entries. An empty ban list
       and a clean tree print the same thing."
elif [ -z "$survivors" ]; then
  pass "none of the $n_symbols authority-cache symbol(s) survives under crates/"
else
  fail "the deleted authority cache is back:$survivors
       Authority is resolved per request by zeroship_authz::authority::resolve.
       Caching it reopens the revocation window the deletion closed, and it does
       so invisibly: a test that seats a member and acts at once never crosses a
       TTL. If a resolve ever measures badly, the designed fallback is caching
       the User entity's own attributes - never the decision, never membership,
       never the rank."
fi

# ---------------------------------------------------------------------------
# Arm 5: no end-to-end harness writes an app row without a project
# ---------------------------------------------------------------------------
#
# `zeroship.apps.project_id` and `zeroship.apps.organization_id` are BOTH NOT
# NULL and neither has a default, so PostgreSQL already refuses a column list
# missing either - at the moment the harness runs, which for most of these is
# rarely, by hand, after a full multi-node stack has come up. This arm moves that
# refusal to the commit, where the person who wrote the INSERT is still reading
# it.
#
# IT CHECKED ONLY `project_id` UNTIL 2026-09-07, which is exactly as much as a
# green tells you: FIVE hand-written INSERTs across the harnesses named the
# project and not the organization, so every one of them was
#
#   ERROR: null value in column "organization_id" of relation "apps"
#          violates not-null constraint
#
# and the arm reported them all clean. The composite key `(project_id,
# organization_id) -> projects(id, organization_id)` is why both must be named:
# the copy is what the key consumes to keep the two from disagreeing, so it is
# not derivable and cannot be defaulted.
#
# GATES ARE EXCLUDED, and not as a convenience: `organization_authority_gate.sh`
# writes an app row with NO project ON PURPOSE, to prove the column refuses it.
# A rule that could not tell a fixture from a proof would either fail forever or
# be relaxed until it saw nothing - the same distinction arm 3 draws between a
# live query and an absence assertion.
#
# The INSERT's column list can wrap, and in one harness it does, so the window
# read below is the opening line and the two after it rather than the one line
# grep matched.
n_app_inserts=0
incomplete=""
while IFS=: read -r file line _; do
  n_app_inserts=$((n_app_inserts + 1))
  window="$(sed -n "${line},$((line + 2))p" "$file")"
  missing=""
  case "$window" in *project_id*) ;; *) missing="project_id" ;; esac
  case "$window" in
    *organization_id*) ;;
    *) missing="${missing:+$missing and }organization_id" ;;
  esac
  [ -n "$missing" ] && incomplete="$incomplete
       $file:$line (no $missing)"
done < <(grep -rn 'INSERT INTO zeroship\.apps' tests/*.sh | grep -v '_gate\.sh:')

# MEASURED 2026-09-06: a handful of harnesses write an app row by hand; every
# other harness creates its apps through the control plane, which mints the
# caller's personal organization and default project on first use.
if ! gate_arm harness_app_rows "$n_app_inserts" 3; then
  fail "found $n_app_inserts hand-written app INSERT(s) under tests/. The
       enumeration is broken, not the harnesses."
elif [ -z "$incomplete" ]; then
  pass "all $n_app_inserts harness app INSERT(s) name both ownership columns"
else
  fail "these harness INSERTs leave an ownership column unnamed:$incomplete
       apps.project_id and apps.organization_id are both NOT NULL with no
       default, so PostgreSQL refuses the row outright. Seed the organization
       and its project first - tests/lib/organization_fixture.sh emits both and
       leaves their ids in ZS_FIXTURE_ORGANIZATION_ID / ZS_FIXTURE_PROJECT_ID -
       and seat_app_owner_sql gives the app an owner through them."
fi

# ---------------------------------------------------------------------------
# Arm 6: no harness throws a seat's failure away
# ---------------------------------------------------------------------------
#
# `seat_app_owner_sql` ends in a RAISE precisely because `INSERT ... SELECT`
# over an empty result is a SUCCESSFUL statement affecting no rows, so a seat
# that matched nothing has to announce itself. Every standalone call site was
# then written as
#
#     psql_exec >/dev/null 2>&1 <<SQL
#     $(seat_app_owner_sql "$APP" "$CREATOR")
#     SQL
#
# which discards the message AND the exit status, in scripts that run `set -uo
# pipefail` with NO `-e`. The RAISE reached nobody, and the harness went on to
# fail much later as an unexplained 403 from the one service that requires a
# literal owner row. That is the exact failure the emitter was built to remove,
# reintroduced at the point of use.
#
# THE RULE. A seat is either run through `seat_app_owner` - which reads a token
# back out of the database and exits, so its caller has nothing to discard - or
# it is spliced into a heredoc whose OPENING line carries a guard that ACTS: a
# `||` whose right-hand side ends the run or records a failure, or the heredoc
# is the condition of an `if`. Nothing else counts.
#
# IT ACCEPTED A GUARD TOKEN UNTIL 2026-09-07, not a guard. The test was
# `case "$opener" in *"||"*)`, so
#
#     psql_exec >/dev/null 2>&1 <<SQL || true
#
# passed - and that line is the defect this arm exists to forbid, written with
# one more word: `|| true` swallows the status exactly as the bare form did, and
# the redirection is still throwing the RAISE away. An arm that a no-op
# satisfies is a spelling rule, not a guard.
#
# So the right-hand side is judged. `true`, `:` and `/bin/true` are refused by
# name, and a `||` with nothing after it is refused as well - the accepted forms
# are the ones that end the run (`exit`, `return`) or put the failure somewhere a
# reader will see (`fail`, `die`, `echo`, a `{ ...; }` group containing one of
# them). `2>&1 >/dev/null` on the opener is NOT refused: it hides the message,
# but the status still reaches a guard that acts on it, and the guard is what
# this arm is about.
#
# The library and the gates are excluded: the library DEFINES both spellings,
# and a gate that writes a seat is proving something about it rather than
# depending on one.

n_seats=0
unguarded=""
while IFS=: read -r file line _; do
  n_seats=$((n_seats + 1))
  # Walk up to the nearest heredoc opener and judge THAT line. The seat's own
  # line is inside the heredoc body, where a guard cannot be written.
  opener="$(awk -v n="$line" 'NR < n && /<</ { keep = $0; kept = NR } END { print keep }' "$file")"
  case "$opener" in
    *"||"*)
      guard_acts "$opener" || unguarded="$unguarded
       $file:$line (the guard is a no-op: ${opener##*||})" ;;
    if\ *|*[[:space:]]if\ *) ;;
    *) unguarded="$unguarded
       $file:$line (heredoc opened by: ${opener:-<none found>})" ;;
  esac
done < <(grep -rn '\$(seat_app_owner_sql ' tests/*.sh | grep -v '_gate\.sh:')

# The runner sites need no opener: the check is inside the function.
n_runner=$(grep -rc '^[[:space:]]*seat_app_owner "' tests/*.sh 2>/dev/null \
           | awk -F: '{ s += $2 } END { print s + 0 }')
n_seats=$((n_seats + n_runner))

# MEASURED 2026-09-06: most harnesses seat through the runner, a few splice the
# emitter into a heredoc that also writes the app row. Floor 8 sits under the
# total and above the emitter sites alone, so losing the runner sweep is a
# refusal rather than a pass on the heredoc sites' evidence.
if ! gate_arm seat_failure_is_loud "$n_seats" 8; then
  fail "found $n_seats seat site(s) under tests/. The enumeration is broken, not
       the harnesses."
elif [ -z "$unguarded" ]; then
  pass "all $n_seats harness seat(s) surface their own failure"
else
  fail "these harness seats discard the failure they were built to report:$unguarded
       seat_app_owner_sql ends in a RAISE because an INSERT ... SELECT matching
       no rows SUCCEEDS. No e2e harness here sets -e, so an unguarded heredoc
       drops that RAISE and the run continues without an owner. Call
       seat_app_owner (tests/lib/organization_fixture.sh), which reads the seat
       back and exits, or guard the heredoc with a '||' whose right-hand side
       ENDS the run or reports - '|| true' carries the token and none of the
       behaviour, which is the shape this arm used to accept."
fi

# ---------------------------------------------------------------------------
# Arm 7: every organization READ-BACK is guarded at its call site
# ---------------------------------------------------------------------------
#
# `app_organization` (tests/lib/organization_fixture.sh) exits when the read
# finds nothing, and that is not enough on its own: it is necessarily written as
# a command SUBSTITUTION, and `exit` inside `$( ... )` ends the substitution's
# subshell. `$?` carries out; the run does not stop. With no `-e` anywhere in
# these harnesses the next line then runs with an EMPTY organization id, which
# reaches a foreign key several statements later and reports a constraint name
# instead of the app nobody checked.
#
# This is arm 6's rule for the other direction of the same seam: the library
# makes the failure loud, and the call site has to be the thing that acts on it.
n_reads=0
unchecked=""
while IFS=: read -r file line _; do
  n_reads=$((n_reads + 1))
  text="$(sed -n "${line}p" "$file")"
  case "$text" in
    *"||"*) guard_acts "$text" || unchecked="$unchecked
       $file:$line (the guard is a no-op: ${text##*||})" ;;
    *) unchecked="$unchecked
       $file:$line ($text)" ;;
  esac
done < <(grep -rn '\$(app_organization ' tests/*.sh | grep -v '_gate\.sh:')

# The library is where the helper is DEFINED and where the guard is documented,
# so it is not a call site. Floor 1: one harness reads the organization back
# today, and an enumeration that found none would print exactly what a clean
# tree prints.
if ! gate_arm organization_readback "$n_reads" 1; then
  fail "found $n_reads app_organization call site(s) under tests/. The
       enumeration is broken, not the harnesses."
elif [ -z "$unchecked" ]; then
  pass "all $n_reads organization read-back(s) are guarded at the call site"
else
  fail "these organization read-backs drop the failure the helper reports:$unchecked
       app_organization exits, but inside \$( ... ) that ends the SUBSTITUTION
       and the harness runs on with an empty id. Write
       ORGANIZATION=\"\$(app_organization \"\$APP\" psql_exec)\" || exit 1."
fi

gate_arms_finish || FAIL=$((FAIL + 1))
echo "  organization ownership path gate: $PASS passed, $FAIL failed"
[ "$FAIL" -eq 0 ] || exit 1
