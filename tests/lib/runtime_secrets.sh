#!/usr/bin/env bash
# Generated inputs for shell harnesses that boot the multi-service platform.
#
# Source this file and call `e2e_export_runtime_secrets "$WORK"` after the
# harness has assigned its own key variables. Existing strong values are kept;
# missing or weak fixture values are replaced. The function exports the real
# service inputs, so every normal startup guard remains active.
#
# EVERY name below is the canonical `ZEROSHIP_` projection of a declared
# setting, and there is no second spelling. The bare middle names this file
# used to export - the unprefixed control-key, master-key and signing-key-file
# spellings - are not read by any binary any more: Step 5 of
# docs/proposals/2026-08-11-config-name-alignment.md deleted them along with
# the value flags that shared them. Adding one back here would not restore a
# fallback, it would just set a variable nothing reads, and the harness would
# fail at the startup guard several hundred lines later.
#
# Every name below is exported. Nothing here is withheld from a child any
# more: the one name that was, the platform mint key, authenticated control to
# an auth endpoint that no longer exists, so the pair of launcher wrappers that
# handed it to two services and scrubbed it from the rest went with it.

# Repo root + the vendored `jose` build, derived from THIS file's location so a
# harness that sources only this file still gets both. `tests/lib/e2e_stack.sh`
# assigns the same two names before it sources this file, and those assignments
# win.
if [ -z "${E2E_ROOT:-}" ]; then
  E2E_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
fi
E2E_JOSE_JS="${E2E_JOSE_JS:-$E2E_ROOT/node_modules/.pnpm/jose@6.2.3/node_modules/jose/dist/webapi/index.js}"

_e2e_strong_value() {
  local value="${1:-}"
  [ "${#value}" -ge 32 ] || return 1
  case "$value" in
    platform-key|dev-secret|dev-stash-key-please-rotate|\
    dev-stash-signing-key-not-for-production|\
    dev-only-stash-signing-key-not-for-production-use\!\!|\
    dev-pairwise-salt-never-rotate-in-prod|\
    dev-worker-key-not-for-production-use)
      return 1
      ;;
  esac
  return 0
}

_e2e_keep_or_generate() {
  local name="$1" current="${2:-}" generated
  if _e2e_strong_value "$current"; then
    printf -v "$name" '%s' "$current"
  else
    generated="$(openssl rand -hex 32)" || return 1
    printf -v "$name" '%s' "$generated"
  fi
  export "$name"
}

_e2e_keep_or_generate_hex_key() {
  local name="$1" current="${2:-}" generated
  if [[ "$current" =~ ^[0-9a-fA-F]{64}$ ]] &&
     [ "$current" != "00000000000000000000000000000000000000000000000000000000000000ff" ]; then
    printf -v "$name" '%s' "$current"
  else
    generated="$(openssl rand -hex 32)" || return 1
    printf -v "$name" '%s' "$generated"
  fi
  export "$name"
}

# ---------------------------------------------------------------------------
# The harness's own platform OP.
#
# Control accepts exactly ONE principal credential: a platform OAuth access
# token. `crates/core/src/auth_provider/platform.rs` requires an `at+jwt` typ
# header, an EdDSA signature under a `kid` published in the issuer's JWKS, an
# `iss` equal to `ZEROSHIP_AUTH_PLATFORM_ISSUER`, and the registered claims
# `exp iss aud sub iat jti client_id scope`; control then checks the audience
# against its own `--oauth-audience` and turns `scope` into the token policy
# (`crates/authn/src/lib.rs`, `oauth_guard_from_bearer`).
#
# The personal access token these harnesses used to sign for themselves is gone
# along with `zeroship.permission_tokens`, so a harness that needs an authorized
# caller has to BE an issuer. That is what this pair does: `e2e_platform_op_up`
# publishes the workspace ed25519 key as a one-key JWKS on a loopback port and
# names that origin as the issuer, and `e2e_mint_platform_bearer` signs a token
# with the same key under the matching `kid`.
#
# Both halves run out of ONE generated node module, so the `kid` in the JWKS and
# the `kid` in the token header agree by construction rather than by two copies
# of a thumbprint expression staying in step.
#
# `e2e_platform_op_up` must run BEFORE control starts: control reads the issuer
# once at boot and fetches the JWKS over HTTP on the first bearer it sees.
#
# The port is EPHEMERAL by default -- the server binds 0 and reports what the
# kernel gave it, and only then is the issuer named. Every other port in these
# harnesses is a fixed number each file picks so two harnesses can run at once,
# and a single hard-coded default here would have undone that for all of them.
# Set E2E_PLATFORM_OP_PORT to pin one anyway. Exports:
# ZEROSHIP_AUTH_PLATFORM_ISSUER, E2E_PLATFORM_OP_JS, E2E_PLATFORM_OP_PID,
# E2E_PLATFORM_OP_PORT.
#
# The default client id is `zeroship-console`, NOT `zeroship-cli`. The CLI id is
# the one control intersects against the principal's live
# `zeroship.principal_grants` rows, and an unseeded principal falls back to
# `PLATFORM_CLI_ISSUABLE_SCOPES` -- apps:deploy apps:read apps:write
# secrets:read -- which would silently drop apps:delete, env:*, secrets:write
# and every billing scope a harness asks for.
E2E_PLATFORM_OP_CLIENT_ID="zeroship-console"

