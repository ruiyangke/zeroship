#!/usr/bin/env bash
#
# db-migrate.sh — dev wrapper around Liquibase for the zeroship platform schema
# (the control / auth / platform schemas). Hydra owns its own schema via
# `hydra migrate`; this tool does not touch it.
#
# The migrations live in db/changelog/ and are applied by the `migrate` compose
# service at stack boot. This wrapper is for running Liquibase by hand against a
# running dev DB — checking status, previewing, rolling back, validating.
#
# Examples:
#   ops/db-migrate.sh status              # pending vs applied changesets
#   ops/db-migrate.sh update              # apply pending migrations
#   ops/db-migrate.sh update-sql          # print the SQL `update` WOULD run (dry run)
#   ops/db-migrate.sh history             # deployment history
#   ops/db-migrate.sh validate            # checksum / structural validation
#   ops/db-migrate.sh rollback-count 1    # roll back the last applied changeset
#   ops/db-migrate.sh tag v1              # tag the current state for later rollback
#   ops/db-migrate.sh changelog-sync      # mark all changesets applied WITHOUT running
#                                         #   (adopt onto a DB that already has the schema)
#
# Targets the compose Postgres on its host-mapped port by default. Override the
# connection with env vars:
#   ZEROSHIP_DB_JDBC   (default jdbc:postgresql://localhost:5440/zeroship)
#   ZEROSHIP_DB_USER   (default postgres)
#   ZEROSHIP_DB_PASS   (default zeroship)
#   ZEROSHIP_LIQUIBASE_IMAGE (default liquibase/liquibase:4.31)
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
CHANGELOG_DIR="${ZEROSHIP_CHANGELOG_DIR:-$ROOT/db/changelog}"
JDBC="${ZEROSHIP_DB_JDBC:-jdbc:postgresql://localhost:5440/zeroship}"
DB_USER="${ZEROSHIP_DB_USER:-postgres}"
DB_PASS="${ZEROSHIP_DB_PASS:-zeroship}"
IMAGE="${ZEROSHIP_LIQUIBASE_IMAGE:-liquibase/liquibase:4.31}"

if [ "$#" -eq 0 ]; then
  echo "usage: $(basename "$0") <liquibase-command> [args...]" >&2
  echo "       e.g. status | update | update-sql | history | validate | rollback-count N" >&2
  exit 2
fi

# --network host so the container reaches the host-mapped Postgres port on Linux.
# Tracking tables (DATABASECHANGELOG / DATABASECHANGELOGLOCK) live in `public`
# so they don't pollute the per-service schemas.
exec docker run --rm --network host \
  -v "$CHANGELOG_DIR:/liquibase/changelog:ro" \
  "$IMAGE" \
  --url="$JDBC" \
  --username="$DB_USER" \
  --password="$DB_PASS" \
  --changelog-file=changelog/db.changelog-master.yaml \
  --liquibase-schema-name=public \
  "$@"
