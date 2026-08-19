#!/usr/bin/env bash
# ============================================================================
# Every stateful service in deploy/compose must keep its data on a NAMED VOLUME.
#
# THE DEFECT THIS EXISTS FOR, found 2026-08-11 while walking the operator deploy
# (scenario 18). The compose file gives durable named volumes to the cache and
# to the replayable log, and NONE to the database:
#
#   redis      redis-data:/data                       <- a cache
#   redpanda   redpanda-data:/var/lib/redpanda/data   <- a replayable log
#   worker     bundles:, app-storage:
#   postgres   (nothing)                              <- every creator's data
#
# Postgres' only `volumes:` entry is a read-only bind of ../ops/postgres-init.sql
# into /docker-entrypoint-initdb.d. PGDATA therefore lives in the container's
# writable layer, so `docker compose down` - or any `up` that recreates the
# container after a config change - destroys the platform database: control
# state, auth, billing, the platform migration journal, and every `app_*` schema.
# The cache and the log survive it.
#
# The asymmetry is why this reads as an oversight rather than a decision. Nobody
# deliberately makes a Redis cache durable and the system of record ephemeral.
#
# WHAT THIS DOES NOT CHECK, so a green is not over-read:
#   - it does not run docker and does not inspect a live container
#   - it does not verify the volume is mounted at the RIGHT path for the image;
#     it checks that a named volume is attached at all
#   - it says nothing about backups, retention, or whether the host directory
#     behind a named volume is itself durable
#   - `docker compose down -v` removes named volumes too; this gate does not
#     protect against that, and nothing in a compose file can
# ============================================================================
set -uo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
CF="$ROOT/deploy/compose/docker-compose.yml"
PASS=0; FAIL=0
pass() { PASS=$((PASS+1)); echo "  ok   $1"; }
fail() { FAIL=$((FAIL+1)); echo "  FAIL $1"; }

echo "============================================"
echo "  stateful compose services keep data on a named volume"
echo "============================================"

[ -f "$CF" ] || { echo "  x REFUSED: $CF not found." >&2; exit 1; }

# Services whose data must outlive the container. Listed explicitly rather than
# inferred: "is this service stateful" is a judgement the file cannot express,
# and a wrong inference here would either miss the database or demand a volume
# for a stateless binary.
STATEFUL="postgres redis redpanda"

# A named-volume mount is `- <name>:<path>` where <name> is declared under the
# top-level `volumes:` key. A bind mount (`- ../foo:/bar`) contains a slash or a
# dot before the colon and does NOT count - that is the distinction the postgres
# service fell through, since its init-script bind looks like a volumes entry.
# DO NOT assign to $1 here. The obvious `gsub(/:/,"",$1)` REBUILDS $0 from the
# fields, which strips the leading indentation - so the next rule,
# `v && /^[a-z]/{v=0}`, sees a line that now starts with a lowercase letter and
# clears the in-block flag. Exactly ONE volume escapes before the reset, which
# is what my first version did: it returned `bundles` alone and then reported
# redis and redpanda as volume-less when both plainly have one. The name is
# taken into a local instead, leaving $0 untouched.
mapfile -t DECLARED < <(
  awk '/^volumes:/{v=1;next}
       v && /^  [a-z][a-z0-9_-]*:/{ n=$1; sub(/:.*$/,"",n); print n; next }
       v && /^[a-z]/{v=0}' "$CF"
)
if [ "${#DECLARED[@]}" -eq 0 ]; then
  echo "  x REFUSED: parsed ZERO declared named volumes out of $CF." >&2
  echo "    A gate that checks nothing must not report success." >&2
  exit 1
fi

for svc in $STATEFUL; do
  body="$(awk -v s="  $svc:" '$0==s{f=1;next} /^  [a-z][a-z0-9_-]*:[[:space:]]*$/{f=0} f' "$CF")"
  if [ -z "$body" ]; then
    fail "service '$svc' not found in $CF - this gate's premise is stale"
    continue
  fi
  hit=""
  for vol in "${DECLARED[@]}"; do
    if printf '%s\n' "$body" | grep -qE "^      - ${vol}:/"; then hit="$vol"; break; fi
  done
  if [ -n "$hit" ]; then
    pass "$svc keeps data on named volume '$hit'"
  else
    fail "$svc mounts NO named volume; its data lives in the container layer and dies with the container"
  fi
done

echo ""
echo "  $PASS passed, $FAIL failed, $((PASS+FAIL)) ran"

# Counts assertions that RAN, not that PASSED.
#
# EXACT, not a floor, and not overridable. MEASURED 2026-08-19: 3 stateful
# services in the tracked compose files. The count is a pure parse of tracked
# files, so it is deterministic; when a stateful service is added or removed,
# re-measure and change this line in the same commit.
EXPECT_RAN=3
RAN=$((PASS + FAIL))
rc=0
[ "$FAIL" -eq 0 ] || rc=1
if [ "$RAN" -ne "$EXPECT_RAN" ]; then
  echo "  x COUNT: $RAN stateful services checked, expected exactly $EXPECT_RAN." >&2
  echo "    Fewer means services went missing from the parse; more means one was" >&2
  echo "    added - re-measure and bump this line." >&2
  rc=1
fi
exit $rc