_e2e_write_platform_op_js() {
  local path="$1"
  cat > "$path" <<'PLATFORM_OP_JS'
// Generated by tests/lib/runtime_secrets.sh. Two modes, one key, one kid:
//   node platform-op.mjs serve
//   node platform-op.mjs mint <subject> <scope> <client-id> <ttl-seconds>
import { createServer } from "node:http";
import { readFileSync, writeFileSync } from "node:fs";
import { createHash, randomUUID } from "node:crypto";

const jose = await import(process.env.E2E_PLATFORM_OP_JOSE);
const key = await jose.importPKCS8(
  readFileSync(process.env.E2E_PLATFORM_OP_KEY, "utf8"),
  "EdDSA",
  { extractable: true },
);
const x = (await jose.exportJWK(key)).x;
// RFC 7638 JWK thumbprint over the required Ed25519 members, in lexical order.
const kid = createHash("sha256")
  .update(`{"crv":"Ed25519","kty":"OKP","x":"${x}"}`)
  .digest("base64url");

const mode = process.argv[2];

if (mode === "serve") {
  const body = JSON.stringify({
    keys: [{ kid, kty: "OKP", crv: "Ed25519", alg: "EdDSA", use: "sig", x }],
  });
  const server = createServer((req, res) => {
    const path = (req.url || "").split("?")[0];
    if (path === "/oauth2/.well-known/jwks.json") {
      res.writeHead(200, { "content-type": "application/json" });
      res.end(body);
      return;
    }
    res.writeHead(404, { "content-type": "application/json" });
    res.end('{"error":"not_found"}');
  });
  // Port 0 unless the caller pinned one: the kernel picks a free port and the
  // shell reads it back out of this file, so two harnesses never contend.
  server.listen(Number(process.env.E2E_PLATFORM_OP_PORT || 0), "127.0.0.1", () => {
    writeFileSync(process.env.E2E_PLATFORM_OP_PORT_FILE, String(server.address().port));
  });
} else if (mode === "mint") {
  const [subject, scope, clientId, ttl] = process.argv.slice(3);
  const now = Math.floor(Date.now() / 1000);
  const jwt = await new jose.SignJWT({
    client_id: clientId,
    scope,
    jti: randomUUID(),
  })
    .setProtectedHeader({ alg: "EdDSA", typ: "at+jwt", kid })
    .setIssuer(process.env.E2E_PLATFORM_OP_ISSUER)
    .setAudience(process.env.E2E_PLATFORM_OP_AUDIENCE)
    .setSubject(subject)
    .setIssuedAt(now)
    .setNotBefore(now - 1)
    .setExpirationTime(now + Number(ttl))
    .sign(key);
  process.stdout.write(jwt);
} else {
  process.stderr.write(`platform-op.mjs: unknown mode ${mode}\n`);
  process.exit(2);
}
PLATFORM_OP_JS
}

