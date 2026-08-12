#!/usr/bin/env bash
# ============================================================================
# Every port deploy/compose publishes must be bound to loopback, EXCEPT the
# edge itself.
#
# WHY THIS EXISTS, found 2026-08-11 while walking the operator deploy (scenario
# 18). The compose file pins its infrastructure to loopback and its PLATFORM
# services to all interfaces, and the asymmetry is easy to read as deliberate
# when it is not:
#
#   verdaccio  127.0.0.1:4873     postgres  127.0.0.1:5440
#   redpanda   127.0.0.1:19092    redpanda  127.0.0.1:9644
#   migrated   127.0.0.1:9091     gateway   127.0.0.1:8000
#   auth       127.0.0.1:9092     caddy     80
#
# `"8000:8000"` with no address publishes on 0.0.0.0. On the internet-facing
# host this deployment targets, that is a second entrance to the gateway that
# does not pass through Caddy - and therefore does not pass through Cloudflare,
# which is where TLS terminates. Same for auth on 9092.
#
# THE PART THAT MAKES IT A REAL FINDING RATHER THAN A STYLE POINT: Caddy does
# NOT use those published ports. It reaches both by SERVICE NAME over the
# compose network - `reverse_proxy gateway:8000` and `reverse_proxy auth:9092`
# in deploy/ops/Caddyfile. So publishing them buys nothing the documented path
# uses, and loopback-binding costs nothing: 127.0.0.1:8000 is still reachable
# from the host, so every local harness and every `curl localhost:8000` keeps
# working. Only REMOTE reach is removed.
#
# Control no longer publishes a host port. Its control.<domain> Caddy block is
# active only when the compose service has no known security-relaxation input.
# The route reaches control over the private compose network.
#
# WHAT THIS DOES NOT CHECK, so a green is not over-read:
#   - it does not run docker and does not probe any host
#   - it says NOTHING about whether a published port is actually reachable from
#     the internet; that depends on the host firewall, and docker's iptables
#     rules commonly bypass ufw. I have not checked the firewall on the target
#     host, so treat this as "published on all interfaces", not "confirmed open"
#   - it does not check EXPOSE or container-to-container reachability, which is
#     the compose network and is unaffected either way
# ============================================================================
set -uo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
CF="$ROOT/deploy/compose/docker-compose.yml"
CADDYFILE="$ROOT/deploy/ops/Caddyfile"
PASS=0; FAIL=0
pass() { PASS=$((PASS+1)); echo "  ok   $1"; }
fail() { FAIL=$((FAIL+1)); echo "  FAIL $1"; }

echo "============================================"
echo "  deploy/compose publishes only the edge on 0.0.0.0"
echo "============================================"

[ -f "$CF" ] || { echo "  x REFUSED: $CF not found." >&2; exit 1; }
[ -f "$CADDYFILE" ] || { echo "  x REFUSED: $CADDYFILE not found." >&2; exit 1; }

# The ONLY service allowed to publish on all interfaces. Caddy is the edge; that
# is its entire job. Keep this list at one entry - every addition is a second
# way into the platform that does not pass the edge.
EDGE_SERVICE="caddy"

# service + published-port spec, e.g. "gateway 8000:8000".
#
# BLOCK-AWARE ON PURPOSE. My first version of this parser matched any 6-space
# `- "..."` list item holding digits and colons, and it reported FOURTEEN ports
# where the file has eight - inventing `worker publishes 4`, `200`, `3` and
# `redpanda publishes 1` out of quoted scalars in `command:` and healthcheck
# blocks. That is a gate that lies, so it is not what ships. Entries are taken
# ONLY from inside a service's `ports:` block, which ends at the next key at the
# same indent.
mapfile -t ENTRIES < <(
  awk '
    /^  [a-z][a-z0-9_-]*:[[:space:]]*$/ { svc = $1; sub(/:$/, "", svc); inports = 0; next }
    /^    ports:[[:space:]]*$/          { inports = 1; next }
    /^    [a-z_]+:/                     { inports = 0 }
    inports && /^      - "/             { spec = $2; gsub(/"/, "", spec); print svc, spec }
  ' "$CF"
)

