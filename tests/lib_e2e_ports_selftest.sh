#!/usr/bin/env bash
# Self-test for tests/lib/e2e_ports.sh.
#
# The library exists so two harnesses on one box cannot fight over one listen
# socket, and so neither has any reason to kill whatever holds a port. That is
# only worth something if it is right in BOTH directions:
#
#   a claimed port MUST NOT be handed out again   - else two runs bind the same
#                                                   socket and the second one
#                                                   reports an unhealthy
#                                                   service that has nothing to
#                                                   do with the code under test
#   an OCCUPIED port MUST NOT be handed out       - the claim directory only
#                                                   knows about runs that use
#                                                   this library; a compose
#                                                   stack or a sibling project
#                                                   holding a socket is invisible
#                                                   to it and must be detected
#   a DEAD run's claim MUST be reclaimed          - else the band leaks one port
#                                                   per hard-killed run until it
#                                                   is exhausted
#   a LIVE run's claim MUST NOT be reclaimed      - which is the whole defect
#                                                   this file exists to remove:
#                                                   "the owner looks idle" is
#                                                   how `lsof | kill -9` shoots
#                                                   a peer agent's control plane
#
# THE BAND IS PINNED TO ONE PORT for the deterministic arms. `_zs_port_claim`
# takes it as a positional argument for exactly this reason: over the real
# 12000-port band, "the allocator skipped the occupied port" and "the allocator
# never tried the occupied port" print the same thing.
#
# Run directly: tests/lib_e2e_ports_selftest.sh
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
LIB="$ROOT/tests/lib/e2e_ports.sh"

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

# Point the library at a private reservation root so this selftest can neither
# see nor disturb a real run's claims. TMPDIR is read once, at source time.
export TMPDIR="$TMP"
# shellcheck source=tests/lib/e2e_ports.sh
. "$LIB"

fail=0
pass=0
ok()  { pass=$((pass + 1)); echo "ok   - $1"; }
bad() { fail=$((fail + 1)); echo "FAIL - $1" >&2; }
check() { if [ "$2" = "$3" ]; then ok "$1 ($3)"; else bad "$1: expected '$2', got '$3'"; fi; }

# A port in the pinned band that nothing on this box is using. Found by asking,
# not by assuming: a hard-coded constant here would make every arm below depend
# on what else happens to be running.
PINNED=""
for cand in $(seq 23100 23180); do
  if ! (exec 9<>"/dev/tcp/127.0.0.1/$cand") 2>/dev/null; then PINNED="$cand"; break; fi
done
if [ -z "$PINNED" ]; then
  echo "CANNOT RUN: no free port in 23100-23180 to pin the deterministic arms to." >&2
  exit 2
fi
echo "=== pinned band: single port $PINNED ==="

echo
echo "=== a reserved port is claimed, exported, and printed ==="
# Redirected to a file rather than captured: a command substitution runs in a
# subshell, so the variable the library exports would never reach this shell
# and every arm below would read an empty string.
zs_ports_reserve ZS_T_ALPHA > "$TMP/alpha.out" 2>&1
check "reserve succeeded" "0" "$?"
out="$(cat "$TMP/alpha.out")"
if [ -n "${ZS_T_ALPHA:-}" ] && [ "$ZS_T_ALPHA" -ge 20000 ] && [ "$ZS_T_ALPHA" -lt 32000 ]; then
  ok "assigned a port in the band ($ZS_T_ALPHA)"
else
  bad "assigned '${ZS_T_ALPHA:-}' which is not in 20000-31999"
fi
case "$out" in
  *"ZS_T_ALPHA=$ZS_T_ALPHA"*) ok "the assignment was printed, so a log records it" ;;
  *) bad "reserve printed '$out', which does not name the port it handed out" ;;
esac
if [ -d "$ZS_PORTS_DIR/$ZS_T_ALPHA" ]; then
  ok "the port is claimed on disk"
else
  bad "no claim directory for $ZS_T_ALPHA; a concurrent run could take it"
fi
check "the claim records this shell as owner" "$$" "$(cat "$ZS_PORTS_DIR/$ZS_T_ALPHA/owner")"

echo
echo "=== two ports in one call are distinct ==="
zs_ports_reserve ZS_T_B ZS_T_C >/dev/null
if [ "$ZS_T_B" != "$ZS_T_C" ]; then ok "$ZS_T_B != $ZS_T_C"; else bad "both vars got $ZS_T_B"; fi