# e2e_platform_op_up <signing-key.pem> <workdir>
#
# A harness that already runs the REAL `zeroship-auth` OP and wants control to
# trust IT pins ZEROSHIP_AUTH_PLATFORM_ISSUER to that OP's origin BEFORE calling
# here (tests/e2e_device_login.sh does, and asserts a real token's `iss` against
# it). This function then configures the minter against the pinned issuer and
# starts no server of its own: two issuers on one control plane is not a shape
# control has, so the harness picks one.
e2e_platform_op_up() {
  local key="$1" dir="$2" i
  [ -s "$key" ] || {
    echo "e2e_platform_op_up: no signing key at $key" >&2
    return 1
  }
  [ -n "$dir" ] || {
    echo "e2e_platform_op_up: a workspace directory is required" >&2
    return 1
  }
  local pinned_issuer="${ZEROSHIP_AUTH_PLATFORM_ISSUER:-}"
  E2E_PLATFORM_OP_KEY="$key"
  E2E_PLATFORM_OP_JOSE="file://$E2E_JOSE_JS"
  # Control's `--oauth-audience` default. A token minted for anything else is
  # rejected with `wrong_audience` before the scope is ever read.
  E2E_PLATFORM_OP_AUDIENCE="${E2E_PLATFORM_OP_AUDIENCE:-control.zeroship.ai}"
  E2E_PLATFORM_OP_JS="$dir/platform-op.mjs"
  E2E_PLATFORM_OP_PORT_FILE="$dir/platform-op.port"
  export E2E_PLATFORM_OP_KEY E2E_PLATFORM_OP_JOSE
  export E2E_PLATFORM_OP_AUDIENCE E2E_PLATFORM_OP_JS E2E_PLATFORM_OP_PORT_FILE

  mkdir -p "$dir"
  _e2e_write_platform_op_js "$E2E_PLATFORM_OP_JS" || return 1

  # The pinned-issuer path needs neither node nor jose, and tests/health_endpoints.sh
  # takes it precisely so the health contract stays testable on a checkout that
  # has never run `pnpm install`. So these two are checked HERE, on the path that
  # actually runs a node server, not at the top of the function.
  if [ -n "$pinned_issuer" ]; then
    E2E_PLATFORM_OP_ISSUER="$pinned_issuer"
    # Empty, not unset: a caller doing `PIDS+=($E2E_PLATFORM_OP_PID)` must add
    # nothing here rather than trip `set -u`.
    E2E_PLATFORM_OP_PID=""
    export E2E_PLATFORM_OP_ISSUER E2E_PLATFORM_OP_PID
    echo "  note: platform issuer pinned to $pinned_issuer; serving no harness JWKS" >&2
    return 0
  fi
  [ -f "$E2E_JOSE_JS" ] || {
    echo "e2e_platform_op_up: missing jose at $E2E_JOSE_JS (run pnpm install)" >&2
    return 1
  }
  command -v node >/dev/null 2>&1 || {
    echo "e2e_platform_op_up: node is required" >&2
    return 1
  }

  rm -f "$E2E_PLATFORM_OP_PORT_FILE"
  node "$E2E_PLATFORM_OP_JS" serve > "$dir/platform-op.log" 2>&1 &
  E2E_PLATFORM_OP_PID=$!
  export E2E_PLATFORM_OP_PID
  # Harnesses that use tests/lib/e2e_stack.sh let `stack_down` reap this; ones
  # that keep their own PID list add $E2E_PLATFORM_OP_PID to it.
  if [ -n "${PIDFILE:-}" ] && [ -f "$PIDFILE" ]; then
    echo "$E2E_PLATFORM_OP_PID" >> "$PIDFILE"
  fi

  for i in $(seq 1 80); do
    [ -s "$E2E_PLATFORM_OP_PORT_FILE" ] && break
    sleep 0.25
  done
  E2E_PLATFORM_OP_PORT="$(cat "$E2E_PLATFORM_OP_PORT_FILE" 2>/dev/null)"
  # 127.0.0.1 rather than localhost: the issuer string is compared byte-for-byte
  # against the token's `iss`, and the JWKS is fetched from the same origin, so
  # a name that may resolve to ::1 on one host and 127.0.0.1 on another is a
  # portability hazard for no gain.
  E2E_PLATFORM_OP_ISSUER="http://127.0.0.1:$E2E_PLATFORM_OP_PORT/oauth2"
  export E2E_PLATFORM_OP_PORT E2E_PLATFORM_OP_ISSUER
  if [ -z "$E2E_PLATFORM_OP_PORT" ] ||
     ! curl -sf "$E2E_PLATFORM_OP_ISSUER/.well-known/jwks.json" >/dev/null 2>&1; then
    echo "e2e_platform_op_up: JWKS never answered (port='${E2E_PLATFORM_OP_PORT:-unbound}')" >&2
    tail -10 "$dir/platform-op.log" >&2 2>/dev/null || true
    return 1
  fi

  ZEROSHIP_AUTH_PLATFORM_ISSUER="$E2E_PLATFORM_OP_ISSUER"
  export ZEROSHIP_AUTH_PLATFORM_ISSUER
  return 0
}

