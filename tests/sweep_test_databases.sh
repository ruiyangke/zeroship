#!/usr/bin/env bash
# ============================================================================
# sweep_test_databases.sh - reclaim the test databases no branch can still need.
#
# WHY THIS CAN EXIST NOW
# ----------------------
# It could not before. While every agent named its own suite database, "is
# zeroship_auth_test_s46 still wanted?" had no answer in the tree - the name
# encoded an agent slot, and slots outlive nothing you can query. So nobody
# swept, and the shared cluster at :5440 reached 84 databases (measured
# 2026-08-19), the oldest of them months dead and indistinguishable from the
# four that were in use that hour.
#
# tests/lib/suite_db.sh changed the axis: a suite database is now named
# `<family>_<12 hex>` where the hash is over the migration set it was built
# from. That makes the question decidable. A hash no reachable branch produces
# is a schema nothing in this repository can ask for again.
#
# THREE LAYERS BETWEEN THIS SCRIPT AND A LIVE AGENT'S RUN
# -------------------------------------------------------
# 1. REACHABILITY. A schema-keyed database is a candidate only if its hash is
#    produced by no local branch, no worktree HEAD and no worktree WORKING TREE.
#    The working trees matter most: an agent halfway through writing a migration
#    has committed nothing, and its database's hash exists nowhere in git.
#
# 2. LOCAL PROCESSES. Every agent on this box runs as one user, so /proc is
#    readable and authoritative. A suite run exports its database name into its
#    own environment (PG_TEST_URL, TEST_DB), so the name is in
#    /proc/<pid>/environ for the whole run - INCLUDING the minutes it spends in
#    cargo with no session attached.
#
#    THIS LAYER IS THE ONE THAT MATTERS, and the reason the brief's "plain DROP
#    DATABASE fails safely when sessions are attached, so attempt-and-skip is
#    fine" is not sufficient on its own. It is true and it is necessary; it is
#    just not the whole guarantee. A suite run is eleven separate cargo
#    invocations, and between them - and throughout every compile - the run
#    holds ZERO backends on its database while being very much alive. Measured
#    on this tree: `cargo test -p zeroship-auth` spends minutes compiling before
#    the first connection. A sweeper that trusted pg_stat_activity alone would
#    find that database session-free, drop it cleanly, and the victim would
#    discover it at the next `CREATE TABLE`.
#
# 3. THE DROP ITSELF IS PLAIN. Never `WITH (FORCE)`. A forced drop terminates
#    the backends first and therefore always succeeds, which converts "somebody
#    is using this" from a refusal into a casualty. A plain DROP fails with
#    55006 while any session is attached, so the last word belongs to the
#    server, atomically, after both checks above have gone stale.
#
# DRY RUN BY DEFAULT. `--apply` is required to drop anything.
#
# USAGE
#   tests/sweep_test_databases.sh                       # plan, against the overlay's server
#   tests/sweep_test_databases.sh --port 5440           # plan, against another one
#   tests/sweep_test_databases.sh --port 5440 --apply   # do it
#   tests/sweep_test_databases.sh --legacy              # also consider the
#       pre-hash names (zeroship_auth_test_s46, ...). Their reachability CANNOT
#       be decided - the name says nothing about what is in it - so they rest on
#       layers 2 and 3 alone, which is why they need a flag.
#
# tests/sweep_test_databases_selftest.sh covers the classifier and the
# liveness checks in both directions.
# ============================================================================
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

. "$ROOT/tests/lib/suite_db.sh"
. "$ROOT/tests/lib/test_config.sh"

# The families this script owns. A database is a candidate only if its name is
# one of these, or one of these followed by `_`. Deliberately a short explicit
# list rather than a `zeroship%` wildcard: `zeroship` itself is the dev platform
# database on this cluster and carries real rows.
FAMILIES=(zeroship_auth_test zeroship_billing_test)

APPLY=0
LEGACY=0
OPT_HOST=""; OPT_PORT=""; OPT_USER=""; OPT_PASS=""

usage() {
  cat >&2 <<'EOF'
usage: tests/sweep_test_databases.sh [--apply] [--legacy]
                                     [--host H] [--port P] [--user U] [--password P]
  --apply     actually drop. Without it nothing is dropped and the plan is printed.
  --legacy    also consider pre-hash names, whose reachability cannot be decided.
  --host/--port/--user/--password  override the server from deploy/ops/zeroship.test.toml
EOF
}

