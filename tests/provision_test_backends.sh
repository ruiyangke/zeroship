#!/usr/bin/env bash
# ============================================================================
# provision_test_backends.sh - stand up the backends the test suites REQUIRE.
#
# Provisions the shared PostgreSQL service still used by the shell suites.
# Mailer, auth, authn, KV and Redis driver tests own their servers through
# Testcontainers.
#
# PostgreSQL comes from deploy/compose so its logical-decoding and prepared-
# transaction settings match the service definition.
#
# Usage:
#   tests/provision_test_backends.sh          # start and wait
#   tests/provision_test_backends.sh --check  # adopt existing servers
#
# Both forms write deploy/ops/zeroship.test.toml in the platform config schema.
# The file is generated and gitignored because database credentials are secrets.
# PG_* inputs select PostgreSQL; shared suites may override it with PG_TEST_URL.
# ============================================================================
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
COMPOSE_FILE="${ZEROSHIP_COMPOSE_FILE:-$ROOT/deploy/compose/docker-compose.yml}"

# ---------------------------------------------------------------------------
# THE COORDINATES OF THE TEST BACKENDS. This block is their one definition.
#
# They used to be written out per harness: run_auth_suite.sh:65-68 and
# run_billing_suite.sh:124-127 each carried the same four `${PG_x:-...}` lines,
# the DSN was spelled a fifth time in this file's closing banner, and the two
# driver suites carried a sixth and seventh copy as Rust constants. Seven copies
# of one address is how a run ends up against the wrong server with the wrong
# password, which is what happened three times in one day.
#
# Everything else now reads these through the generated overlay written below.
# The environment names remain as the OVERRIDE tier - that is how a caller
# points a run at a server of their own, and it is the same relationship the
# platform services have with their own TOML.
# ---------------------------------------------------------------------------
PG_HOST="${PG_HOST:-127.0.0.1}"
PG_PORT="${PG_PORT:-5440}"
PG_USER="${PG_USER:-postgres}"
PG_PASS="${PG_PASS:-zeroship}"
PG_DB="${PG_DB:-zeroship}"
PG_DSN="postgres://${PG_USER}:${PG_PASS}@${PG_HOST}:${PG_PORT}/${PG_DB}"

TEST_OVERLAY="$ROOT/deploy/ops/zeroship.test.toml"

CHECK_ONLY=0
[ "${1:-}" = "--check" ] && CHECK_ONLY=1

# A BOUNDED wait, and the bound is the point. A poll long enough to outlast a
# server that is never coming is the deleted flag again in a slower costume: it
# converts "there is no database" into "the run took a while and then failed
# obscurely". 60 seconds is a first `postgres:16` initdb with room to spare;
# anything past that is a missing server, not a slow one.
READY_TIMEOUT_SECONDS="${READY_TIMEOUT_SECONDS:-60}"

fatal() { echo "FATAL: $*" >&2; exit 1; }

# ---------------------------------------------------------------------------
# The placeholder env file, and why `docker compose up -d postgres` needs
# one.
#
# MEASURED 2026-08-18 on a checkout with no `deploy/compose/.env`: that command
# exits 1 having started nothing, with fourteen lines of
#   error while interpolating services.worker.environment.ZEROSHIP_CONTROL_KEY:
#   required variable ZEROSHIP_CONTROL_KEY is missing a value: run zeroship dev init
# Compose interpolates the WHOLE file before it selects services, so the `:?`
# guards on the PLATFORM services (auth, control, gateway, worker, migrate-server)
# reject a run that would not have started any of them. Those guards are right
# and stay: booting the platform with junk credentials is exactly what they
# exist to stop.
#
# `up` below selects only `postgres`, so no platform service that consumes
# these placeholder secrets is created.
# A real `deploy/compose/.env` is loaded after this one and wins, so on a
# machine that has run `zeroship dev init` the real values are what compose
# sees.
#
# The names are SCANNED out of the compose file rather than listed here. A list
# would be a second copy of the platform's required-secret set, and it would go
# stale the first time a service gained one - as a failure that reads as
# "provisioning is broken" rather than "the list is short".
# ---------------------------------------------------------------------------
PLACEHOLDER_ENV=""
cleanup() { [ -n "$PLACEHOLDER_ENV" ] && rm -f "$PLACEHOLDER_ENV"; return 0; }
trap cleanup EXIT

compose_env_args() {
  PLACEHOLDER_ENV="$(mktemp -t zeroship-provision-env.XXXXXX)"
  grep -oE '\$\{[A-Z0-9_]+:\?[^}]*\}' "$COMPOSE_FILE" \
    | grep -oE '\{[A-Z0-9_]+' | tr -d '{' | sort -u \
    | sed 's/$/=provision-placeholder-no-service-reads-this/' >"$PLACEHOLDER_ENV"
  printf '%s\n%s\n' --env-file "$PLACEHOLDER_ENV"
  if [ -f "$(dirname "$COMPOSE_FILE")/.env" ]; then
    printf '%s\n%s\n' --env-file "$(dirname "$COMPOSE_FILE")/.env"
  fi
}