# e2e_mint_platform_bearer <subject-uuid> <scope> [client-id] [ttl-seconds]
#
# Echoes the access token on stdout. Seeding the principal row is the CALLER's
# job -- control resolves `sub` against `zeroship.users` and refuses a missing,
# disabled, pending-deletion or anonymized principal.
e2e_mint_platform_bearer() {
  local subject="$1" scope="$2"
  local client_id="${3:-$E2E_PLATFORM_OP_CLIENT_ID}" ttl="${4:-3600}"
  [ -n "${E2E_PLATFORM_OP_JS:-}" ] && [ -f "${E2E_PLATFORM_OP_JS:-}" ] || {
    echo "e2e_mint_platform_bearer: call e2e_platform_op_up first" >&2
    return 1
  }
  [ -n "$subject" ] || {
    echo "e2e_mint_platform_bearer: a subject uuid is required" >&2
    return 1
  }
  [ -n "$scope" ] || {
    echo "e2e_mint_platform_bearer: a scope string is required" >&2
    return 1
  }
  node "$E2E_PLATFORM_OP_JS" mint "$subject" "$scope" "$client_id" "$ttl"
}

_e2e_secret_file() {
  local path="$1" current_hex sentinel_hex
  current_hex="$(od -An -tx1 -v "$path" 2>/dev/null | tr -d ' \n')"
  sentinel_hex="$(printf '%s' 'dev-broker-master-secret-never-use-prod' | od -An -tx1 -v | tr -d ' \n')"
  if [ ! -s "$path" ] ||
     [ "$(wc -c < "$path" 2>/dev/null || echo 0)" -lt 32 ] ||
     [ "$current_hex" = "$sentinel_hex" ] ||
     [ -z "${current_hex//0/}" ]; then
    openssl rand -base64 48 > "$path" || return 1
  fi
  chmod 600 "$path"
}

# The owner_app every platform migration is stamped with.
#
# It is a CONSTANT, not a knob, and it is load-bearing twice over: the engine
# derives each migration's journal version from `owner_app + name`
# (crates/zeroship-migrate-core/src/render/lower.rs:8806-8811), so changing it
# re-mints every version and re-applies the whole corpus against a database that
# already has it; and it is the value `policies/platform-table-owners.json` maps
# every platform table to, so the two must agree or every op is refused as
# targeting a table owned by someone else.
ZS_PLATFORM_OWNER_APP="zeroship_platform"

