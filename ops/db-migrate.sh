#!/usr/bin/env bash
#
# db-migrate.sh — dev wrapper around the `zeroship-migrate` binary for the
# zeroship platform schema (the zeroship / public schemas).
#
# The platform migrations live in db/migrations-ts/ as committed JS DSL
# `@zeroship/migrate` modules. The `migrate` compose service records them to
# transient IR and applies them under the PLATFORM trust profile at stack boot.
# This wrapper is for running the same migration engine by hand against a
# running dev DB.
#
# Subcommands:
#   migrate             apply pending platform JS DSL migrations
#   status              generic CLI journal view; does not yet load platform `.ts`
#   validate            generic CLI dry-run; does not yet load platform `.ts`
#   rollback [args...]  generic CLI rollback for SQL/IR corpora
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
#   ZEROSHIP_MIGRATIONS_DIR (default <repo>/db/migrations-ts)
#   ZEROSHIP_MIGRATE_PROFILE (default platform)
#   ZEROSHIP_MIGRATE_BIN   (a prebuilt binary path; if unset, runs via `cargo run`)
#   ZEROSHIP_RECORDER_CHILD (path to zeroship-migrate-recorder-child; auto-built for cargo run)
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
MIGRATIONS_DIR="${ZEROSHIP_MIGRATIONS_DIR:-$ROOT/db/migrations-ts}"
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
  if [ -z "${ZEROSHIP_RECORDER_CHILD:-}" ]; then
    export ZEROSHIP_RECORDER_CHILD="$(dirname "$ZEROSHIP_MIGRATE_BIN")/zeroship-migrate-recorder-child"
  fi
  RUNNER=("$ZEROSHIP_MIGRATE_BIN")
else
  cargo build --quiet --manifest-path "$ROOT/Cargo.toml" \
    -p zeroship-migrate --bin zeroship-migrate-recorder-child
  export ZEROSHIP_RECORDER_CHILD="${ZEROSHIP_RECORDER_CHILD:-$ROOT/target/debug/zeroship-migrate-recorder-child}"
  RUNNER=(cargo run --quiet --manifest-path "$ROOT/Cargo.toml" \
          -p zeroship-migrate --bin zeroship-migrate --)
fi

exec "${RUNNER[@]}" \
  "$SUBCOMMAND" \
  --dir "$MIGRATIONS_DIR" \
  --database-url "$DSN" \
  --profile "$PROFILE" \
  "$@"
