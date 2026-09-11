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
#    produced by no local branch, no worktree HEAD and no worktree WORKING
#    TREE. The working trees matter most: an agent halfway through writing a
#    migration has committed nothing, and its database's hash exists nowhere in
#    git.
#
# 2. LOCAL PROCESSES. Every agent on this box runs as one user, so /proc is
#    readable and authoritative. A suite run exports its database name into its
#    own environment (PG_TEST_URL), so the name is in /proc/<pid>/environ for
#    the whole run - INCLUDING the minutes it spends in cargo with no session
#    attached.
#
#    THIS LAYER IS THE ONE THAT MATTERS, and it is why "plain DROP DATABASE
#    fails safely when sessions are attached, so attempt-and-skip is fine" is
#    true, necessary, and NOT SUFFICIENT.
#
#    MEASURED 2026-08-20 on the :5440 cluster, against another agent's live
#    billing suite, three consecutive samples taken seconds apart:
#
#        pg_stat_activity sessions on zeroship_billing_test_v65v : 0, 0, 0
#        this script's verdict                                   : IN USE, IN USE, IN USE
#                                                                  (local pids, rotating)
#
#    The suite was mid-run and progressing throughout. A suite run is eleven
#    separate cargo invocations; between them, and throughout every compile, it
#    holds ZERO backends on its database while being very much alive. A sweeper
#    trusting pg_stat_activity alone would have found that database session-free
#    three times over, dropped it cleanly, and the victim would have discovered
#    it at the next CREATE TABLE.
#
# 3. THE DROP ITSELF IS PLAIN. Never `WITH (FORCE)`. A forced drop terminates
#    the backends first and therefore always succeeds, which converts "somebody
#    is using this" from a refusal into a casualty. A plain DROP fails with
#    55006 while any session is attached - including a session that is merely
#    `idle in transaction`, which is the case where "in use" is least visible -
#    so the last word belongs to the server, atomically, after both checks
#    above have gone stale.
#
# 4. AND EVERY LAYER SAYS WHAT IT COULD NOT SEE. Each of the three above
#    answers with a SET, and an empty set has two unrelated causes: nothing is
#    alive, or the check could not look. Those printed the same table, the same
#    counts and the same exit 0, so "no evidence of life" was read as "evidence
#    of death" - and the consequence is an irreversible drop of a peer agent's
#    run or of the one artifact somebody is debugging a failed suite from.
#
#    The ways each layer can come back empty WITHOUT the truth being "nothing
#    is alive", all of them live in this script or in the module it calls:
#
#      layer 1  a working tree whose migration set cannot be hashed is skipped,
#               and its agent - the one midway through authoring a migration,
#               with nothing committed - is exactly whose database then looks
#               unreachable. `git worktree list` or `git for-each-ref` failing
#               does the same for the whole set at once.
#      layer 2  the pg_stat_activity query failing left SESSIONS empty and
#               every database reading as zero sessions attached. Its exit
#               status was never checked, while the query beside it was.
#      layer 2  /proc unlistable; /proc hiding processes (hidepid=2, subset=pid,
#               a PID namespace); a scan subcommand that failed and set its own
#               variables to "nothing held"; and the environment half going
#               dead - the half that sees a suite sitting in cargo with no
#               backend - while the argv half keeps the pass looking healthy.
#      layer 2  a run that stops spelling its database name in argv or in its
#               environment becomes invisible here. That is a property of
#               tests/lib/suite_db.sh exporting TEST_DB and PG_TEST_URL, and
#               NOTHING IN THIS SCRIPT CAN DETECT IT CHANGING. It is the one
#               enumerated hole with no guard; the selftest's holder case is
#               what pins it.
#      layer 1  the family list going stale - the suites rename their databases
#               and the sweeper owns none of them. This one runs the OTHER way:
#               a database the sweeper does not own is one it can never
#               reclaim, so it leaks rather than deletes. It still prints the
#               same "nothing to do" as a clean cluster, so the candidate count
#               is stated out loud below rather than left to be inferred.
#
#    Everything above except the last refuses now, and only when something is
#    on the drop list: every one of these gaps can only make a database look
#    MORE dead, so a drop list that is already empty is a conclusion no further
#    evidence could overturn. See `zs_sweep_decide`.
#
# DRY RUN BY DEFAULT. `--apply` is required to drop anything.
#
# EXIT CODES
#   0  scanned, and either nothing was dead or everything planned was dropped
#   1  a drop was REFUSED by the server (reported by name, never counted as a
#      success), or the plan itself was refused for lack of evidence
#   2  usage, configuration, or a server that could not be read at all
#   3  REFUSED: something was on the drop list and this run could not establish
#      that it was dead. Distinct from 0 on purpose - that identity was the
#      whole defect.
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
# The decisions live in tests/lib/sweep_db.sh, separated from the server they
# are made against; tests/lib_sweep_db_selftest.sh covers them.
# ============================================================================
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