# zs_platform_migrate <dsn> [--migrations-dir DIR] [--project-schema S]
#                           [--project-id P] [extra zero-migrate flags ...]
#
# Apply the platform schema (db/migrations-ts) with the DSN off the argv.
#
# WHAT CHANGED, AND WHY THE FIRST ARGUMENT IS GONE. This used to take a path to
# the `zeroship-platform-migrate` binary and hand it `--database-url-file`. That
# binary was deleted on 2026-08-28; the platform schema is applied by the general
# `zero-migrate` CLI (packages/zero-migrate-cli), which is a Node program built by
# `pnpm --filter zero-migrate-cli build`. There is no binary to name any more, so
# the parameter that named one is gone rather than kept and ignored.
#
# THE DSN STILL NEVER REACHES argv, and that is the whole reason this function
# exists rather than a bare call. The DSN these harnesses pass is the postgres
# SUPERUSER one, and an argument list is public to every process in the PID
# namespace and to `ps` for every user on the box. The CLI advertises
# `--database-url <value>`; THAT SPELLING IS NEVER USED HERE. The DSN is written
# into a per-run `zero-migrate.toml` and passed as `--config <path>`.
#
# THE OWNER-ONLY REFUSAL SURVIVED THE MOVE, and it is enforced by the reader, not
# by this function. `packages/zero-migrate-cli/src/config.ts` (`enforceOwnerOnly`)
# refuses a config file that supplies a LITERAL `url` and has any bit set in
# 0o077, naming the mode and the chmod - the same contract the deleted binary got
# from `crates/core/src/config/secrets.rs`. Measured 2026-08-28 with one variable
# between the arms: the same file at 0600 is accepted and the run proceeds to the
# corpus; at 0644 the CLI exits 1 with
#   config file ... supplies a literal database url and has mode 0644; group and
#   other permissions must be zero (chmod 600 '...')
# A `url = "env:NAME"` reference at 0644 is deliberately NOT refused - it carries
# no credential - which is the control that shows the rule is scoped to secrets
# rather than to every config file.
#
# THE 0600 ARM USED TO CLAIM IT REPORTED `status: 35 applied, 0 pending`, and it
# cannot have. On the day that was written the corpus imported `@zeroship/migrate`
# and no Node applier could resolve it, so a run that got past this permission
# check still died on migration file #1. Only the part this function is
# responsible for - the mode check - was ever observed; the corpus half was
# carried over from a path that no longer existed. What the corpus actually does
# is measured by `tests/platform_migration_corpus_gate.sh`, which applies it.
#
# WHERE THE FILE LIVES, AND WHO REMOVES IT. Both are in THIS function, so the
# lifetime is one invocation and no caller has to remember a cleanup step:
#
#   - PER RUN, never a fixed path. `mktemp -d` picks the directory, so two
#     harnesses - or two agents running the SAME harness - cannot collide.
#   - The DIRECTORY is 0700 and the file 0600, both set EXPLICITLY. `mktemp -d`
#     already creates at 0700, but the mode is not incidental: the CLI refuses on
#     it, so a future edit that changes how the file is created cannot silently
#     produce a world-readable one that fails as "the migrate tool is broken".
#   - Removed unconditionally after the child exits, on the failure arm too, and
#     the child's exit status is what this function returns.
#
# DOES NOT COVER: a harness SIGKILLed between the write and the `rm` leaves one
# 0600 file behind in $TMPDIR. That is a leaked file readable only by the user who
# ran the suite, which is strictly less exposure than the argv form gave every
# user on the box for the whole life of every run.
#
# The DSN is passed as an ARGUMENT to this function and never through the
# environment: an exported name would be inherited by every other child the
# harness spawns, which is the exposure this exists to remove.
#
# --project-id IS VALIDATED, NOT IGNORED. The retired binary took a separate
# project id for the advisory-lock key and the journal. The CLI has no such flag:
# its lock key IS the project schema (`owner_app_project()`,
# crates/zeroship-migrate-node/src/verbs.rs:1142-1148). Every caller in this tree
# passes the same value for both, so the flag is accepted and checked for
# agreement rather than silently dropped - a caller that ever passes two different
# values is asking for something the CLI cannot express and should be told so.
zs_platform_migrate() {
  if [ "$#" -lt 1 ]; then
    echo "zs_platform_migrate: usage: zs_platform_migrate <dsn> [flag ...]" >&2
    return 2
  fi
  local dsn="$1"; shift
  if [ -z "$dsn" ]; then
    echo "zs_platform_migrate: refusing to write an empty DSN file" >&2
    return 2
  fi

  local root migrations_dir schema project_id
  root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
  migrations_dir="$root/db/migrations-ts"
  schema="zeroship"
  project_id=""

  local passthrough=()
  while [ "$#" -gt 0 ]; do
    case "$1" in
      --migrations-dir) migrations_dir="$2"; shift 2 ;;
      --project-schema) schema="$2"; shift 2 ;;
      --project-id) project_id="$2"; shift 2 ;;
      *) passthrough+=("$1"); shift ;;
    esac
  done
  if [ -n "$project_id" ] && [ "$project_id" != "$schema" ]; then
    echo "zs_platform_migrate: --project-id '$project_id' differs from --project-schema" \
         "'$schema'. The zero-migrate CLI derives its advisory-lock key and journal" \
         "project from the schema alone, so two different values cannot be honoured." >&2
    return 2
  fi

  local cli="$root/packages/zero-migrate-cli/dist/cli-bin.js"
  if [ ! -f "$cli" ]; then
    echo "zs_platform_migrate: $cli is missing - run: pnpm install && pnpm build &&" \
         "pnpm --filter zero-migrate-cli build" >&2
    return 2
  fi

  local cfg_dir rc
  cfg_dir="$(mktemp -d "${TMPDIR:-/tmp}/zeroship-migrate-cfg.XXXXXXXX")" || return 1
  chmod 700 "$cfg_dir" || { rm -rf "$cfg_dir"; return 1; }
  # `printf %s` on the DSN, never `echo`: a DSN may contain a backslash and echo
  # would interpret it on some shells.
  {
    printf '[env.platform]\n'
    printf 'url = "%s"\n' "$dsn"
    printf 'dir = "%s"\n' "$migrations_dir"
    printf 'schema = "%s"\n' "$schema"
    printf 'owner_app = "%s"\n' "$ZS_PLATFORM_OWNER_APP"
    printf 'registry = "%s"\n' "$root/policies/platform-table-owners.json"
    printf 'policy = ["%s"]\n' "$root/policies/platform.policy.toml"
  } > "$cfg_dir/zero-migrate.toml" || { rm -rf "$cfg_dir"; return 1; }
  chmod 600 "$cfg_dir/zero-migrate.toml" || { rm -rf "$cfg_dir"; return 1; }

  node "$cli" apply --config "$cfg_dir/zero-migrate.toml" --env platform --approve \
    "${passthrough[@]}"
  rc=$?
  rm -rf "$cfg_dir"
  return "$rc"
}