if [ "${#ENTRIES[@]}" -eq 0 ]; then
  echo "  x REFUSED: parsed ZERO published ports out of $CF." >&2
  echo "    Either the file changed shape or this parser is broken; a gate that" >&2
  echo "    checks nothing must not report success." >&2
  exit 1
fi

for e in "${ENTRIES[@]}"; do
  svc="${e%% *}"; spec="${e#* }"
  if [ "$svc" = "$EDGE_SERVICE" ]; then
    pass "$svc publishes $spec (the edge, expected on all interfaces)"
  elif [[ "$spec" == 127.0.0.1:* ]]; then
    pass "$svc publishes $spec (loopback)"
  else
    fail "$svc publishes $spec on ALL INTERFACES; it is not the edge, so this is a second entrance that bypasses Caddy and Cloudflare"
  fi
done

PORT_RAN=$((PASS + FAIL))

# This is separate from the eight-port inventory: it couples activation of the
# staged public control route to removal of both known insecure compose inputs.
# Commented Caddy examples do not match the anchored active-site expression.
CONTROL_BLOCK=$(awk '
  /^  control:[[:space:]]*$/ { in_control = 1; next }
  in_control && /^  [a-z][a-z0-9_-]*:[[:space:]]*$/ { exit }
  in_control && !/^[[:space:]]*#/ { print }
' "$CF")
CONTROL_ROUTE_ACTIVE=0
CONTROL_POSTURE_RELAXED=0
CONTROL_ROUTE_STAGED=0
if grep -Eq '^[[:space:]]*#[[:space:]]*http://control[.]\{\$ZEROSHIP_DOMAIN(:[^}]*)?\}[[:space:]]*\{' \
    "$CADDYFILE" && \
   grep -Eq '^[[:space:]]*#[[:space:]]*reverse_proxy[[:space:]]+control:9090([[:space:]]|$)' \
    "$CADDYFILE"; then
  CONTROL_ROUTE_STAGED=1
fi
grep -Eq '^[[:space:]]*(https?://)?control[.]\{\$ZEROSHIP_DOMAIN(:[^}]*)?\}[[:space:]]*\{' \
  "$CADDYFILE" && CONTROL_ROUTE_ACTIVE=1
grep -q -- '--dev-insecure' <<<"$CONTROL_BLOCK" && CONTROL_POSTURE_RELAXED=1
grep -Eq '^[[:space:]]*ZEROSHIP_CONTROL_KEY:[[:space:]]*platform-key([[:space:]]|$)' \
  <<<"$CONTROL_BLOCK" && CONTROL_POSTURE_RELAXED=1

if [ "$CONTROL_POSTURE_RELAXED" -eq 1 ]; then
  if [ "$CONTROL_ROUTE_STAGED" -eq 1 ] && [ "$CONTROL_ROUTE_ACTIVE" -eq 0 ]; then
    pass "control route is staged while compose has an insecure input"
  else
    fail "control route must remain staged and inactive while compose has an insecure input"
  fi
elif [ "$CONTROL_ROUTE_ACTIVE" -eq 1 ]; then
  pass "control route is active after insecure compose inputs were removed"
else
  fail "control route stayed inactive after insecure compose inputs were removed"
fi

if [ "$CONTROL_ROUTE_ACTIVE" -eq 1 ] && [ "$CONTROL_POSTURE_RELAXED" -eq 1 ]; then
  fail "control route is active while control still has an insecure compose input"
else
  pass "control route state does not expose a relaxed control service"
fi

echo ""
echo "  $PASS passed, $FAIL failed, $((PASS+FAIL)) ran"

# Floor counts assertions that RAN, not that PASSED: a mutation moves an outcome
# BETWEEN those columns, so only a LOST assertion drops the sum.
# MEASURED 2026-08-12: 8 published ports in the file after removing control's
# host publication.
MIN_RAN="${COMPOSE_PORTS_MIN_RAN:-8}"
rc=0
[ "$FAIL" -eq 0 ] || rc=1
if [ "$PORT_RAN" -lt "$MIN_RAN" ]; then
  echo "  x FLOOR: only $PORT_RAN published ports checked, expected at least $MIN_RAN." >&2
  echo "    Ports went missing from the parse - a smaller green is not a pass." >&2
  rc=1
fi
exit $rc
