#!/usr/bin/env bash
#
# db-migrate.sh - dev wrapper that applies the zeroship platform schema (the
# zeroship / public / service_authn schemas) with the `zero-migrate` CLI.
#
# The platform migrations live in db/migrations-ts/ as committed JS DSL modules.
# The CLI imports each one, records the ops in-process, lowers under the PLATFORM
# charter (policies/platform.policy.toml) and applies over `pg`. This wrapper is
# for running that one-shot by hand against a running dev DB; the compose
# `migrate` service runs the same command inside the image.
#
# IT REPLACED A RUST BINARY, and the difference is worth knowing. Until
# 2026-08-28 this ran `zeroship-platform-migrate`, which authored the same files
# in zeroship-runtime's own V8. That binary is deleted. What you gain is the rest
# of the CLI's verbs against the same journal - `status`, `plan`, `history`,
# `baseline`, `rollback` - which the one-shot never exposed. What you need that
# you did not before is NODE and a built CLI:
#
#   pnpm install && pnpm build
#
# The apply is idempotent: a re-run re-derives byte-identical journal versions
# (each is a hash of owner_app + migration name) and skips every applied file.
#
# `cargo xtask test migrations` exercises this CLI and platform policy directly
# from Rust. It reconciles the recorder-operation ledger, applies the corpus to
# owned PostgreSQL, verifies journal and status identities, and checks that
# applying again leaves history unchanged. It does not invoke this wrapper.
#
# Examples:
#   deploy/ops/db-migrate.sh
#   ZEROSHIP_MIGRATE_VERB=history deploy/ops/db-migrate.sh --json
#   ZEROSHIP_MIGRATE_VERB=status deploy/ops/db-migrate.sh
#
# Targets the compose Postgres on its host-mapped port by default. Override the
# connection + behaviour with env vars:
#   ZEROSHIP_MIGRATE_DSN     (default postgres://postgres:zeroship@localhost:5440/zeroship)
#   ZEROSHIP_MIGRATIONS_DIR  (default <repo>/db/migrations-ts)
#   ZEROSHIP_MIGRATE_VERB    (default apply; any zero-migrate verb, e.g. status)
#   ZEROSHIP_MIGRATE_CLI     (a prebuilt cli-bin.js; default the in-tree build)
#
# ZEROSHIP_PROJECT_SCHEMA AND ZEROSHIP_PROJECT_ID ARE GONE, and they were never
# knobs. The corpus spells its own schema - 562 `schema: "zeroship"` and one
# `schema: "public"` - so a different project schema produces a charter that
# refuses every file in it. The project id was the retired runner's advisory-lock
# key; the CLI derives that key from the schema alone
# (`owner_app_project` in crates/zeroship-migrate-node/src/verbs.rs), so there is
# nothing left for a second name to select.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
MIGRATIONS_DIR="${ZEROSHIP_MIGRATIONS_DIR:-$ROOT/db/migrations-ts}"
DSN="${ZEROSHIP_MIGRATE_DSN:-postgres://postgres:zeroship@localhost:5440/zeroship}"
VERB="${ZEROSHIP_MIGRATE_VERB:-apply}"
CLI="${ZEROSHIP_MIGRATE_CLI:-$ROOT/packages/zero-migrate-cli/dist/cli-bin.js}"

if [ ! -f "$CLI" ]; then
  echo "db-migrate.sh: $CLI is missing." >&2
  echo "  run: pnpm install && pnpm build" >&2
  exit 2
fi

# THE DSN REACHES THE CLI IN A 0600 FILE, NEVER AS AN ARGUMENT VALUE. `ps` and
# /proc/<pid>/cmdline are readable by every process of this user, so a DSN in argv
# is a password published to the whole session for as long as the migrate runs.
# The CLI advertises `--database-url <value>`; it is deliberately not used here.
#
# The reader enforces the permission, not this script:
# packages/zero-migrate-cli/src/config.ts (`enforceOwnerOnly`) REFUSES a config
# file that supplies a literal `url` and has any bit set in 0o077, naming the mode
# and the chmod. The explicit chmod below is what keeps that refusal from firing;
# it is not decoration.
CFG_DIR="$(mktemp -d -t zeroship-migrate-cfg.XXXXXX)"
chmod 700 "$CFG_DIR"
trap 'rm -rf "$CFG_DIR"' EXIT HUP INT TERM
{
  printf '[env.platform]\n'
  printf 'url = "%s"\n' "$DSN"
  printf 'dir = "%s"\n' "$MIGRATIONS_DIR"
  printf 'schema = "zeroship"\n'
  printf 'owner_app = "zeroship_platform"\n'
  printf 'registry = "%s"\n' "$ROOT/policies/platform-table-owners.json"
  printf 'policy = ["%s"]\n' "$ROOT/policies/platform.policy.toml"
} > "$CFG_DIR/zero-migrate.toml"
chmod 600 "$CFG_DIR/zero-migrate.toml"

# `--approve` only on the verbs that change something. `status`/`history`/`plan`
# refuse the flag, so passing it unconditionally would break the read-only verbs
# this wrapper now also gives you.
APPROVE=()
case "$VERB" in
  apply|rollback|resolve|baseline) APPROVE=(--approve) ;;
esac

# `exec` would drop the EXIT trap and leave the DSN on disk, so this runs as a
# child and the trap does the cleanup.
node "$CLI" "$VERB" \
  --config "$CFG_DIR/zero-migrate.toml" \
  --env platform \
  "${APPROVE[@]}" \
  "$@"