# e2e_export_database_urls <dsn>
#
# One admin DSN, seven canonical names. The services' database roles are DISTINCT
# settings - control writes the registry, the gateway only reads sessions, the
# worker runs creator SQL - so each has its own identity and there is no shared
# middle name to point them all at. A harness that runs the whole stack against
# one ephemeral Postgres says so once, here, instead of restating a `--db` on
# every launch line. The worker is the exception: app code runs in its process,
# so this helper always replaces the admin userinfo with its constrained role.
# Pass another per-service DSN by setting that name after this call.
_e2e_database_url_for_role() {
  local dsn="$1" role="$2" password="$3" scheme rest host_and_path
  case "$dsn" in
    postgres://*|postgresql://*) ;;
    *)
      echo "e2e_export_database_urls: expected a postgres:// DSN" >&2
      return 1
      ;;
  esac
  scheme="${dsn%%://*}"
  rest="${dsn#*://}"
  host_and_path="${rest#*@}"
  if [ "$host_and_path" = "$rest" ]; then
    host_and_path="$rest"
  fi
  printf '%s://%s:%s@%s' "$scheme" "$role" "$password" "$host_and_path"
}

e2e_export_database_urls() {
  local dsn="${1:-}"
  [ -n "$dsn" ] || {
    echo "e2e_export_database_urls: a DSN is required" >&2
    return 1
  }
  ZEROSHIP_CONTROL_DATABASE_URL="$dsn"
  ZEROSHIP_GATEWAY_DATABASE_URL="$dsn"
  ZEROSHIP_WORKER_DATABASE_URL="$(_e2e_database_url_for_role "$dsn" zeroship_worker zeroship_worker)" || return 1
  ZEROSHIP_AUTH_DATABASE_URL="$dsn"
  ZEROSHIP_MIGRATE_SERVER_DATABASE_URL="$dsn"
  ZEROSHIP_MIGRATE_SERVER_PROVISION_DATABASE_URL="$dsn"
  ZEROSHIP_WORKFLOW_SCHEDULER_DATABASE_URL="$dsn"
  export ZEROSHIP_CONTROL_DATABASE_URL ZEROSHIP_GATEWAY_DATABASE_URL
  export ZEROSHIP_WORKER_DATABASE_URL ZEROSHIP_AUTH_DATABASE_URL
  export ZEROSHIP_MIGRATE_SERVER_DATABASE_URL ZEROSHIP_MIGRATE_SERVER_PROVISION_DATABASE_URL
  export ZEROSHIP_WORKFLOW_SCHEDULER_DATABASE_URL
}

