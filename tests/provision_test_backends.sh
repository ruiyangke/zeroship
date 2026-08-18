#!/usr/bin/env bash
# ============================================================================
# provision_test_backends.sh - stand up the backends the test suites REQUIRE.
#
# WHY THIS EXISTS
# ---------------
# PostgreSQL and Redis are not optional for this workspace's tests, but until
# now nothing in the tree stood them up. The suites simply assumed a server was
# there, and when it was not the affected tests returned early and counted as
# passes. `ZEROSHIP_REQUIRE_LIVE_BACKENDS=1` existed to turn those into
# failures; it was opt-in, so the developer who most needed it was the one who
# did not know it existed. The flag is deleted. Provisioning is a real step
# instead, and this is it.
#
# WHAT IT PROVISIONS, and it is deliberately the two named backends and nothing
# else:
#
#   postgres  deploy/compose's `postgres` service, on 127.0.0.1:5440.
#             It runs `postgres -c wal_level=logical`, which is LOAD-BEARING
#             and is the reason this script uses the compose definition rather
#             than a `docker run` of its own. See the 25-line comment on that
#             service: on the postgres:16 default (`replica`) CREATE PUBLICATION
#             succeeds with only a WARNING while the replication slot fails, so
#             a subscription serves its initial snapshot and then hangs. Ten
#             guarded tests in crates/plugin-db/tests/integration.rs skip
#             themselves on a `replica` server.
#   redis     deploy/compose's `redis` service, on 127.0.0.1:6390.
#
# WHAT IT DOES NOT PROVISION, so a green here is not over-read: MinIO (the
# `compio-s3` / storage-parity suites start their own container and announce a
# skip when docker is unavailable), Redpanda, a Dragonfly CLUSTER
# (deploy/compose/cluster.yml), an SMTP sink. Those still announce skips, and
# the suite gates count them.
#
# THE DEFAULTS LINE UP ON PURPOSE. The addresses above are exactly what
# `PG_TEST_URL` and `REDIS_TEST_URL` fall back to when unset, so a developer who
# runs this script needs to export nothing at all. If you change a port here,
# change the two defaults in libs/compio-postgres/tests/integration.rs and
# libs/compio-redis/tests/common/mod.rs in the same commit.
#
# USAGE
# -----
#   tests/provision_test_backends.sh            # up + wait for healthy
#   tests/provision_test_backends.sh --check    # only verify; provision nothing
# ============================================================================
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
COMPOSE_FILE="${ZEROSHIP_COMPOSE_FILE:-$ROOT/deploy/compose/docker-compose.yml}"

PG_HOST="${PG_HOST:-127.0.0.1}"
PG_PORT="${PG_PORT:-5440}"
REDIS_HOST="${REDIS_HOST:-127.0.0.1}"
REDIS_PORT="${REDIS_PORT:-6390}"

CHECK_ONLY=0
[ "${1:-}" = "--check" ] && CHECK_ONLY=1

# A BOUNDED wait, and the bound is the point. A poll long enough to outlast a
# server that is never coming is the deleted flag again in a slower costume: it
# converts "there is no database" into "the run took a while and then failed
# obscurely". 60 seconds is a first `postgres:16` initdb with room to spare;
# anything past that is a missing server, not a slow one.
READY_TIMEOUT_SECONDS="${READY_TIMEOUT_SECONDS:-60}"

fatal() { echo "FATAL: $*" >&2; exit 1; }

# Is a TCP port accepting connections? bash's /dev/tcp, so this needs no nc, no
# psql and no redis-cli - none of which are guaranteed on a fresh checkout, and
# the whole point of this script is to work on the machine that has nothing.
port_open() {
  (exec 3<>"/dev/tcp/$1/$2") 2>/dev/null && exec 3<&- 3>&- && return 0
  return 1
}