. "$ROOT/tests/lib/suite_db.sh"
. "$ROOT/tests/lib/sweep_db.sh"
. "$ROOT/tests/lib/test_config.sh"

ZS_SWEEP_SELF_PID="$$"

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
#
# BOTH HALVES ARE COUNTED. A working tree whose migration set cannot be hashed
# used to be skipped by `2>/dev/null` and a `[ -n "$fp" ]`, which is the same
# silence as a tree that produced nothing to reach - and it is the tree of the
# agent this layer exists to protect. The counts go to `zs_sweep_decide`.
GIT_LISTING_OK=1
WORKTREES_SEEN=0
WORKTREES_FINGERPRINTED=0

WORKTREE_LIST="$(git worktree list --porcelain)" || GIT_LISTING_OK=0
while IFS= read -r line; do
  case "$line" in
    worktree\ *)
      wt="${line#worktree }"
      WORKTREES_SEEN=$((WORKTREES_SEEN + 1))
      if fp="$(zs_schema_fingerprint "$wt" 2>/dev/null)" && [ -n "$fp" ]; then
        WORKTREES_FINGERPRINTED=$((WORKTREES_FINGERPRINTED + 1))
        add_reachable "$fp" "working tree $wt"
      else
        printf '    %-12s  working tree %s HAS NO FINGERPRINT\n' "--" "$wt"
      fi
      ;;
  esac
done < <(printf '%s\n' "$WORKTREE_LIST")

# Then every local branch and every remote-tracking head, which covers a branch
# nobody has checked out anywhere.
#
# A ref that produces nothing is NOT counted as a gap: most tags and many old
# branches carry no migrations at all, and `zs_fingerprint_of_ref` returns the
# same 1 for that as for a git failure. The worktrees above are where the
# distinction is affordable and where the blast radius is.
REF_LIST="$(git for-each-ref --format='%(refname)' refs/heads refs/remotes)" || GIT_LISTING_OK=0
while IFS= read -r ref; do
  [ -n "$ref" ] || continue
  fp="$(zs_fingerprint_of_ref "$ROOT" "$ref")" || continue
  add_reachable "$fp" "$ref"
done < <(printf '%s\n' "$REF_LIST")

if [ -z "$REACHABLE" ]; then
  echo "FATAL: no fingerprint could be computed from any branch or worktree." >&2
  echo "       Every database would look unreachable, so this refuses to sweep." >&2
  exit 2
fi

# ---------------------------------------------------------------------------
# Read the server
# ---------------------------------------------------------------------------

echo "==> Reading databases from ${PG_HOST}:${PG_PORT}"
ALL="$(run_psql -d postgres -v ON_ERROR_STOP=1 -tAc \
  "SELECT datname FROM pg_database WHERE NOT datistemplate ORDER BY 1")"
status=$?
if [ "$status" -ne 0 ]; then
  echo "FATAL: could not list databases on ${PG_HOST}:${PG_PORT} (psql exit ${status})." >&2
  exit 2
