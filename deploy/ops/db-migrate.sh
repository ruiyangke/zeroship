#!/usr/bin/env bash
#
# db-migrate.sh - dev wrapper around the `zeroship-platform-migrate` binary for the
# zeroship platform schema (the zeroship / public schemas).
#
# The platform migrations live in db/migrations-ts/ as committed JS DSL
# `@zeroship/migrate` modules. The runner authors each `.ts` in-process
# (zeroship-runtime's own V8 + the published `zero-migrate` v1 recorder), lowers
# under the PLATFORM trust profile, and applies over the native compio-postgres
# seam - the same path the `migrate` compose service runs at stack boot. This
# wrapper is for running that one-shot by hand against a running dev DB.
#
# The published-engine platform one-shot is APPLY-ONLY and idempotent: a re-run
# re-derives byte-identical journal versions and skips already-applied files.
# (The retired in-tree CLI's `status`/`validate`/`rollback` subcommands are not
# exposed by this bin.)
#
# Examples:
#   deploy/ops/db-migrate.sh
#
# Targets the compose Postgres on its host-mapped port by default. Override the
# connection + behaviour with env vars:
#   ZEROSHIP_MIGRATE_DSN     (default postgres://postgres:zeroship@localhost:5440/zeroship)
#   ZEROSHIP_MIGRATIONS_DIR  (default <repo>/db/migrations-ts)
#   ZEROSHIP_PROJECT_SCHEMA  (default zeroship)
#   ZEROSHIP_PROJECT_ID      (default zeroship)
#   ZEROSHIP_MIGRATE_BIN     (a prebuilt binary path; if unset, runs via `cargo run`)
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
MIGRATIONS_DIR="${ZEROSHIP_MIGRATIONS_DIR:-$ROOT/db/migrations-ts}"
DSN="${ZEROSHIP_MIGRATE_DSN:-postgres://postgres:zeroship@localhost:5440/zeroship}"
PROJECT_SCHEMA="${ZEROSHIP_PROJECT_SCHEMA:-zeroship}"
PROJECT_ID="${ZEROSHIP_PROJECT_ID:-zeroship}"

# Run a prebuilt binary if one is provided; otherwise build+run via cargo from
# the workspace (the dev ergonomic - no separate install step needed). The
# platform-migrate bin lives in the `zeroship-migrate-adapter` crate behind the
# `platform-cli` feature (it lights up the V8 authoring front-end), so a
# from-source run must enable it explicitly.
if [ -n "${ZEROSHIP_MIGRATE_BIN:-}" ]; then
  RUNNER=("$ZEROSHIP_MIGRATE_BIN")
else
  RUNNER=(cargo run --quiet --manifest-path "$ROOT/Cargo.toml" \
          -p zeroship-migrate-adapter --bin zeroship-platform-migrate \
          --features zeroship-migrate-adapter/platform-cli --)
fi

exec "${RUNNER[@]}" \
  --migrations-dir "$MIGRATIONS_DIR" \
  --database-url "$DSN" \
  --project-schema "$PROJECT_SCHEMA" \
  --project-id "$PROJECT_ID" \
  "$@"
