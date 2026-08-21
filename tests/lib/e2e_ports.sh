# shellcheck shell=bash
# ============================================================================
# e2e_ports.sh - a per-run listen port, so two harnesses cannot evict each
# other's server.
#
# WHY THIS EXISTS
# ---------------
# Every e2e harness in this tree picks its listen ports from a fixed band
# written into the script (`ZEROSHIP_CONTROL_PORT=9181`, `:-9137`, and ~40
# more). Two things follow, and both have been measured on this tree rather
# than reasoned about:
#
#   COLLISION. The bands overlap. tests/e2e_stripe_billing.sh:137 and
#   tests/e2e_real_app_end_to_end.sh:52 both name control port 9181;
#   tests/create_demo_invoices.sh:116 and tests/e2e_stripe_webhooks_live.sh:156
#   both name 9182; tests/e2e_dev_vs_deployed_db.sh and
#   tests/e2e_redeploy_replaces_app.sh name the SAME trio 9393/8393/8303 and
#   run in the SAME CI job. Two runs on one box therefore fight over one
#   socket, and the loser reports an unhealthy service - a failure with no
#   connection to the code under test.
#
#   EVICTION, which is worse than collision. Because the ports are fixed, every
#   one of those harnesses opens with some spelling of
#       lsof -ti :$PORT | xargs -r kill -9
#   to clear a process a PREVIOUS run of ITSELF leaked. That line cannot tell a
#   leaked corpse from a peer agent's live control plane fifteen minutes into
#   its own run, so on a shared box it is a remote kill. It is the port-shaped
#   twin of `DROP DATABASE ... WITH (FORCE)`, and tests/lib/scratch_db.sh
#   already carries the database half of the same lesson.
#
# WHY ALLOCATION AND NOT A DERIVED PORT. The alternative considered was
# deriving a port from something stable - the harness name, the branch, the
# migration-set fingerprint that tests/lib/suite_db.sh hashes for database
# names. Every such derivation fails the case that actually happens here: TWO
# AGENTS RUNNING THE SAME HARNESS ON THE SAME COMMIT. A stable derivation
# returns the same number to both by construction, which is the defect, not the
# fix. It is the right answer for a database name - where two runs on one
# commit WANT the same schema and the cost of sharing is zero - and the wrong
# one for a listen socket, where sharing is exclusive.
#
# THE COST OF ALLOCATION IS REPRODUCIBILITY, AND IT IS PAID HERE. A port that
# differs per run cannot be read out of a stale log or guessed by a human
# attaching a debugger. So `zs_ports_reserve` PRINTS every port it hands out,
# on stdout, in the harness's own transcript, next to the variable name that
# carries it. "Which port was control on in that failed run" is answered by
# reading the log rather than by knowing the constant.
#
# THE RESERVATION IS NOT JUST A FREE-PORT PROBE. Asking the kernel whether a
# port is free and then binding it later is a race: two harnesses starting
# together both see 24601 free and both plan to use it. So a port is claimed by
# `mkdir` of a per-port directory under $TMPDIR - atomic across processes on
# one box - and the claim is held for the life of the run. The liveness probe
# runs AFTER the claim, so something already listening on a port nobody claimed
# (a compose stack, a sibling project) is still detected and skipped.
#
# STALE CLAIMS ARE RECLAIMED, AND ONLY ON EVIDENCE OF DEATH. A run killed with
# SIGKILL leaves its directory behind. It is reclaimed only when BOTH the owner
# pid is gone from /proc AND nothing is listening on the port. Either test
# alone is wrong: a pid can be reused, and a live server whose owner shell has
# exited is still a live server.
#
# NOTHING HERE KILLS ANYTHING. That is the point. A harness that allocates its
# ports has nothing to free, so the `lsof | kill -9` line goes away with the
# constant it was protecting.
#
# THE BAND IS 20000-31999, deliberately BELOW Linux's default
# ip_local_port_range (32768-60999): a port in the ephemeral range can be
# handed to an unrelated outgoing connection between our probe and our bind.
# It is also above 10080, the highest entry in the WHATWG bad-ports list that
# crates/runtime/src/web/fetch/bad_ports.rs enforces - a harness whose dev
# server lands on a blocked port fails with a message about fetch, not about
# ports (tests/lib/e2e_stack.sh stack_dev_diagnosis carries that measurement).
#
# tests/lib_e2e_ports_selftest.sh covers both directions of both functions.
# ============================================================================