fi
echo "    $(printf '%s\n' "$ALL" | grep -c .) databases"

# Sessions per database, read ONCE. Re-reading per candidate would let the
# report and the decision disagree. No WHERE on `state`: a backend that is
# `idle in transaction` is doing nothing and still blocks a plain DROP, which
# is exactly the case a state filter would get wrong - and a second reason
# measured 2026-08-20 on this cluster, which the original note did not have: to
# a role that is neither superuser nor a member of pg_read_all_stats, `state`
# comes back NULL for every backend it does not own, while `datname` does not.
# A state filter would therefore drop every OTHER agent's rows and keep only
# its own. `datname` was checked against such a role and is visible, so the
# query above is not a privilege-shaped blind spot; this one would have been.
#
# ITS EXIT STATUS IS CHECKED, and it was not. A failed query left SESSIONS
# empty, `sessions_on` answered 0 for every database, and the sweep proceeded
# on a liveness layer that had not run - the query directly above it had a
# status check and this one did not.
SESSIONS_OK=1
SESSIONS="$(run_psql -d postgres -v ON_ERROR_STOP=1 -tAc \
  "SELECT datname, count(*) FROM pg_stat_activity WHERE datname IS NOT NULL GROUP BY 1")" \
  || SESSIONS_OK=0
if [ "$SESSIONS_OK" -eq 0 ]; then
  echo "    WARNING: the pg_stat_activity query did not return." >&2
  echo "             Session counts below read '?', not 0." >&2
fi

# `?` rather than 0 when the query did not return, so the table cannot print a
# session count the server never gave it.
sessions_on() {
  local n
  [ "$SESSIONS_OK" -eq 1 ] || { printf '?'; return 0; }
  n="$(printf '%s\n' "$SESSIONS" | awk -F'|' -v d="$1" '$1 == d { print $2 }')"
  printf '%s' "${n:-0}"
}

# ---------------------------------------------------------------------------
# Classify
# ---------------------------------------------------------------------------

declare -a DOOMED=()
kept=0; keyed_live=0; held=0; legacy_skipped=0

# Every name in a suite family, written out once so the /proc scan can be a
# single pass over the process table rather than one pass per candidate.
PATTERNS="$(mktemp)"
trap 'rm -f "$PATTERNS"' EXIT
while IFS= read -r db; do
  [ -n "$db" ] || continue
  zs_sweep_family_of "$db" >/dev/null && printf '%s\n' "$db"
done < <(printf '%s\n' "$ALL") > "$PATTERNS"

# The status is CHECKED. On failure the shim sets its variables to a state that
# reads exactly like "nothing on this box holds any of these names", which is
# the one answer a scan that did not run must not be allowed to give.
CANDIDATES="$(grep -c . "$PATTERNS" || true)"
echo "==> ${CANDIDATES} of the databases on this server belong to a swept family"
if [ "$CANDIDATES" -eq 0 ]; then
  # SAID, not counted. This is the fifth way an empty answer happens: the
  # family list in xtask/src/platform_db/sweep.rs stopped matching what
  # the suites name their databases. It cannot delete anything - a database the
  # sweeper does not own is one it can never reclaim - so it is a LEAK rather
  # than a loss, and it prints the same "nothing to do" as a clean cluster.
  echo "    Either this cluster is clean, or the family list stopped matching" >&2
  echo "    what the suites name their databases. Nothing here can tell which." >&2
fi

echo "==> Scanning /proc for runs holding one of these names"
SCAN_OK=1
zs_sweep_scan_holders "$PATTERNS" || SCAN_OK=0
[ "$SCAN_OK" -eq 1 ] || echo "    WARNING: the /proc scan did not run." >&2

printf '\n%-46s %-10s %s\n' "DATABASE" "VERDICT" "WHY"
printf -- '---------------------------------------------------------------------------------\n'