while [ "$#" -gt 0 ]; do
  case "$1" in
    --apply) APPLY=1; shift ;;
    --legacy) LEGACY=1; shift ;;
    --host) OPT_HOST="${2:?--host needs a value}"; shift 2 ;;
    --port) OPT_PORT="${2:?--port needs a value}"; shift 2 ;;
    --user) OPT_USER="${2:?--user needs a value}"; shift 2 ;;
    --password) OPT_PASS="${2:?--password needs a value}"; shift 2 ;;
    -h|--help) usage; exit 0 ;;
    *) echo "FATAL: unknown argument '$1'" >&2; usage; exit 2 ;;
  esac
done

zs_test_config_load "$ROOT" || exit 2
[ -n "$OPT_HOST" ] && PG_HOST="$OPT_HOST"
[ -n "$OPT_PORT" ] && PG_PORT="$OPT_PORT"
[ -n "$OPT_USER" ] && PG_USER="$OPT_USER"
[ -n "$OPT_PASS" ] && PG_PASS="$OPT_PASS"

PSQL="${PSQL:-}"
if [ -z "$PSQL" ]; then
  if command -v psql >/dev/null 2>&1; then
    PSQL="$(command -v psql)"
  else
    PSQL="$(ls -d /nix/store/*postgresql*/bin/psql 2>/dev/null | head -1 || true)"
  fi
fi
[ -n "$PSQL" ] && [ -x "$PSQL" ] || { echo "FATAL: no psql found; set \$PSQL" >&2; exit 2; }

run_psql() { PGPASSWORD="$PG_PASS" "$PSQL" -h "$PG_HOST" -p "$PG_PORT" -U "$PG_USER" "$@"; }

# ---------------------------------------------------------------------------
# Layer 1: which schema fingerprints can any branch in this repository still
# produce?
# ---------------------------------------------------------------------------

# Recompute zs_schema_fingerprint from a git tree instead of a directory.
#
# Byte-for-byte the same construction as the library's - `<basename> ` then the
# sha256 of the file's bytes, sorted, hashed, truncated to 12 - because a
# fingerprint computed one way here and another way there would mark every
# database unreachable and delete the lot. `git cat-file blob | sha256sum` and
# `sha256sum < file` produce identical lines (both end in a literal `-`), which
# is what makes the two agree.
zs_fingerprint_of_ref() {
  local ref="$1" digest listing
  listing="$(git ls-tree "$ref" -- db/migrations-ts/)" || return 1
  [ -n "$listing" ] || return 1
  digest="$(
    printf '%s\n' "$listing" | while read -r _mode type sha name; do
      [ "$type" = "blob" ] || continue
      case "$name" in *.ts) ;; *) continue ;; esac
      printf '%s ' "${name##*/}"
      git cat-file blob "$sha" | sha256sum
    done | LC_ALL=C sort | sha256sum
  )"
  [ -n "$digest" ] || return 1
  printf '%s\n' "${digest:0:12}"
}

echo "==> Collecting the fingerprints this repository can still produce"
REACHABLE=""
add_reachable() { # add_reachable <fingerprint> <why>
  [ -n "$1" ] || return 0
  case " $REACHABLE " in *" $1 "*) return 0 ;; esac
  REACHABLE="$REACHABLE $1"
  printf '    %s  %s\n' "$1" "$2"
}

# Every WORKING TREE first, and this is not a formality. An agent that is
# midway through authoring a migration has committed nothing; its suite
# database's hash appears in no ref, only on disk. Sweeping on refs alone would
# delete exactly the database of the agent doing the most work.
while IFS= read -r line; do
  case "$line" in
    worktree\ *)
      wt="${line#worktree }"
      fp="$(zs_schema_fingerprint "$wt" 2>/dev/null)"
      [ -n "$fp" ] && add_reachable "$fp" "working tree $wt"
      ;;
  esac
done < <(git worktree list --porcelain)

# Then every local branch, every worktree HEAD, and the remote-tracking heads.
# `git for-each-ref` over refs/heads and refs/remotes covers a branch nobody has
# checked out anywhere.
while IFS= read -r ref; do
  fp="$(zs_fingerprint_of_ref "$ref")" || continue
  add_reachable "$fp" "$ref"
done < <(git for-each-ref --format='%(refname)' refs/heads refs/remotes)

if [ -z "$REACHABLE" ]; then
  echo "FATAL: no fingerprint could be computed from any branch or worktree." >&2
  echo "       Every database would look unreachable, so this refuses to sweep." >&2
  exit 2
fi

# ---------------------------------------------------------------------------
# Layer 2: is any process on this box holding this database's name?
# ---------------------------------------------------------------------------

SELF_PID="$$"
PROC_UNREADABLE=0

