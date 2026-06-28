#!/usr/bin/env bash
#
# db-migrate.sh — dev wrapper around the `zeroship-migrate` binary for the
# zeroship platform schema (the zeroship / oauth_hydra / public schemas). Hydra
# owns its own schema via `hydra migrate`; this tool does not touch it.
#
# The migrations live in db/migrations/ (Flyway-style `V<NNNN>__*.sql` /
# `.down.sql` / `R__*.sql`) and are applied by the `migrate` compose service at
# stack boot. This wrapper is for running the migration engine by hand against a
# running dev DB — checking status, validating, applying, rolling back. It runs
# the engine under the PLATFORM trust profile (the widened guard for the
# platform schemas — design docs/proposals/2026-06-17-platform-migrations-flyway-mode-design.md).
#
# Subcommands (mapped from the old Liquibase verbs):
#   status              pending vs applied migrations         (was: status)
#   migrate             apply pending migrations              (was: update)
#   validate            dry-run + guard-check + drift report  (was: update-sql / validate)
#   rollback [args...]  roll back applied migrations          (was: rollback*)
#                       e.g. `rollback --steps 1` or `rollback --to 0024`
# `changelog-sync` is DROPPED — there is no Liquibase history to adopt onto, and
# pre-launch zeroship has no deployed DB to back-fill (adoption is a non-goal).
#
# Examples:
#   ops/db-migrate.sh status
#   ops/db-migrate.sh migrate
#   ops/db-migrate.sh validate
#   ops/db-migrate.sh rollback --steps 1
#
# Targets the compose Postgres on its host-mapped port by default. Override the
# connection + behaviour with env vars:
#   ZEROSHIP_MIGRATE_DSN   (default postgres://postgres:zeroship@localhost:5440/zeroship)
#   ZEROSHIP_MIGRATIONS_DIR (default <repo>/db/migrations)
#   ZEROSHIP_MIGRATE_PROFILE (default platform)
#   ZEROSHIP_MIGRATE_BIN   (a prebuilt binary path; if unset, runs via `cargo run`)
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
MIGRATIONS_DIR="${ZEROSHIP_MIGRATIONS_DIR:-$ROOT/db/migrations}"
DSN="${ZEROSHIP_MIGRATE_DSN:-postgres://postgres:zeroship@localhost:5440/zeroship}"
PROFILE="${ZEROSHIP_MIGRATE_PROFILE:-platform}"

if [ "$#" -eq 0 ]; then
  echo "usage: $(basename "$0") <status|migrate|validate|rollback> [args...]" >&2
  exit 2
fi

SUBCOMMAND="$1"; shift

# Run a prebuilt binary if one is provided; otherwise build+run via cargo from
# the workspace (the dev ergonomic — no separate install step needed).
if [ -n "${ZEROSHIP_MIGRATE_BIN:-}" ]; then
  RUNNER=("$ZEROSHIP_MIGRATE_BIN")
else
  RUNNER=(cargo run --quiet --manifest-path "$ROOT/Cargo.toml" \
          -p zeroship-migrate --bin zeroship-migrate --)
fi

exec "${RUNNER[@]}" \
  "$SUBCOMMAND" \
  --dir "$MIGRATIONS_DIR" \
  --database-url "$DSN" \
  --profile "$PROFILE" \
  "$@"
