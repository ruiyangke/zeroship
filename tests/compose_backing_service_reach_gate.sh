#!/usr/bin/env bash
# ============================================================================
# Every backing service deploy/compose RUNS must be one that some compose
# service is CONFIGURED TO REACH.
#
# THE DEFECT THIS EXISTS FOR, found 2026-08-20 and written up in
# docs/proposals/2026-08-20-metering-transport-not-configured.md. Compose
# declares `redpanda`, healthchecks it, gives it a persistent volume, and
# deploy/scripts/deploy-remote.sh runs `docker compose up -d`, so ON THE
# DEPLOYED HOST THE BROKER RUNS. Nothing publishes to it and nothing consumes
# from it: `grep METERING_BROKERS deploy/compose/docker-compose.yml` returns two
# hits and both are comments. The worker and gateway producers therefore take
# `build_usage_outbox`'s `Ok(None)` arm and drain-and-drop every usage event,
# control never spawns the forwarder or spend recompute, no app is ever
# invoiced, and the spend ladder cannot fire because spend is structurally zero.
# That had been true for 43 days when it was found - since `redpanda` entered
# compose in c30c3baac (2026-07-08).
#
# THE SHAPE, and it is why a running broker is worse than an absent one:
#
#   a broker nobody publishes to
#     == a deployment with no broker
#     == an app that served no traffic
#
# Three states, one appearance. `docker compose ps` says healthy, /readyz says
# ready, the creator's usage endpoint says zero, and the only signal is a WARN
# line every 10 seconds in a service that logs at info. The comment beside the
# service even asserts the wiring exists ("In compose that value is
# redpanda:9092"), beside a file where that value appears nowhere.
#
# SCOPE: deploy/compose/docker-compose.yml, because that is what ships.
# deploy/scripts/deploy-remote.sh:356 snapshots exactly
# `compose/.env compose/docker-compose.yml ops/Caddyfile ops/zeroship.toml`
# and runs compose against it. The other three files under deploy/compose/
# (cluster.yml, lago.yml, openmeter.yml) are opt-in test stacks with their own
# compose projects and are deliberately not inspected. A path may be passed as
# argv to point this at a scratch copy; CI passes none.
#
# WHAT THIS DOES NOT CATCH, so a green is not over-read. This gate rules on
# CONFIGURATION, and configuration is a PROXY for the thing that actually
# matters, which is that usage flows. The gap between the two is not small:
#
#   - A service configured with a broker address that never publishes a single
#     event passes here. The proxy is "is this service told how to reach that
#     one", not "did a record land". This gate goes green the moment somebody
#     adds one `environment:` line, with no event transported, ever. If the
#     producer is broken, misconfigured downstream, or the topic name is wrong,
#     this gate cannot tell and will not try.
#   - It reads text. It does not run docker, does not resolve compose DNS, does
#     not dial a port, and does not know whether the address is correct - only
#     that the service name appears as a host in another service's config.
#     `ZEROSHIP_METERING_BROKERS: redpanda:9999` passes.
#   - The reach predicate needs `<service>:<port>`. A setting that names a host
#     with no port, an IP literal, a network alias, or a value that arrives
#     through a mounted credential FILE is invisible to it. That last one is
#     live today, not hypothetical: `migrate` reaches postgres through
#     /etc/zeroship/secrets/migrate-dsn and scores no edge here. It is only
#     harmless because postgres has five other consumers - if brokers ever
#     become a credential (the open question in #108), the same blindness would
#     apply to redpanda and this gate would report a false red.
#   - `depends_on` is NOT counted; see the block above ORDERING_IS_NOT_REACH.
#   - It says nothing about whether the broker, database or cache is correctly
#     sized, reachable, healthy, or backed up.
#
# The honest claim is narrow: this gate distinguishes "compose runs a backing
# service that no service was told about" from "compose runs a backing service
# that at least one service was told about". Tonight that is exactly the
# distinction nothing in the tree could make.
#
# IT IS RED ON THE COMMIT THAT ADDS IT, deliberately, and it is deliberately
# NOT YET A STEP IN .github/workflows/ci.yml. Redpanda is genuinely unreached,
# so a correct gate fails here today; softening it to land green would have made
# it a gate that passed for the whole 43 days the gap existed. CI enumerates
# gate steps by hand (one `run:` per gate) and tests/ci_invocable_gate.sh reads
# CI to find scripts, never the reverse, so an unwired gate breaks nothing -
# while `gate-arm-census -- tests` globs this directory and does pick this file
# up, statically, which is why the arm contract below is satisfied from the
# first commit. WIRE THIS INTO ci.yml IN THE SAME COMMIT THAT CONFIGURES THE
# BROKER, and not before: a red required step on unrelated work teaches people
# to ignore it, which is a worse outcome than the gap it reports.
# ============================================================================
set -uo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
CF="${1:-$ROOT/deploy/compose/docker-compose.yml}"
PASS=0; FAIL=0
pass() { PASS=$((PASS+1)); echo "  ok   $1"; }
fail() { FAIL=$((FAIL+1)); echo "  FAIL $1"; }