wait_for_port() {
  local host="$1" port="$2" label="$3" deadline
  deadline=$(( $(date +%s) + READY_TIMEOUT_SECONDS ))
  while [ "$(date +%s)" -lt "$deadline" ]; do
    if port_open "$host" "$port"; then
      echo "  ok   ${label} answering on ${host}:${port}"
      return 0
    fi
    sleep 1
  done
  return 1
}

if [ "$CHECK_ONLY" -eq 0 ]; then
  command -v docker >/dev/null 2>&1 \
    || fatal "docker is not on PATH, and the test backends are containers.
       Install docker, or point the suites at servers you already run:
         PG_TEST_URL=postgres://user:pass@host:port/db
         REDIS_TEST_URL=redis://host:port"

  [ -f "$COMPOSE_FILE" ] || fatal "compose file not found: $COMPOSE_FILE"

  echo "==> docker compose up -d postgres redis"
  # `--` nothing clever: only the two services, so this does not drag the
  # gateway, worker, control, auth, Caddy, verdaccio or redpanda along with it.
  # A test run needs two servers, not the platform.
  docker compose -f "$COMPOSE_FILE" up -d postgres redis \
    || fatal "docker compose could not start postgres and redis.
       If the port is already taken by a container this file does not own,
       that container is what your tests have been running against - stop it
       (docker stop <name>) and re-run, so the server under test is the one
       this repo defines."
fi

echo "==> waiting for the backends (bound: ${READY_TIMEOUT_SECONDS}s each)"
wait_for_port "$PG_HOST" "$PG_PORT" "PostgreSQL" \
  || fatal "PostgreSQL never came up on ${PG_HOST}:${PG_PORT} within ${READY_TIMEOUT_SECONDS}s.
       docker compose -f $COMPOSE_FILE logs postgres"
wait_for_port "$REDIS_HOST" "$REDIS_PORT" "Redis" \
  || fatal "Redis never came up on ${REDIS_HOST}:${REDIS_PORT} within ${READY_TIMEOUT_SECONDS}s.
       docker compose -f $COMPOSE_FILE logs redis"

# The wal_level check is not decoration. A Postgres that answers on 5440 is not
# necessarily THIS Postgres: on a shared development machine the port is
# routinely held by a hand-started container nobody owns, and the one that was
# holding it while this script was written ran the postgres:16 default. Ten
# plugin-db tests then skip themselves and still report as passed, so the
# server being wrong is invisible in every number a run prints. Checking it
# HERE, where the answer is one query, is the difference between a provisioning
# step and a hope.
#
# Reported, not fatal: `compio-postgres`, `compio-redis` and the auth/billing
# suites do not need logical decoding, and failing them for a plugin-db
# requirement would be its own kind of wrong. The line is loud enough to act on.
if command -v docker >/dev/null 2>&1; then
  pg_cid="$(docker compose -f "$COMPOSE_FILE" ps -q postgres 2>/dev/null || true)"
  if [ -n "$pg_cid" ]; then
    wal="$(docker exec "$pg_cid" psql -U postgres -tAc 'show wal_level' 2>/dev/null || true)"
    if [ "$wal" = "logical" ]; then
      echo "  ok   PostgreSQL wal_level=logical (logical-decoding tests will run)"
    else
      echo "  WARN PostgreSQL wal_level=${wal:-unknown}, not 'logical'." >&2
      echo "       The 10 pg_has_logical_wal-guarded tests in" >&2
      echo "       crates/plugin-db/tests/integration.rs will announce skips." >&2
    fi
  else
    echo "  WARN no compose-managed postgres container found; something else is" >&2
    echo "       holding ${PG_HOST}:${PG_PORT}, and it is what your tests will use." >&2
  fi
fi

cat <<EOF

Backends ready. The test defaults already point here, so nothing needs exporting:
  PG_TEST_URL     postgres://postgres:zeroship@${PG_HOST}:${PG_PORT}/zeroship
  REDIS_TEST_URL  redis://${REDIS_HOST}:${REDIS_PORT}
EOF
