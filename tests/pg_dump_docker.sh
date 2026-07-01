#!/usr/bin/env bash
# Test-only pg_dump shim for the migrate PG e2e suite.
#
# The Nix dev shell used by these tests does not put pg_dump on PATH. The
# confirmed local Postgres fixture already has the matching pg_dump binary inside
# the appbase-migrate-postgres-1 container, so this wrapper runs that binary
# without touching container lifecycle. It rewrites the host-mapped localhost:5440
# DSN to the container-local 127.0.0.1:5432 endpoint before execing pg_dump.
set -euo pipefail

container="${MIGRATE_TEST_PG_CONTAINER:-appbase-migrate-postgres-1}"

rewrite_dsn() {
  local dsn="$1"
  case "$dsn" in
    postgres://*|postgresql://*)
      dsn="${dsn//localhost:5440/127.0.0.1:5432}"
      dsn="${dsn//127.0.0.1:5440/127.0.0.1:5432}"
      ;;
    *host=localhost*|*host=127.0.0.1*)
      dsn="${dsn//host=localhost/host=127.0.0.1}"
      dsn="${dsn//port=5440/port=5432}"
      ;;
  esac
  printf '%s\n' "$dsn"
}

args=()
for arg in "$@"; do
  args+=("$(rewrite_dsn "$arg")")
done

exec docker exec -i "$container" pg_dump "${args[@]}"