while IFS= read -r db; do
  [ -n "$db" ] || continue
  if ! family="$(zs_sweep_family_of "$db")"; then
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
  # for KEEP. A table that only names the runs it was about to delete would
  # leave a reader unable to see that the checks fired at all.
  n="$(sessions_on "$db")"
  holders="$(zs_sweep_holders_of "$db")"
  # `?` is UNKNOWN, and it must not be spelled as either answer. Treating it as
  # "sessions attached" would hide the missing evidence behind a KEEP that
  # looks deliberate; treating it as 0 is the defect. It stays out of this
  # verdict, and `zs_sweep_decide` refuses on it below.
  if [ "$n" != "0" ] && [ "$n" != "?" ]; then
    sessions_reason="${n} session(s) attached"
  else
    sessions_reason=""
  fi
  if [ -n "$sessions_reason" ] || [ -n "$holders" ]; then
    reason="$sessions_reason"
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
echo
echo "    what the liveness scan ruled on, and what it could not:"
echo "      ${WORKTREES_FINGERPRINTED}/${WORKTREES_SEEN} working trees fingerprinted"
echo "      ${ZS_SWEEP_PROC_EXAMINED} /proc entries examined"
echo "      ${ZS_SWEEP_PROC_PEER_ENV_READ} environments read for processes outside this run"
echo "      ${ZS_SWEEP_PROC_ENV_DENIED} entries gave their argv and not their environment"
echo "      ${ZS_SWEEP_PROC_VANISHED} entries belonged to processes that had exited"
echo "      ${ZS_SWEEP_PROC_UNEXPLAINED:-none} live entries with no readable argv"

# THE DECISION IS SEPARATE FROM THE REPORT, and it is not a comment. The table
# above prints whatever it found; this rules on whether what it found is worth
# acting on. Both a run that scanned everything and found nothing dead, and a
# run that could not scan and therefore saw nothing alive, used to reach the
# `exit 0` below.
decide_out="$(zs_sweep_decide "${#DOOMED[@]}" "$SCAN_OK" "$SESSIONS_OK" \
  "$GIT_LISTING_OK" "$WORKTREES_SEEN" "$WORKTREES_FINGERPRINTED" 2>&1)"
decide_status=$?
echo
[ -n "$decide_out" ] && printf '%s\n' "$decide_out"
if [ "$decide_status" -ne 0 ]; then
  echo "==> nothing was dropped."
  exit "$decide_status"
fi

if [ "${#DOOMED[@]}" -eq 0 ]; then
  echo "==> scanned ${keyed_live} reachable and ${held} in use; nothing dead"
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
dropped=0; refused=0; REFUSED_NAMES=()
for db in "${DOOMED[@]}"; do
  out="$(run_psql -d postgres -v ON_ERROR_STOP=1 -c "DROP DATABASE ${db};" 2>&1)"
  if [ "$?" -eq 0 ]; then
    echo "    dropped  ${db}"
    dropped=$((dropped + 1))
  else
    echo "    REFUSED  ${db}: $(printf '%s' "$out" | head -2 | tr '\n' ' ')"
    refused=$((refused + 1))
    REFUSED_NAMES+=("$db")
  fi
done

echo "==> dropped ${dropped}, refused ${refused}"
# A SKIP IS REPORTED BY NAME, never absorbed into a count that reads as
# success. The server refusing a plain DROP means somebody attached to that
# database between the liveness scan and this line - which is the layer working,
# and is also the strongest evidence available that the scan above was wrong
# about it.
if [ "$refused" -ne 0 ]; then
  echo "    the server refused these, so a session was attached to each:" >&2
  for db in "${REFUSED_NAMES[@]}"; do echo "      ${db}" >&2; done
  echo "    Each one was on the drop list, so the liveness scan called it dead" >&2
  echo "    and the server disagreed. Nothing was forced; nothing was lost." >&2
  exit 1
fi
exit 0