# Per-arm anti-vacuity accounting (tests/lib/gate_arms.sh).
# shellcheck source=tests/lib/gate_arms.sh
. "$ROOT/tests/lib/gate_arms.sh"
gate_arms_init compose_backing_service_reach

echo "============================================"
echo "  every backing service in deploy/compose is one somebody is configured to reach"
echo "============================================"
echo "  compose file: $CF"

[ -f "$CF" ] || { echo "  x REFUSED: $CF not found." >&2; exit 1; }

# ---------------------------------------------------------------------------
# 1. Enumerate the services.
#
# Anchored on the top-level `services:` key and released at the next top-level
# key, because the top-level `volumes:` block's entries have the same two-space
# shape and would otherwise be read as services.
# ---------------------------------------------------------------------------
mapfile -t SERVICES < <(
  awk '/^services:[[:space:]]*$/ { s = 1; next }
       s && /^[a-z]/            { s = 0 }
       s && /^  [a-z][a-z0-9_-]*:[[:space:]]*$/ { n = $1; sub(/:$/, "", n); print n }' "$CF"
)
N_SERVICES=${#SERVICES[@]}
# MEASURED 2026-08-21: 11 services in the tracked compose file. Everything
# below is derived from this list, so a parser that lost the `services:` anchor
# would enumerate nothing, find no backing services, rule on nothing, and print
# a clean green. Floor well under 11: the stack would have to shed more than
# half its services to reach it, while the failure guarded against drops it to 0.
if ! gate_arm compose_services "$N_SERVICES" 5; then
  echo "  x REFUSED: parsed too few services out of $CF." >&2
  echo "    A gate that checks nothing must not report success." >&2
  exit 1
fi

# The literal body of one service: from its key to the next two-space key or
# the next top-level key.
#
# CACHED IN A VARIABLE, and every test below reads the variable rather than a
# pipeline, because `producer | grep -q ...` under `set -o pipefail` is a RACE.
# `grep -q` exits at its first match and closes the pipe; whether the producer
# has already finished writing decides whether it dies of SIGPIPE, and if it
# does, pipefail hands the whole pipeline a non-zero status even though the
# match succeeded. My first version of this gate did exactly that and dropped
# `control` out of the built-service list on some runs and not others - a gate
# whose findings depend on pipe buffering is worse than no gate.
svc_body() {
  awk -v s="  $1:" '
    $0 == s                                  { f = 1; next }
    f && /^  [a-z][a-z0-9_-]*:[[:space:]]*$/ { f = 0 }
    f && /^[a-z]/                            { f = 0 }
    f                                        { print }' "$CF"
}
declare -A BODY
for svc in "${SERVICES[@]}"; do BODY["$svc"]="$(svc_body "$svc")"; done

# ---------------------------------------------------------------------------
# 2. WHAT COUNTS AS "CONFIGURED TO REACH IT" - the predicate, and the parts of
#    a service body it is deliberately NOT allowed to see.
#
# Established by reading how postgres and redis are wired TODAY, since a
# predicate that does not cover the working cases is bound to a spelling rather
# than to the hazard. All four live forms are `<service-name>:<port>` inside a
# value:
#
#   control    ZEROSHIP_CONTROL_DATABASE_URL: ...@postgres:5432/zeroship
#   worker     ZEROSHIP_WORKER_KV_URL:        redis://redis:6379
#   gateway    command:  --control-url http://control:9090
#   control    ZEROSHIP_CONTROL_MIGRATED_URL: ${...:-http://migrated:9091}
#
# so the predicate is the service name used as a host with a port, whatever
# precedes it (`@`, `//`, `=`, a space). An `env_file:` value would be scanned
# the same way; there is no env_file directive anywhere under deploy/ today.
#
# EXCLUDED BLOCKS, each for a reason that has a false positive behind it:
#   image:        `image: postgres:16` is the postgres NAME beside a NUMBER and
#                 matches the predicate exactly. It is not reach.
#   volumes:      `../ops/postgres-init.sql:/docker-entrypoint-initdb.d/...`
#                 likewise.
#   healthcheck:  a self-probe (`pg_isready -U postgres`), never reach.
#   ports:        publication, not reach - and `127.0.0.1:19092` is how the
#                 HOST talks to redpanda, which is not a compose service.
#   networks:     aliases, which are names for other services to use, not uses.
#   depends_on:   see ORDERING_IS_NOT_REACH below.
#   build:, deploy:, security_opt:  no host ever appears there.
# Comment lines are stripped first: the two METERING_BROKERS mentions in this
# file are both comments, and counting a comment as configuration is precisely
# how the redpanda gap stayed invisible.
#
# ORDERING_IS_NOT_REACH. `depends_on` is start ordering and health gating, not
# configuration, and this file settles the question rather than arguing it:
#   - it is not SUFFICIENT. Nothing depends_on redpanda today, so counting it
#     would not change tonight's verdict - but it WOULD make this gate go green
#     the day somebody adds `depends_on: redpanda` for start ordering, with no
#     address configured and no event transported.
#   - it is not NECESSARY. `migrate` depends_on postgres AND genuinely reaches
#     it; `gateway` depends_on worker AND genuinely reaches it. Every real
#     consumer in this file that has a depends_on entry also carries an address.
# So depends_on adds no true positive here and admits an obvious false one.
# ---------------------------------------------------------------------------
config_region() {
  printf '%s\n' "${BODY[$1]}" | awk '
    /^[[:space:]]*#/ { next }
    /^    [a-z_]+:/ {
      key = $1; sub(/:.*$/, "", key)
      skip = (key == "depends_on" || key == "volumes" || key == "healthcheck" \
           || key == "ports"      || key == "networks" || key == "build" \
           || key == "image"      || key == "deploy"   || key == "security_opt")
    }
    !skip { print }'
}
declare -A CFG
for svc in "${SERVICES[@]}"; do CFG["$svc"]="$(config_region "$svc")"; done

reaches() {  # reaches <consumer> <target>
  grep -qE "(^|[^A-Za-z0-9_.-])${2}:[0-9]" <<<"${CFG[$1]}"
}

# ---------------------------------------------------------------------------
# 3. THE CONTROL ARM. Scan every ordered pair and count the reach edges found
#    ANYWHERE in the file, not only the ones involving a backing service.
#
# This is the arm that separates "redpanda is genuinely unreached" from "my
# config-region parser broke". Those two produce opposite-looking output - a
# broken parser reports every service unreached, which is loud - but a loud
# wrong answer is still a wrong answer, and the one number that tells them
# apart is how many edges the same scanner found elsewhere in the same file.
# ---------------------------------------------------------------------------
declare -A CONSUMERS_OF
N_EDGES=0
for target in "${SERVICES[@]}"; do
  who=""
  for consumer in "${SERVICES[@]}"; do
    [ "$consumer" = "$target" ] && continue
    if reaches "$consumer" "$target"; then
      who="$who $consumer"
      N_EDGES=$((N_EDGES + 1))
    fi
  done
  CONSUMERS_OF["$target"]="${who# }"
done
# MEASURED 2026-08-21: 12 reach edges across the tracked file (postgres 5,
# control 2, auth 2, migrated 1, worker 1, redis 1). Floor well under that:
# ordinary edits add or drop one address, while a config_region whose block
# filter or body extraction broke collapses this to 0 - and without this arm the
# gate would then announce all three backing services unreached, naming postgres
# and redis, which is a confident finding about a file it could no longer read.
# Verified by running this gate against a compose file carrying services with no
# config at all: it refuses here rather than reporting three.
if ! gate_arm reach_edges "$N_EDGES" 4; then
  echo "  x REFUSED: the reach scanner found almost no configured addresses in" >&2
  echo "    $CF. It is far likelier that this parser broke than that the stack" >&2
  echo "    stopped wiring itself together; a finding produced by a blind" >&2
  echo "    instrument is not a finding." >&2
  exit 1
fi

# ---------------------------------------------------------------------------
# 4. WHAT COUNTS AS A "BACKING SERVICE" - derived, then classified exhaustively.
#
# DERIVED: a service this repository does not BUILD. Six services carry a
# `build:` stanza against deploy/Dockerfile - the five native servers plus the
# one-shot `migrate`, all sharing one built image and differing only by
# `command:`. Everything WITHOUT one runs somebody else's image and is
# infrastructure the platform sits on top of.
# That is a property of the file, so a new third-party service is picked up the
# day it lands. A hardcoded `postgres redis redpanda` list would not be: it is a
# census, and a census cannot contain a service nobody has added yet.
#
# CLASSIFIED: not every third-party image is a backing service, so two roles
# below name the ones that are not. THERE IS NO UNCLASSIFIED STATE: a candidate
# named by neither role is a BACKEND and must be reached. That default is
# fail-closed on purpose, and it is the anti-rot property - a third-party
# service added tomorrow is presumed to be something the platform dials, and
# turns this gate red until either a service is configured to reach it or
# somebody states what it is instead.
#
# The roles are not an allowlist. An allowlist is silent about what it excuses;
# each role here is an ASSERTION THAT IS CHECKED, is counted in the ran total,
# and can fail on its own. A role naming a service that is no longer a non-built
# service in the file is a refusal, checked below.
#
#   edge       entered from OUTSIDE the compose network, so nothing inside
#              names it. Checked: must publish a port on all interfaces.
#   host-tool  consumed by the HOST over a published loopback port, not by any
#              compose service. Checked: must publish a loopback port AND must
#              have zero in-compose consumers - the day a service is configured
#              to reach it, it has become a backend, and this goes red saying so
#              rather than quietly continuing to excuse it.
#
# Everything else is a BACKEND and must have at least one consumer.
#
# WHY VERDACCIO IS A HOST-TOOL AND NOT AN UNREACHED BACKEND, since that is the
# one judgement call here: docs/runbooks/private-registry.md publishes packages
# to it from the host through 127.0.0.1:4873 and, where an in-network route is
# wanted, tells the operator to attach the container to the SANDBOX project's
# network by hand. No service in this file is meant to dial it. Redpanda is the
# opposite in every respect: it advertises an in-compose address
# (`internal://redpanda:9092`), the producers have a setting whose documented
# compose value is exactly that address, and no service carries it.
# ---------------------------------------------------------------------------
EDGE_SERVICES="caddy"
HOST_TOOL_SERVICES="verdaccio"

CANDIDATES=()
PLATFORM_SERVICES=""
for svc in "${SERVICES[@]}"; do
  if grep -qE '^    build:' <<<"${BODY[$svc]}"; then
    PLATFORM_SERVICES="$PLATFORM_SERVICES $svc"
  else
    CANDIDATES+=("$svc")
  fi
done
PLATFORM_SERVICES="${PLATFORM_SERVICES# }"
N_CANDIDATES=${#CANDIDATES[@]}
# MEASURED 2026-08-21: 5 services run a third-party image (verdaccio, postgres,
# redis, redpanda, caddy); the other 6 share deploy/Dockerfile. Floor well under
# 5, because the collapse this guards against is total: a `build:` test that
# matched everything leaves 0 candidates, 0 backends, 0 verdicts and a green.
# The opposite break - matching nothing, so all 11 are candidates - is caught
# instead by the unclassified-candidate refusal below, which names each one.
if ! gate_arm infra_candidates "$N_CANDIDATES" 2; then
  echo "  x REFUSED: derived too few non-built services out of $CF." >&2
  exit 1
fi

# A role naming a service that no longer exists is the census failure wearing
# the uniform of the fix: it excuses nothing, silently, forever. Rule on it.
for declared in $EDGE_SERVICES $HOST_TOOL_SERVICES; do
  found=0
  for svc in "${CANDIDATES[@]}"; do [ "$svc" = "$declared" ] && found=1; done
  if [ "$found" -eq 0 ]; then
    echo "  x REFUSED: role declared for '$declared', which is not a non-built" >&2
    echo "    service in $CF. Either it was renamed or removed, or it now has a" >&2
    echo "    build: stanza. Update the role lines in this gate in the same" >&2
    echo "    commit; a role pointing at nothing exempts nothing and hides it." >&2
    exit 1
  fi
done

# ---------------------------------------------------------------------------
# 5. Rule on every candidate. Exactly one verdict each, no silent skips.
# ---------------------------------------------------------------------------
VERDICTS=0
BACKENDS=0

# Published host ports of one service, block-aware: entries come ONLY from
# inside that service's `ports:` block. compose_port_exposure_gate.sh records
# what happens without that anchor - a parser that matched any quoted list item
# holding digits and colons invented six ports out of `command:` scalars.
published_ports() {
  printf '%s\n' "${BODY[$1]}" | awk '
    /^    ports:[[:space:]]*$/ { p = 1; next }
    /^    [a-z_]+:/            { p = 0 }
    p && /^      - "/          { spec = $2; gsub(/"/, "", spec); print spec }'
}
# The empty-specs guard is load-bearing, not defensive noise: a here-string of
# the empty string is ONE EMPTY LINE, which `grep -v 127.0.0.1` matches. Without
# it, a service that publishes no port at all would be reported as publishing on
# all interfaces - the strongest possible claim, drawn from no evidence.
publishes_on_all_interfaces() {
  local specs; specs="$(published_ports "$1")"
  [ -n "$specs" ] || return 1
  grep -qv '^127\.0\.0\.1:' <<<"$specs"
}
publishes_on_loopback() {
  local specs; specs="$(published_ports "$1")"
  [ -n "$specs" ] || return 1
  grep -q '^127\.0\.0\.1:' <<<"$specs"
}