# Shell globals with a reserved prefix, NOT environment variables: an allocator
# whose bookkeeping is inherited from whatever shell launched it cannot be
# reasoned about from its own invocation.
ZS_PORTS_DIR="${TMPDIR:-/tmp}/zs-e2e-ports"
ZS_PORTS_HELD=""

# Is something accepting connections on 127.0.0.1:<port>?
#
# /dev/tcp rather than lsof: lsof reports sockets owned by processes this user
# can see, so a port held by another user reads as free. A refused connect is
# the kernel's answer and does not depend on who owns the listener.
#
# The connect happens in a SUBSHELL for two reasons, and neither is style. A
# bare `exec 9<>...` in this shell would leave the descriptor open on success,
# and `exec ... 2>/dev/null` in this shell would redirect the HARNESS's stderr
# permanently, because an `exec` with no command applies its redirections to
# the running shell. The subshell owns both, and takes the "Connection refused"
# line - which is the expected answer here, not a fault - with it.
_zs_port_listening() {
  local p="$1"
  if ( exec 9<>"/dev/tcp/127.0.0.1/$p" ) 2>/dev/null; then
    return 0
  fi
  return 1
}

# _zs_port_claim [<lo> <span>]
#
# Claim one free port from [lo, lo+span) and echo it. Returns 1 if every
# candidate it tried was claimed or occupied.
#
# The band is a POSITIONAL ARGUMENT, not a variable this file reads out of the
# environment, so tests/lib_e2e_ports_selftest.sh can pin it to a single port
# and drive the claim/reclaim/occupied arms deterministically. A random band
# would leave those arms testable only by chance, which is the same as not
# testing them.
_zs_port_claim() {
  local lo="${1:-20000}" span="${2:-12000}"
  local tries=0 p d owner
  mkdir -p "$ZS_PORTS_DIR" 2>/dev/null || return 1
  while [ "$tries" -lt 400 ]; do
    tries=$((tries + 1))
    p=$(( lo + (RANDOM % span) ))
    d="$ZS_PORTS_DIR/$p"
    if ! mkdir "$d" 2>/dev/null; then
      # Claimed. Reclaim ONLY on evidence of death - see the header.
      owner="$(cat "$d/owner" 2>/dev/null || true)"
      case "$owner" in
        ''|*[!0-9]*) : ;;
        *)
          if [ ! -d "/proc/$owner" ] && ! _zs_port_listening "$p"; then
            rm -rf "$d" 2>/dev/null || true
          fi
          ;;
      esac
      continue
    fi
    printf '%s\n' "$$" > "$d/owner" 2>/dev/null || { rm -rf "$d" 2>/dev/null; continue; }
    # The claim is ours; now ask whether anything unrelated already holds the
    # socket. Order matters: probing first and claiming second is the race this
    # directory exists to close.
    if _zs_port_listening "$p"; then
      rm -rf "$d" 2>/dev/null || true
      continue
    fi
    printf '%s\n' "$p"
    return 0
  done
  return 1
}

# zs_ports_reserve <VARNAME> [<VARNAME>...]
#
# Assigns each named variable a distinct, claimed, currently-free TCP port,
# exports it (the platform binaries read these names as their ZEROSHIP_*
# configuration twins), and prints the assignment so the run's transcript
# records which socket it used.
#
# Returns 1 - loudly - if a port cannot be found. A harness that silently fell
# back to a constant here would reintroduce exactly the collision this file
# removes, so there is no fallback.
zs_ports_reserve() {
  local var p
  for var in "$@"; do
    if ! p="$(_zs_port_claim)"; then
      echo "FATAL: no free TCP port in 20000-31999 after 400 attempts." >&2
      echo "       Reservations live in $ZS_PORTS_DIR; a run that died hard" >&2
      echo "       leaves one behind, and it is reclaimed only once its owner" >&2
      echo "       pid is gone AND the port is silent." >&2
      return 1
    fi
    printf -v "$var" '%s' "$p"
    # shellcheck disable=SC2163  # exporting by name is the whole point here
    export "$var"
    ZS_PORTS_HELD="$ZS_PORTS_HELD $p"
    echo "  port $var=$p"
  done
  return 0
}

# Release every port THIS shell claimed. Never touches another run's claim.
#
# Called from a cleanup path, so it must not change the exit status: the
# trailing `return 0` is load-bearing for the same reason it is in
# zs_scratch_db_cleanup.
zs_ports_release() {
  local p
  for p in $ZS_PORTS_HELD; do
    rm -rf "${ZS_PORTS_DIR:?}/$p" 2>/dev/null || true
  done
  ZS_PORTS_HELD=""
  return 0
}