e2e_export_runtime_secrets() {
  local secret_dir="$1"
  [ -n "$secret_dir" ] || {
    echo "e2e_export_runtime_secrets: a workspace directory is required" >&2
    return 1
  }
  command -v openssl >/dev/null 2>&1 || {
    echo "e2e_export_runtime_secrets: openssl is required" >&2
    return 1
  }
  mkdir -p "$secret_dir"

  _e2e_keep_or_generate ZEROSHIP_CONTROL_KEY "${ZEROSHIP_CONTROL_KEY:-}" || return 1
  _e2e_keep_or_generate_hex_key ZEROSHIP_CONTROL_MASTER_KEY \
    "${ZEROSHIP_CONTROL_MASTER_KEY:-}" || return 1
  _e2e_keep_or_generate ZEROSHIP_WORKER_KEY "${ZEROSHIP_WORKER_KEY:-}" || return 1
  _e2e_keep_or_generate ZEROSHIP_PAIRWISE_SALT "${ZEROSHIP_PAIRWISE_SALT:-}" || return 1
  _e2e_keep_or_generate ZEROSHIP_MIGRATE_SERVER_POLICY_SEAL_KEY \
    "${ZEROSHIP_MIGRATE_SERVER_POLICY_SEAL_KEY:-}" || return 1
  _e2e_keep_or_generate_hex_key ZEROSHIP_AUTH_TOTP_ENC_KEY \
    "${ZEROSHIP_AUTH_TOTP_ENC_KEY:-}" || return 1

  # The stash HMAC is one deployment fact with two canonical consumers: the
  # gateway mints the cookie and the OP verifies it, so the two names carry
  # identical material. They are separate settings because the two services
  # can be rolled independently, not because the value may differ.
  _e2e_keep_or_generate ZEROSHIP_GATEWAY_STASH_SIGNING_KEY \
    "${ZEROSHIP_GATEWAY_STASH_SIGNING_KEY:-}" || return 1
  ZEROSHIP_AUTH_STASH_SIGNING_KEY="${ZEROSHIP_AUTH_STASH_SIGNING_KEY:-$ZEROSHIP_GATEWAY_STASH_SIGNING_KEY}"
  export ZEROSHIP_AUTH_STASH_SIGNING_KEY

  if ! _e2e_strong_value "${ZEROSHIP_CONTROL_STRIPE_WEBHOOK_SECRET:-}"; then
    ZEROSHIP_CONTROL_STRIPE_WEBHOOK_SECRET="whsec_$(openssl rand -hex 32)" || return 1
  fi
  export ZEROSHIP_CONTROL_STRIPE_WEBHOOK_SECRET

  # The issuer whose tokens control accepts, and whose JWKS it fetches to verify
  # them. A harness that runs a real auth binary sets this to that binary's
  # origin before calling here; one that does not calls `e2e_platform_op_up`,
  # which points it at the harness's own JWKS. This default -- the auth port,
  # which nothing may be serving -- is the last resort, and a stack that lands
  # on it gets `platform_token_verification_failed` on its first admin call.
  ZEROSHIP_AUTH_PLATFORM_ISSUER="${ZEROSHIP_AUTH_PLATFORM_ISSUER:-http://localhost:${AUTH_PORT:-9092}/oauth2}"
  ZEROSHIP_ORIGIN_SCHEME="${ZEROSHIP_ORIGIN_SCHEME:-http}"
  ZEROSHIP_CONTROL_ALLOW_UNSUPPORTED_BILLING="${ZEROSHIP_CONTROL_ALLOW_UNSUPPORTED_BILLING:-true}"
  export ZEROSHIP_AUTH_PLATFORM_ISSUER
  export ZEROSHIP_ORIGIN_SCHEME ZEROSHIP_CONTROL_ALLOW_UNSUPPORTED_BILLING

  # The gateway's wrapper-token issuer. It is the ONLY surviving consumer of a
  # `signing_key_file` in this stack: control's and migrated's existed to build
  # the personal-access-token issuer and went with it. `signing_key_file` is
  # Operational<PathBuf>: a path to key material is not itself key material.
  ZEROSHIP_GATEWAY_SIGNING_KEY_FILE="${ZEROSHIP_GATEWAY_SIGNING_KEY_FILE:-$secret_dir/platform-signing.pem}"
  if [ ! -s "$ZEROSHIP_GATEWAY_SIGNING_KEY_FILE" ]; then
    openssl genpkey -algorithm ed25519 -out "$ZEROSHIP_GATEWAY_SIGNING_KEY_FILE" 2>/dev/null || return 1
  fi
  chmod 600 "$ZEROSHIP_GATEWAY_SIGNING_KEY_FILE"
  export ZEROSHIP_GATEWAY_SIGNING_KEY_FILE

  ZEROSHIP_GATEWAY_BROKER_SECRET_FILE="${ZEROSHIP_GATEWAY_BROKER_SECRET_FILE:-$secret_dir/gateway-broker-secret}"
  _e2e_secret_file "$ZEROSHIP_GATEWAY_BROKER_SECRET_FILE" || return 1
  export ZEROSHIP_GATEWAY_BROKER_SECRET_FILE

  ZEROSHIP_AUTH_SIGNING_KEY_FILE="${ZEROSHIP_AUTH_SIGNING_KEY_FILE:-$secret_dir/auth-signing.pem}"
  if [ ! -s "$ZEROSHIP_AUTH_SIGNING_KEY_FILE" ]; then
    openssl genpkey -algorithm ed25519 -out "$ZEROSHIP_AUTH_SIGNING_KEY_FILE" 2>/dev/null || return 1
  fi
  chmod 600 "$ZEROSHIP_AUTH_SIGNING_KEY_FILE"
  export ZEROSHIP_AUTH_SIGNING_KEY_FILE

  ZEROSHIP_AUTH_PAIRWISE_SALT_FILE="${ZEROSHIP_AUTH_PAIRWISE_SALT_FILE:-$secret_dir/auth-pairwise-salt}"
  ZEROSHIP_AUTH_BROKER_SECRET_FILE="${ZEROSHIP_AUTH_BROKER_SECRET_FILE:-$ZEROSHIP_GATEWAY_BROKER_SECRET_FILE}"
  ZEROSHIP_AUTH_REFRESH_HASH_KEY_FILE="${ZEROSHIP_AUTH_REFRESH_HASH_KEY_FILE:-$secret_dir/refresh-hash-key}"
  ZEROSHIP_AUTH_REFRESH_IDEM_KEY_FILE="${ZEROSHIP_AUTH_REFRESH_IDEM_KEY_FILE:-$secret_dir/refresh-idem-key}"
  printf '%s' "$ZEROSHIP_PAIRWISE_SALT" > "$ZEROSHIP_AUTH_PAIRWISE_SALT_FILE" || return 1
  chmod 600 "$ZEROSHIP_AUTH_PAIRWISE_SALT_FILE"
  # The OP and gateway derive per-client broker secrets from identical bytes.
  # Keep an explicit auth override when a harness supplied one; otherwise both
  # services consume the gateway file generated above.
  _e2e_secret_file "$ZEROSHIP_AUTH_BROKER_SECRET_FILE" || return 1
  if ! grep -Eq '^[1-9][0-9]*:[A-Za-z0-9_-]{43,}$' "$ZEROSHIP_AUTH_REFRESH_HASH_KEY_FILE" 2>/dev/null; then
    printf '1:%s' "$(openssl rand -hex 32)" > "$ZEROSHIP_AUTH_REFRESH_HASH_KEY_FILE" || return 1
  fi
  chmod 600 "$ZEROSHIP_AUTH_REFRESH_HASH_KEY_FILE"
  _e2e_secret_file "$ZEROSHIP_AUTH_REFRESH_IDEM_KEY_FILE" || return 1
  export ZEROSHIP_AUTH_PAIRWISE_SALT_FILE ZEROSHIP_AUTH_BROKER_SECRET_FILE
  export ZEROSHIP_AUTH_REFRESH_HASH_KEY_FILE ZEROSHIP_AUTH_REFRESH_IDEM_KEY_FILE
}
