# shellcheck shell=bash
# ============================================================================
# sweep_db.sh - the decisions tests/sweep_test_databases.sh makes, separated
# from the server it makes them against.
#
# They are in their own file because they are the part that can be WRONG
# SILENTLY. The script's other half either connects or does not; these four
# functions produce a verdict, and a wrong verdict is a dropped database.
#
# The one that matters most is `zs_fingerprint_of_ref`. It recomputes
# `zs_schema_fingerprint` from a git tree instead of from a directory, and the
# two MUST agree byte for byte. If they ever disagree, every database on the
# server matches no reachable branch, the sweeper calls the lot dead, and
# `--apply` deletes every agent's work at once. That is the single most
# destructive bug this design admits, so the selftest pins the agreement
# against the real repository rather than against a fixture.
#
# tests/lib_sweep_db_selftest.sh covers all four in both directions.
# ============================================================================

# The database families the sweeper owns. Deliberately a short explicit list
# rather than a `zeroship%` wildcard: `zeroship` itself is the dev platform
# database on the shared cluster and carries real rows.
ZS_SWEEP_FAMILIES=(zeroship_auth_test zeroship_billing_test)

# Print the family a database name belongs to, or fail.
#
# A name matches a family only if it IS the family or the family followed by
# `_`. The prefix test has to be anchored that way: `zeroship` is a prefix of
# every name here, and a sweeper that treated prefixes loosely would put the
# platform's own database on the list.
zs_sweep_family_of() {
  local name="$1" f
  for f in "${ZS_SWEEP_FAMILIES[@]}"; do
    [ "$name" = "$f" ] && { printf '%s' "$f"; return 0; }
    case "$name" in "${f}_"*) printf '%s' "$f"; return 0 ;; esac
  done
  return 1
}

# Recompute zs_schema_fingerprint from a git tree instead of a directory.
#
# Byte-for-byte the same construction as the library's - `<basename> ` then the
# sha256 of the file's bytes, sorted, hashed, truncated to 12. `git cat-file
# blob | sha256sum` and `sha256sum < file` emit identical lines (both end in a
# literal `-`, because neither is given a filename), which is what makes the
# two agree. Read the header above before changing either one.
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

# Is `pid` this shell or one of its descendants?
#
# Our own subshells inherit our command line and our environment, so a /proc
# scan for a name we are examining will otherwise find US and report every
# candidate live. Walks the ppid chain rather than comparing command lines,
# which is what makes it independent of what the caller was invoked with.
zs_pid_is_ours() {
  local pid="$1" hops=0 stat ppid
  while [ "$hops" -lt 32 ]; do
    # The ROOT test comes FIRST. Every ancestry chain ends at 1 and then 0, so
    # if the self-pid were ever 0 or 1 the match below would fire on the last
    # hop of every walk and declare the entire process table ours - which is a
    # sweeper that silently reclaims nothing, the failure mode that looks like
    # success. Checking the terminals first makes that unreachable.
    { [ "$pid" = "1" ] || [ "$pid" = "0" ]; } && return 1
    [ "$pid" = "${ZS_SWEEP_SELF_PID:-$$}" ] && return 0
    [ -r "/proc/$pid/stat" ] || return 1
    stat="$(cat "/proc/$pid/stat")" || return 1
    # Field 4 is ppid, but field 2 (comm) can contain spaces and parentheses -
    # `(Web Content)`, `(sh -c foo)` - so count from the LAST ')' rather than
    # from the start of the line.
    stat="${stat##*) }"
    ppid="$(printf '%s' "$stat" | cut -d' ' -f2)"
    [ -n "$ppid" ] || return 1
    pid="$ppid"
    hops=$((hops + 1))
  done
  return 1
}

# Scan /proc ONCE for every name in the file `$1`, filling `ZS_SWEEP_HELD_BY`
# with `<name> <pid>` lines for the names something on this box is holding, and
# incrementing `ZS_SWEEP_PROC_UNREADABLE` for each entry it could not read.
#
# The ENVIRONMENT is the half that earns this scan its place. A suite run
# exports PG_TEST_URL=postgres://.../<db>, so the name sits in
# /proc/<pid>/environ from the first line of the script to the last - INCLUDING
# the minutes it spends in cargo with no backend attached at all.
#
# ONE PASS, not one per candidate. The obvious shape - loop candidates, loop
# /proc - is 62 x ~1000 x 2 greps and did not finish in two minutes.
#
# TWO STAGES, because substring and token are different questions.
# `zeroship_auth_test` is a substring of `zeroship_auth_test_s46`, so a
# substring test alone would report the short name held whenever the long one
# is. Stage 1 uses the cheap substring test only to REJECT the processes that
# mention nothing; stage 2 splits the survivors into identifier tokens and
# matches exactly.
zs_sweep_scan_holders() {
  local patterns="$1" d pid candidates="" tokens name
  ZS_SWEEP_HELD_BY=""
  ZS_SWEEP_PROC_UNREADABLE=0

  for d in /proc/[0-9]*; do
    pid="${d#/proc/}"
    # A process of another user, or one that exited between the glob and here.
    # Neither can be a run of OURS holding one of these names, so skipping is
    # correct - but the count is reported, so the blindness is visible rather
    # than assumed.
    if [ ! -r "$d/environ" ] || [ ! -r "$d/cmdline" ]; then
      ZS_SWEEP_PROC_UNREADABLE=$((ZS_SWEEP_PROC_UNREADABLE + 1))
      continue
    fi
    if cat "$d/cmdline" "$d/environ" 2>/dev/null | grep -qaFf "$patterns"; then
      candidates="$candidates $pid"
    fi
  done

  for pid in $candidates; do
    zs_pid_is_ours "$pid" && continue
    [ -r "/proc/$pid/environ" ] || continue
    tokens="$(cat "/proc/$pid/cmdline" "/proc/$pid/environ" 2>/dev/null \
      | tr -c 'a-zA-Z0-9_' '\n' | LC_ALL=C sort -u)"
    while IFS= read -r name; do
      [ -n "$name" ] || continue
      case "
$tokens
" in *"
$name
"*) ZS_SWEEP_HELD_BY="${ZS_SWEEP_HELD_BY}${name} ${pid}
" ;; esac
    done < "$patterns"
  done
}

# The pids holding `$1`, or the empty string.
zs_sweep_holders_of() {
  printf '%s' "${ZS_SWEEP_HELD_BY:-}" | awk -v d="$1" '$1 == d { printf "%s ", $2 }' | sed 's/ $//'
}