# Is `pid` this script or one of its descendants? Our own subshells inherit our
# cmdline and our environment, so without this every name we are examining
# would look live because WE are examining it.
zs_pid_is_ours() {
  local pid="$1" hops=0 stat ppid
  while [ "$hops" -lt 32 ]; do
    [ "$pid" = "$SELF_PID" ] && return 0
    [ "$pid" = "1" ] || [ "$pid" = "0" ] && return 1
    [ -r "/proc/$pid/stat" ] || return 1
    stat="$(cat "/proc/$pid/stat")" || return 1
    # Field 4 is ppid, but field 2 (comm) can contain spaces and parentheses,
    # so count from the LAST ')' rather than from the start of the line.
    stat="${stat##*) }"
    ppid="$(printf '%s' "$stat" | cut -d' ' -f2)"
    [ -n "$ppid" ] || return 1
    pid="$ppid"
    hops=$((hops + 1))
  done
  return 1
}

# Scan /proc ONCE for every candidate name, filling `HELD_BY` with
# `<name> <pid> <pid>...` lines for the names something on this box is holding.
#
# The ENVIRONMENT is the half that earns this scan its place. A suite run
# exports PG_TEST_URL=postgres://.../<db>, so the name sits in
# /proc/<pid>/environ from the first line of the script until the last -
# through every cargo compile, when the database has no backends at all and
# pg_stat_activity says it is idle.
#
# ONE PASS, not one per candidate. The obvious shape - loop candidates, loop
# /proc - is 62 x ~1000 x 2 greps here and did not finish in two minutes.
#
# TWO STAGES, because substring and token are different questions.
# `zeroship_auth_test` is a substring of `zeroship_auth_test_s46`, so a
# substring test alone would report the short name held whenever the long one
# is. Stage 1 uses the cheap substring test only to REJECT the ~99 percent of
# processes that mention nothing; stage 2 splits the survivors into identifier
# tokens and matches exactly.
zs_scan_local_holders() {
  local patterns="$1" d pid candidates=""
  HELD_BY=""

  for d in /proc/[0-9]*; do
    pid="${d#/proc/}"
    # A process of another user, or one that exited between the glob and here.
    # Neither can be a run of OURS holding one of these names, so skipping is
    # correct - but the count is reported, so the blindness is visible rather
    # than assumed.
    if [ ! -r "$d/environ" ] || [ ! -r "$d/cmdline" ]; then
      PROC_UNREADABLE=$((PROC_UNREADABLE + 1))
      continue
    fi
    if cat "$d/cmdline" "$d/environ" 2>/dev/null | grep -qaFf "$patterns"; then
      candidates="$candidates $pid"
    fi
  done

  for pid in $candidates; do
    # Only now, on a handful of pids: our own subshells inherit our cmdline and
    # our environment, so without this the scan can see itself.
    zs_pid_is_ours "$pid" && continue
    [ -r "/proc/$pid/environ" ] || continue
    local tokens
    tokens="$(cat "/proc/$pid/cmdline" "/proc/$pid/environ" 2>/dev/null \
      | tr -c 'a-zA-Z0-9_' '\n' | LC_ALL=C sort -u)"
    local name
    while IFS= read -r name; do
      [ -n "$name" ] || continue
      case "
$tokens
" in *"
$name
"*) HELD_BY="${HELD_BY}${name} ${pid}
" ;; esac
    done < "$patterns"
  done
}

# The pids holding `$1`, or the empty string.
zs_holders_of() {
  printf '%s' "$HELD_BY" | awk -v d="$1" '$1 == d { printf "%s ", $2 }' | sed 's/ $//'
}

# ---------------------------------------------------------------------------
# Classify
# ---------------------------------------------------------------------------

echo "==> Reading databases from ${PG_HOST}:${PG_PORT}"
ALL="$(run_psql -d postgres -v ON_ERROR_STOP=1 -tAc \
  "SELECT datname FROM pg_database WHERE NOT datistemplate ORDER BY 1")"
status=$?
if [ "$status" -ne 0 ]; then
  echo "FATAL: could not list databases on ${PG_HOST}:${PG_PORT} (psql exit ${status})." >&2
  exit 2
fi
total="$(printf '%s\n' "$ALL" | grep -c .)"
echo "    ${total} databases"

# Sessions per database, read ONCE. Re-reading per candidate would let the
# report and the decision disagree.
SESSIONS="$(run_psql -d postgres -v ON_ERROR_STOP=1 -tAc \
  "SELECT datname, count(*) FROM pg_stat_activity WHERE datname IS NOT NULL GROUP BY 1")"

sessions_on() {
  local n
  n="$(printf '%s\n' "$SESSIONS" | awk -F'|' -v d="$1" '$1 == d { print $2 }')"
  printf '%s' "${n:-0}"
}