echo
echo "=== a LIVE claim is never handed out (band pinned to one port) ==="
mkdir -p "$ZS_PORTS_DIR/$PINNED"
printf '%s\n' "$$" > "$ZS_PORTS_DIR/$PINNED/owner"   # this shell is alive
got="$(_zs_port_claim "$PINNED" 1)"; rc=$?
check "claim refuses the only port in the band" "1" "$rc"
check "and hands out nothing" "" "$got"
if [ -d "$ZS_PORTS_DIR/$PINNED" ]; then
  ok "the live claim survived"
else
  bad "the live claim was reclaimed - this is the peer-eviction defect"
fi

echo
echo "=== a DEAD run's claim IS reclaimed ==="
# A pid that is certainly not running: spawn one and reap it.
( exit 0 ) & dead_pid=$!; wait "$dead_pid" 2>/dev/null
printf '%s\n' "$dead_pid" > "$ZS_PORTS_DIR/$PINNED/owner"
got="$(_zs_port_claim "$PINNED" 1)"; rc=$?
check "claim succeeds over a dead owner" "0" "$rc"
check "and returns the pinned port" "$PINNED" "$got"
check "the claim now names this shell" "$$" "$(cat "$ZS_PORTS_DIR/$PINNED/owner")"

echo
echo "=== an OCCUPIED port is refused even when its claim is free ==="
# The one-variable partner of the arm above: SAME port, SAME free claim
# directory, the ONLY difference being that something is listening.
rm -rf "${ZS_PORTS_DIR:?}/$PINNED"
node -e 'require("net").createServer().listen(Number(process.argv[1]),"127.0.0.1",()=>console.log("up"))' "$PINNED" \
  > "$TMP/listener.log" 2>&1 &
listener=$!
for _ in $(seq 1 50); do grep -q up "$TMP/listener.log" 2>/dev/null && break; sleep 0.1; done
if ! grep -q up "$TMP/listener.log" 2>/dev/null; then
  bad "could not stand a listener up on $PINNED; the occupied arm did not run"
else
  got="$(_zs_port_claim "$PINNED" 1)"; rc=$?
  check "claim refuses an occupied port" "1" "$rc"
  check "and hands out nothing" "" "$got"
  if [ -d "$ZS_PORTS_DIR/$PINNED" ]; then
    bad "the refused port was left claimed; the band would leak one port per probe"
  else
    ok "the claim was released again after the port turned out to be busy"
  fi
fi
kill "$listener" 2>/dev/null || true
wait "$listener" 2>/dev/null || true

echo
echo "=== THE CASE THAT MATTERS: two concurrent runs get disjoint ports ==="
# Two separate PROCESSES, started together, each reserving three ports - the
# shape of two agents launching two harnesses on one box. Same-process
# uniqueness (checked above) says nothing about this: the bookkeeping that has
# to hold is on disk, not in one shell's variables.
cat > "$TMP/racer.sh" <<EOF
set -uo pipefail
export TMPDIR="$TMP"
. "$LIB"
zs_ports_reserve P1 P2 P3 >/dev/null || exit 1
printf '%s %s %s\n' "\$P1" "\$P2" "\$P3"
EOF
: > "$TMP/race.out"
for i in $(seq 1 8); do bash "$TMP/racer.sh" >> "$TMP/race.out" 2>&1 & done
wait
n_ports=$(tr ' ' '\n' < "$TMP/race.out" | grep -c '^[0-9]\+$')
n_uniq=$(tr ' ' '\n' < "$TMP/race.out" | grep '^[0-9]\+$' | sort -u | wc -l | tr -d ' ')
check "8 concurrent runs each got 3 ports" "24" "$n_ports"
check "and every one of them is distinct" "$n_ports" "$n_uniq"

echo
echo "=== release drops only this shell's claims ==="
mkdir -p "$ZS_PORTS_DIR/$PINNED"; printf '99999999\n' > "$ZS_PORTS_DIR/$PINNED/owner"
held_before="$(printf '%s' "$ZS_PORTS_HELD" | wc -w | tr -d ' ')"
zs_ports_release
check "release returns 0 so it cannot rewrite a green exit status" "0" "$?"
if [ "$held_before" -lt 1 ]; then
  bad "this shell held $held_before port(s), so release ruled on nothing"
else
  ok "release ruled on $held_before claim(s) this shell held"
fi
if [ -d "$ZS_PORTS_DIR/$PINNED" ]; then
  ok "a claim belonging to another owner was left alone"
else
  bad "release deleted a claim this shell did not make"
fi
if [ -d "$ZS_PORTS_DIR/$ZS_T_ALPHA" ]; then
  bad "release left this shell's own claim $ZS_T_ALPHA behind"
else
  ok "this shell's own claim was released"
fi

echo
echo "=================================================================="
echo "e2e ports selftest: ${pass} passed, ${fail} failed"
[ "$fail" -eq 0 ] || exit 1
