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
#   control    9090               gateway   8000
#   auth       9092               caddy     80
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
# CONTROL IS DIFFERENT AND THIS GATE IS DELIBERATELY RED ON IT. There is no
# Caddy site block for the control plane at all - `api.zeroship.co` proxies to
# `gateway:8000`, not to control. So 9090's publication is the ONLY way a
# creator's `zeroship deploy --control=...` can reach it, and loopback-binding
# it would break the primary creator flow. That is a design gap, not a typo: an
# internet-facing deploy needs a control route at the edge so TLS terminates in
# one place. Until an operator decides that, the exposure is real and this gate
# says so rather than allow-listing it green. See task #186.
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
PASS=0; FAIL=0
pass() { PASS=$((PASS+1)); echo "  ok   $1"; }
fail() { FAIL=$((FAIL+1)); echo "  FAIL $1"; }

echo "============================================"
echo "  deploy/compose publishes only the edge on 0.0.0.0"
echo "============================================"

[ -f "$CF" ] || { echo "  x REFUSED: $CF not found." >&2; exit 1; }

# The ONLY service allowed to publish on all interfaces. Caddy is the edge; that
# is its entire job. Keep this list at one entry - every addition is a second
# way into the platform that does not pass the edge.
EDGE_SERVICE="caddy"

# service + published-port spec, e.g. "gateway 8000:8000".
#
# BLOCK-AWARE ON PURPOSE. My first version of this parser matched any 6-space
# `- "…"` list item holding digits and colons, and it reported FOURTEEN ports
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

echo ""
echo "  $PASS passed, $FAIL failed, $((PASS+FAIL)) ran"

# Floor counts assertions that RAN, not that PASSED: a mutation moves an outcome
# BETWEEN those columns, so only a LOST assertion drops the sum.
# MEASURED 2026-08-11: 8 published ports in the file.
MIN_RAN="${COMPOSE_PORTS_MIN_RAN:-8}"
RAN=$((PASS + FAIL))
rc=0
[ "$FAIL" -eq 0 ] || rc=1
if [ "$RAN" -lt "$MIN_RAN" ]; then
  echo "  x FLOOR: only $RAN published ports checked, expected at least $MIN_RAN." >&2
  echo "    Ports went missing from the parse - a smaller green is not a pass." >&2
  rc=1
fi
exit $rc
