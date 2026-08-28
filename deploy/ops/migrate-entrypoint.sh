#!/usr/bin/env bash
#
# zeroship-migrate-platform - the compose `migrate` one-shot's entrypoint.
#
# It applies db/migrations-ts with the `zero-migrate` CLI, and it exists for
# exactly one reason: TO KEEP THE DSN OFF THE ARGV.
#
# The CLI's own DSN flag is `--database-url <value>`. A compose `command:` is
# published by `docker inspect`, `docker ps --no-trunc` and /proc/<pid>/cmdline to
# anything sharing the PID namespace, and the DSN this one-shot needs is the
# cluster SUPERUSER. So the compose file passes a PATH - the same
# `--database-url-file` spelling the retired Rust one-shot took, and the same
# `migrate-dsn` mount `migrated` reads - and this script turns it into the 0600
# `zero-migrate.toml` the CLI reads with `--config`.
#
# THE PERMISSION REFUSAL IS THE READER'S, NOT THIS SCRIPT'S.
# `packages/zero-migrate-cli/src/config.ts` (`enforceOwnerOnly`) refuses a config
# file that supplies a literal `url` and has any bit set in 0o077, naming the mode
# and the chmod. The explicit chmod below is what keeps that refusal from firing.
#
# The flag spelling is also load-bearing for a gate: `tests/deploy_scripts_gate.sh`
# scans the shipped compose for `--<name>-file` rows and floors the count, and
# `migrate --database-url-file` is the only row it finds. Renaming the flag here
# would empty that scan and the gate would report the same clean line over nothing.
set -euo pipefail

DSN_FILE=""
MIGRATIONS_DIR="/db/migrations-ts"
VERB="apply"
PASSTHROUGH=()

while [ "$#" -gt 0 ]; do
  case "$1" in
    --database-url-file) DSN_FILE="$2"; shift 2 ;;
    --migrations-dir)    MIGRATIONS_DIR="$2"; shift 2 ;;
    --verb)              VERB="$2"; shift 2 ;;
    *)                   PASSTHROUGH+=("$1"); shift ;;
  esac
done

if [ -z "$DSN_FILE" ]; then
  echo "zeroship-migrate-platform: --database-url-file <PATH> is required" >&2
  exit 2
fi
if [ ! -r "$DSN_FILE" ]; then
  echo "zeroship-migrate-platform: cannot read $DSN_FILE" >&2
  exit 2
fi

# `tr -d` the trailing newline the secret writer leaves; a DSN with a newline in
# it fails at connect with a message that names neither the file nor the newline.
DSN="$(tr -d '\r\n' < "$DSN_FILE")"
if [ -z "$DSN" ]; then
  echo "zeroship-migrate-platform: $DSN_FILE is empty" >&2
  exit 2
fi

CFG_DIR="$(mktemp -d)"
chmod 700 "$CFG_DIR"
trap 'rm -rf "$CFG_DIR"' EXIT HUP INT TERM
{
  printf '[env.platform]\n'
  printf 'url = "%s"\n' "$DSN"
  printf 'dir = "%s"\n' "$MIGRATIONS_DIR"
  printf 'schema = "zeroship"\n'
  printf 'owner_app = "zeroship_platform"\n'
  printf 'registry = "/policies/platform-table-owners.json"\n'
  printf 'policy = ["/policies/platform.policy.toml"]\n'
} > "$CFG_DIR/zero-migrate.toml"
chmod 600 "$CFG_DIR/zero-migrate.toml"

APPROVE=()
case "$VERB" in
  apply|rollback|resolve|baseline) APPROVE=(--approve) ;;
esac

# ADOPTION IS NOT AUTOMATIC AND MUST NOT BECOME SO. A database that was migrated
# by the retired Rust one-shot carries journal rows this corpus does not produce,
# and the CLI will correctly report them as pending. The fix is ONE operator
# command, run by hand, once:
#
#   docker compose run --rm migrate --verb baseline --supersede-unmatched \
#       --database-url-file /etc/zeroship/secrets/migrate-dsn
#
# It is reachable from here (that is what `--verb` is for) and it is deliberately
# not what `apply` falls back to: a deploy path that silently adopts whatever it
# finds cannot tell a legitimate adoption from a database that is not what the
# corpus produces, and refusing the second is the whole value of the guard
# (crates/zeroship-migrate-node/src/verbs.rs, assert_corpus_output_is_live).
# NOT `exec`: it replaces this process and the EXIT trap never fires, leaving the
# 0600 DSN config behind in the container filesystem. Running the CLI as a child
# costs one process and lets the trap remove it.
node /app/packages/zero-migrate-cli/dist/cli-bin.js "$VERB" \
  --config "$CFG_DIR/zero-migrate.toml" \
  --env platform \
  "${APPROVE[@]}" \
  "${PASSTHROUGH[@]}"