for svc in "${CANDIDATES[@]}"; do
  consumers="${CONSUMERS_OF[$svc]}"
  n=0
  for _ in $consumers; do n=$((n + 1)); done

  case " $EDGE_SERVICES " in
    *" $svc "*)
      VERDICTS=$((VERDICTS + 1))
      if publishes_on_all_interfaces "$svc"; then
        pass "$svc is the edge (publishes on all interfaces); reached from outside, not from inside"
      else
        fail "$svc is declared the edge but publishes nothing on all interfaces - either the role is wrong or the edge stopped being the entrance"
      fi
      continue ;;
  esac

  case " $HOST_TOOL_SERVICES " in
    *" $svc "*)
      VERDICTS=$((VERDICTS + 1))
      if [ "$n" -ne 0 ]; then
        fail "$svc is declared a host-tool but is now configured to be reached by:$consumers - it is a backend; move it out of HOST_TOOL_SERVICES"
      elif publishes_on_loopback "$svc"; then
        pass "$svc is a host-tool (loopback port for the host; no compose service dials it)"
      else
        fail "$svc is declared a host-tool but publishes no loopback port, so the host cannot reach it either - nothing uses this service"
      fi
      continue ;;
  esac

  BACKENDS=$((BACKENDS + 1))
  VERDICTS=$((VERDICTS + 1))
  if [ "$n" -gt 0 ]; then
    pass "$svc is reached by $n service(s):$consumers"
  else
    fail "$svc RUNS AND NOBODY IS CONFIGURED TO REACH IT"
    {
      echo "         compose declares '$svc', starts it, healthchecks it and gives it a"
      echo "         volume, and no service names '$svc:<port>' in its command:,"
      echo "         environment: or env_file:. A backing service nobody publishes to is"
      echo "         indistinguishable from an absent one and from an idle platform."
      echo "         The services that could reach it are the ones this repo builds:"
      echo "           $PLATFORM_SERVICES"
      echo "         Add the address the way the backing services that PASS above are"
      echo "         wired - an environment: entry on each consumer whose value names"
      echo "         the service as a host, e.g. worker's"
      echo "           ZEROSHIP_WORKER_KV_URL: redis://redis:6379"
      echo "         For redpanda specifically the producers are the worker and the"
      echo "         gateway (metering.brokers / ZEROSHIP_METERING_BROKERS) and the"
      echo "         consumer is control (control.stream_transport); see"
      echo "         docs/proposals/2026-08-20-metering-transport-not-configured.md."
      echo "         Otherwise delete the service: running it costs memory, a volume"
      echo "         and a healthcheck, and buys a comment that says it is configured."
    } >&2
  fi