family_of() {
  local name="$1" f
  for f in "${FAMILIES[@]}"; do
    [ "$name" = "$f" ] && { printf '%s' "$f"; return 0; }
    case "$name" in "${f}_"*) printf '%s' "$f"; return 0 ;; esac
  done
  return 1
}

declare -a DOOMED=()
kept=0; keyed_live=0; held=0; legacy_skipped=0

# Every name in a suite family, written out once so the /proc scan below can be
# a single pass over the process table rather than one pass per candidate.
PATTERNS="$(mktemp)"
trap 'rm -f "$PATTERNS"' EXIT
while IFS= read -r db; do
  [ -n "$db" ] || continue
  family_of "$db" >/dev/null && printf '%s\n' "$db"
done < <(printf '%s\n' "$ALL") > "$PATTERNS"

echo "==> Scanning /proc for runs holding one of these names"
HELD_BY=""
zs_scan_local_holders "$PATTERNS"

printf '\n%-46s %-10s %s\n' "DATABASE" "VERDICT" "WHY"
printf -- '---------------------------------------------------------------------------------\n'

while IFS= read -r db; do
  [ -n "$db" ] || continue
  if ! family="$(family_of "$db")"; then
    kept=$((kept + 1))
    continue
  fi

  suffix="${db#"$family"}"
  suffix="${suffix#_}"

  verdict=""; why=""
  if printf '%s' "$suffix" | grep -qE '^[0-9a-f]{12}$'; then
    case " $REACHABLE " in
      *" $suffix "*) verdict="KEEP"; why="schema is reachable"; keyed_live=$((keyed_live + 1)) ;;
      *)             verdict="DOOMED"; why="no branch or worktree produces ${suffix}" ;;
    esac
  elif [ "$LEGACY" -eq 1 ]; then
    verdict="DOOMED"; why="pre-hash name; reachability undecidable"
  else
    verdict="KEEP"; why="pre-hash name; needs --legacy"
    legacy_skipped=$((legacy_skipped + 1))
  fi

  # Liveness is checked for EVERY candidate, including the ones already headed
  # for KEEP. A verdict table that only names the runs it was about to delete
  # would leave a reader unable to see that the checks fired at all.
  n="$(sessions_on "$db")"
  holders="$(zs_holders_of "$db")"
  if [ "$n" != "0" ] || [ -n "$holders" ]; then
    reason=""
    [ "$n" != "0" ] && reason="${n} session(s) attached"
    if [ -n "$holders" ]; then
      [ -n "$reason" ] && reason="${reason}; "
      reason="${reason}local pid(s) ${holders} hold the name"
    fi
    if [ "$verdict" = "DOOMED" ]; then
      held=$((held + 1))
      verdict="IN USE"
    fi
    why="$reason"
  fi

  [ "$verdict" = "DOOMED" ] && DOOMED+=("$db")
  printf '%-46s %-10s %s\n' "$db" "$verdict" "$why"
done < <(printf '%s\n' "$ALL")

echo
echo "    ${kept} outside the suite families and never considered"
echo "    ${keyed_live} schema-keyed and reachable"
echo "    ${legacy_skipped} pre-hash, skipped (pass --legacy to consider them)"
echo "    ${held} in use"
echo "    ${#DOOMED[@]} to drop"
echo "    ${PROC_UNREADABLE} /proc entries unreadable during the liveness scan"

if [ "${#DOOMED[@]}" -eq 0 ]; then
  echo "==> nothing to do"
  exit 0
fi

if [ "$APPLY" -eq 0 ]; then
  echo "==> DRY RUN. Re-run with --apply to drop the ${#DOOMED[@]} above."
  exit 0
fi

# The drop is PLAIN. `WITH (FORCE)` would terminate the backends first and
# therefore always succeed, turning a database somebody attached to between the
# check above and this line into a casualty rather than a refusal. 55006
# (object_in_use) here is the server having the last word, and it is a SKIP.
dropped=0; refused=0
for db in "${DOOMED[@]}"; do
  out="$(run_psql -d postgres -v ON_ERROR_STOP=1 -c "DROP DATABASE ${db};" 2>&1)"
  if [ "$?" -eq 0 ]; then
    echo "    dropped  ${db}"
    dropped=$((dropped + 1))
  else
    echo "    REFUSED  ${db}: $(printf '%s' "$out" | head -2 | tr '\n' ' ')"
    refused=$((refused + 1))
  fi
done

echo "==> dropped ${dropped}, refused ${refused}"
[ "$refused" -eq 0 ]