mapfile -t ENV_ARGS < <(compose_env_args)

dc() { docker compose "${ENV_ARGS[@]}" -f "$COMPOSE_FILE" "$@"; }

# Is a TCP port accepting connections? bash's /dev/tcp, so this needs no nc, no
# psql and no redis-cli - none of which are guaranteed on a fresh checkout, and
# the whole point of this script is to work on the machine that has nothing.
port_open() {
  (exec 3<>"/dev/tcp/$1/$2") 2>/dev/null && exec 3<&- 3>&- && return 0
  return 1
}

# Ready means the SERVICE answers, not that the port is bound.
#
# MEASURED 2026-08-18, and it is why this is not a plain TCP probe. On a first
# `up` the docker proxy publishes 5432 immediately while `initdb` is still
# running inside the container, so `/dev/tcp` connects within a second and the
# server refuses queries for another ten. The first version of this script
# returned "ready" there and then reported `wal_level=unknown` from a psql that
# could not connect - a provisioning step that hands back an unusable server and
# says it is fine is worse than none.
#
# So: the compose HEALTHCHECK is the signal when there is a container to ask
# (postgres runs `pg_isready`), and the TCP check is the fallback for a server this
# script did not start, where there is no container to inspect and the caller
# has pointed PG_TEST_URL somewhere of their own.
wait_for_backend() {
  local service="$1" host="$2" port="$3" label="$4" deadline cid state
  deadline=$(( $(date +%s) + READY_TIMEOUT_SECONDS ))
  cid=""
  command -v docker >/dev/null 2>&1 && cid="$(dc ps -q "$service" 2>/dev/null || true)"

  while [ "$(date +%s)" -lt "$deadline" ]; do
    if [ -n "$cid" ]; then
      state="$(docker inspect --format '{{if .State.Health}}{{.State.Health.Status}}{{else}}no-healthcheck{{end}}' "$cid" 2>/dev/null || true)"
      if [ "$state" = "healthy" ]; then
        echo "  ok   ${label} healthy on ${host}:${port}"
        return 0
      fi
      # A service with no healthcheck cannot be waited on this way; fall through
      # to the port probe rather than looping to the deadline on nothing.
      [ "$state" = "no-healthcheck" ] && cid=""
    elif port_open "$host" "$port"; then
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
         PG_TEST_URL=postgres://user:pass@host:port/db"

  [ -f "$COMPOSE_FILE" ] || fatal "compose file not found: $COMPOSE_FILE"

  echo "==> docker compose up -d postgres"
  # Start only PostgreSQL, so placeholder platform secrets are never consumed.
  dc up -d postgres \
    || fatal "docker compose could not start postgres.
       If the port is already taken by a container this file does not own,
       that container is what your tests have been running against - stop it
       (docker stop <name>) and re-run, so the server under test is the one
       this repo defines."
fi

echo "==> waiting for the backends (bound: ${READY_TIMEOUT_SECONDS}s each)"
wait_for_backend postgres "$PG_HOST" "$PG_PORT" "PostgreSQL" \
  || fatal "PostgreSQL never came up on ${PG_HOST}:${PG_PORT} within ${READY_TIMEOUT_SECONDS}s.
       docker compose -f $COMPOSE_FILE logs postgres"

# These settings checks are not decoration. A Postgres that answers on 5440 is
# not necessarily THIS Postgres: on a shared development machine the port is
# routinely held by a hand-started container nobody owns, and the one that was
# holding it while this script was written ran the postgres:16 default. The
# wal-guarded plugin-db tests then skipped themselves and still reported as
# passed, so the server being wrong was invisible in every number a run printed.
# They FAIL now, which is what makes a wrong server visible in cargo's own
# output; this probe still earns its place by naming the setting BEFORE a suite
# spends its build time discovering it. Checking it HERE, where the answer is
# one query, is the difference between a provisioning step and a hope.
#
# Reported, not fatal: `--check` also describes service containers owned by
# auth and billing jobs, which do not run these logical-replication tests.
#
# THE PROBE FOLLOWS THE PORT, NOT THE COMPOSE PROJECT, and that distinction is
# the whole point. This block used to ask `dc ps -q postgres` for the container
# to interrogate - the compose-managed one - and then report ITS wal_level. But
# the paragraph above says the danger is a hand-started container holding the
# port, and in exactly that case `dc ps -q postgres` is EMPTY: the script fell
# to a generic ownership warning and printed no settings at all. So the one
# situation the check was written for was the one situation it did not measure.
#
# Observed 2026-08-27: 127.0.0.1:5440 was held by `zs-auth-pg-5440` (up 12
# days) running wal_level=replica, max_prepared_transactions=0. Both
# LOAD-BEARING values were wrong, the `pg_has_logical_wal`-guarded tests in
# the former ORM integration suite skipped-and-counted-as-passed,
# and nothing in any printed number said so. That reading is the argument for
# the guards being refusals today - it is not a description of them: `skip` and
# the crate that announced it are deleted, and the same wrong server now takes
# those tests red.
#
# Resolution order: the container PUBLISHING the port wins; the compose service
# is only the fallback for when nothing publishes it (host-network, remote).
if command -v docker >/dev/null 2>&1; then
  pg_cid="$(docker ps -q --filter "publish=${PG_PORT}" 2>/dev/null | head -1 || true)"
  pg_probe_source="the container publishing ${PG_HOST}:${PG_PORT}"
  if [ -z "$pg_cid" ]; then
    pg_cid="$(dc ps -q postgres 2>/dev/null || true)"
    pg_probe_source="the compose-managed postgres service"
  fi
  if [ -n "$pg_cid" ]; then
    echo "  ..   probing $pg_probe_source ($(docker inspect -f '{{.Name}}' "$pg_cid" 2>/dev/null | sed 's|^/||'))"
    wal="$(docker exec "$pg_cid" psql -U postgres -tAc 'show wal_level' 2>/dev/null || true)"
    if [ "$wal" = "logical" ]; then
      echo "  ok   PostgreSQL wal_level=logical (logical-decoding tests will run)"
    else
      echo "  WARN PostgreSQL wal_level=${wal:-unknown}, not 'logical'." >&2
      echo "       Suites using this server cannot verify logical replication." >&2
      echo "       Setting it is postmaster-level: ALTER" >&2
      echo "       SYSTEM SET needs a RESTART, not a reload, and the compose" >&2
      echo "       postgres already carries -c wal_level=logical." >&2
    fi
    prepared="$(docker exec "$pg_cid" psql -U postgres -tAc \
      'show max_prepared_transactions' 2>/dev/null || true)"
    if [[ "$prepared" =~ ^[1-9][0-9]*$ ]]; then
      echo "  ok   PostgreSQL max_prepared_transactions=$prepared (two-phase tests will run)"
    else
      echo "  WARN PostgreSQL max_prepared_transactions=${prepared:-unknown}." >&2
      echo "       compio-postgres' pgoutput two-phase test requires a nonzero value." >&2
    fi
  else
    echo "  WARN no compose-managed postgres container found; something else is" >&2
    echo "       holding ${PG_HOST}:${PG_PORT}, and it is what your tests will use." >&2
  fi
fi

# ---------------------------------------------------------------------------
# Write the test overlay: the servers above, in the platform's own TOML schema.
#
# WHY A FILE AND NOT MORE EXPORTS. Test code named this one PostgreSQL under
# eight environment variables, each read by one crate and exported by whichever
# suite remembered it. A name nobody exported meant its tests did not run; a
# name pointed at the wrong server meant they ran against it in silence. The
# services already solved this with a TOML overlay parsed by FileConfig under
# `deny_unknown_fields`, so this writes that same schema and test code loads it
# through that same parser. A misspelled key is then an error, not a silence.
#
# WHY IT IS GENERATED AND GITIGNORED RATHER THAN COMMITTED. Every DSN leaf in
# the schema is `secret`-classed - checked against the compiled contract dump,
# all eight of auth/control/gateway/migrate-server(x2)/worker(x2)/workflow_scheduler.
# Check 8 of tests/config_name_alignment_gate.sh fails ANY tracked *.toml
# holding a literal at a secret-classed leaf and states it will never carry an
# exception list. Committing this file with a real DSN was tried and rejected:
#
#   deploy/ops/zeroship.test.toml:2 control.database_url is secret-classed
#       and holds a plaintext literal
#
# The same gate exempts untracked overlays deliberately, because a real
# deployment's overlay may itself be a mounted secret. This is that, for tests.
#
# It is rewritten on every run rather than created-if-absent, so a stale file
# from a run with different coordinates cannot outlive them.
# ---------------------------------------------------------------------------
cat >"$TEST_OVERLAY" <<EOF
# GENERATED by tests/provision_test_backends.sh - do not edit, and do not commit
# (deploy/ops/zeroship.test.toml is gitignored; see the note in that script).
#
# Shared PostgreSQL coordinates for suites using the overlay. Read through
# zeroship_core::config::test_overlay in Rust and tests/lib/test_config.sh in
# shell. PG_TEST_URL overrides it for a per-run scratch database.

[control]
database_url = "$PG_DSN"

[auth]
database_url = "$PG_DSN"

[gateway]
database_url = "$PG_DSN"

[migrate_server]
database_url = "$PG_DSN"

[worker]
database_url = "$PG_DSN"

[workflow_scheduler]
database_url = "$PG_DSN"
EOF
echo "  ok   wrote ${TEST_OVERLAY#"$ROOT/"}"

cat <<EOF

Backend ready. ${TEST_OVERLAY#"$ROOT/"} names PostgreSQL:
  postgres   $PG_DSN
EOF