done

echo ""
echo "  $PASS passed, $FAIL failed, $((PASS+FAIL)) ran"

rc=0
[ "$FAIL" -eq 0 ] || rc=1

# Counts verdicts that RAN, not that PASSED: a mutation moves an outcome
# BETWEEN those columns, so only a LOST verdict drops the sum.
#
# BOUND TO THE CORPUS THIS RUN WAS HANDED, not to a number measured once. The
# expectation is the candidate count from arm `infra_candidates`, so a candidate
# that fell through every branch - the failure where a check quietly filters its
# awkward cases out of its own expectation until the comparison always balances
# - shows up as a mismatch rather than as a smaller green.
if [ "$VERDICTS" -ne "$N_CANDIDATES" ]; then
  echo "  x COUNT: $VERDICTS verdict(s) for $N_CANDIDATES candidate(s)." >&2
  echo "    A candidate reached no branch, so nothing was said about it." >&2
  rc=1
fi
if [ "$((PASS + FAIL))" -ne "$N_CANDIDATES" ]; then
  echo "  x COUNT: $((PASS + FAIL)) assertion(s) for $N_CANDIDATES candidate(s)." >&2
  rc=1
fi

# MEASURED 2026-08-21: 5 candidates get a verdict - 3 backends (postgres,
# redis, redpanda) plus 2 role assertions (caddy, verdaccio). This number is
# equal to arm `infra_candidates` BY ASSERTION, checked immediately above, and
# that equality is the point: one arm counts what was enumerated, the other what
# was ruled on, and the gate is red when they differ. Floor well under 5.
gate_arm infra_verdicts "$VERDICTS" 3 || rc=1

# MEASURED 2026-08-21: 3 backends. Floor 2, which postgres and redis clear on
# their own, so this arm cannot go vacuous while the platform still has a
# database and a cache. Deleting redpanda is a legitimate way to fix this gate
# and leaves 2, which still clears. Dropping below 2 means the role lines above
# swallowed a real backend, or the build: derivation inverted.
gate_arm backends_ruled "$BACKENDS" 2 || rc=1

gate_arms_finish || rc=1
exit $rc
