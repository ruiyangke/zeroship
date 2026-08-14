#!/usr/bin/env bash
#
# Deploy a creator app from this machine to a remote zeroship host.
#
#   deploy/scripts/deploy-app.sh --host root@1.2.3.4 --app <app-id> --dir examples/db-todos \
#                                --probe https://db-todos.example.com
#   deploy/scripts/deploy-app.sh --host root@1.2.3.4 --app <app-id> --zship dist/app.zship
#                                --probe-script examples/db-todos/scripts/probe-live.mjs
#
# WHY THIS EXISTS, 2026-08-12. `zeroship deploy` needs to reach the control
# plane, and on a correctly configured host it CANNOT:
#
#     control publishes 127.0.0.1:9090 only, and Caddy has no control route.
#     Measured from a remote machine: `control.<domain>` -> 404 (it falls
#     through to the creator-app wildcard), direct :9090 -> refused.
#
# That is the right posture -- the control plane is the whole platform's
# authority and does not belong on the internet -- but it left app deploys
# with no supported path at all. The golden path still lists "smooth one-step
# deploy" as an open box.
#
# The fix is an SSH port-forward: the control plane stays loopback-only and
# gains no public surface, and the deploy borrows the authentication that is
# already there (key-based ssh). Nothing new is exposed and no new secret is
# introduced.
#
# TEARDOWN IS BY PID, NOT BY PATTERN. Killing the forward with `pkill -f`
# matching the port silently missed it during development and left a listener
# on the box; the port stayed open with no process anyone was looking for.
# The PID is captured and killed from a trap that runs on every exit path.
#
# THE TOKEN IS NEVER AN ARGUMENT. It is read from ZEROSHIP_TOKEN or a file and
# passed to the CLI through the environment, so it cannot be read out of `ps`
# by any other user on this machine.
#
# WHAT THIS DOES NOT DO: it does not create the app, run migrations, or manage
# DNS. `--probe` alone asserts only liveness -- a 200 from an SPA shell says
# the asset route works and nothing about the app. Add `--probe-script` for a
# real behavioural check, and it is only as good as the script you point at.

set -euo pipefail

HOST=""
APP_ID=""
APP_DIR=""
ZSHIP=""
PROBE_URL=""
TOKEN_FILE=""
REMOTE_CONTROL_PORT=9090
SKIP_BUILD=0
PROBE_SCRIPT=""

usage() { sed -n '2,42p' "$0" | sed 's/^# \{0,1\}//'; exit "${1:-0}"; }

while [ $# -gt 0 ]; do
  case "$1" in
    --host)       HOST="$2"; shift 2 ;;
    --app)        APP_ID="$2"; shift 2 ;;
    --dir)        APP_DIR="$2"; shift 2 ;;
    --zship)      ZSHIP="$2"; shift 2 ;;
    --probe)      PROBE_URL="$2"; shift 2 ;;
    --probe-script) PROBE_SCRIPT="$2"; shift 2 ;;
    --token-file) TOKEN_FILE="$2"; shift 2 ;;
    --control-port) REMOTE_CONTROL_PORT="$2"; shift 2 ;;
    --skip-build) SKIP_BUILD=1; shift ;;
    -h|--help)    usage 0 ;;
    *) echo "unknown argument: $1" >&2; usage 1 ;;
  esac
done

say()  { printf '\n== %s\n' "$*"; }
fail() { printf 'FAIL: %s\n' "$*" >&2; exit 1; }

[ -n "$HOST" ]   || fail "--host is required"
[ -n "$APP_ID" ] || fail "--app is required"
[ -n "$APP_DIR" ] || [ -n "$ZSHIP" ] || fail "one of --dir or --zship is required"

ROOT="$(git rev-parse --show-toplevel)"
cd "$ROOT"

# --------------------------------------------------------------- credentials
if [ -n "$TOKEN_FILE" ]; then
  [ -r "$TOKEN_FILE" ] || fail "cannot read token file: $TOKEN_FILE"
  ZEROSHIP_TOKEN="$(cat "$TOKEN_FILE")"
  export ZEROSHIP_TOKEN
fi
[ -n "${ZEROSHIP_TOKEN:-}" ] \
  || fail "no token. Set ZEROSHIP_TOKEN, or pass --token-file, or run 'zeroship login'.
  The token is deliberately not accepted as an argument: arguments are visible
  in the process list to every user on this machine."

SSH_OPTS=(-o BatchMode=yes -o ConnectTimeout=15 -o ExitOnForwardFailure=yes)

# The forward MUST own its own connection. With `ControlMaster auto` in an
# operator's ~/.ssh/config -- which is common, and is set on the machine this
# was written on -- `ssh -L` hands the forward to the existing mux master and
# the process we backgrounded exits immediately. `$!` then names a dead
# process, the trap's kill is a no-op, and the forward outlives the script by
# ControlPersist (10m here), holding a route to the remote control plane that
# nothing is watching.
#
# MEASURED: without these two options the script exits 1 and
#   ss -tlnp | grep <port>  ->  users:(("ssh",pid=...)) ... [mux]
# with ppid 1. With them, the port is free the moment the script returns.
TUNNEL_SSH_OPTS=("${SSH_OPTS[@]}" -o ControlMaster=no -o ControlPath=none)

# ---------------------------------------------------------------- preflight
say "preflight"
ssh "${SSH_OPTS[@]}" "$HOST" true || fail "cannot ssh to $HOST (ssh-add -l to check your agent)"

CLI="$ROOT/target/release/zeroship"
[ -x "$CLI" ] || CLI="$ROOT/target/debug/zeroship"
[ -x "$CLI" ] || CLI="$(command -v zeroship || true)"
[ -n "$CLI" ] && [ -x "$CLI" ] || fail "no zeroship binary (build with: cargo build --release -p zeroship)"
echo "ok  ssh, cli at $CLI"

# -------------------------------------------------------------------- build
if [ -n "$APP_DIR" ] && [ "$SKIP_BUILD" = 0 ]; then
  say "building $APP_DIR"
  ( cd "$APP_DIR" && pnpm build ) || fail "app build failed"
  ZSHIP="${ZSHIP:-$APP_DIR/dist/app.zship}"
fi
[ -n "$ZSHIP" ] || ZSHIP="$APP_DIR/dist/app.zship"

# A deploy that silently ships a stale artifact is worse than one that fails:
# the build can succeed while writing somewhere else entirely.
[ -f "$ZSHIP" ] || fail "artifact not found: $ZSHIP
  The build can succeed without producing this file if the app's vite config
  writes elsewhere. Check the build output before assuming this is a bug here."
echo "ok  artifact $ZSHIP ($(du -h "$ZSHIP" | cut -f1))"

# ------------------------------------------------------------------- tunnel
# Bind the local end to 127.0.0.1 explicitly. A bare `-L port:` would listen on
# all interfaces and hand this machine's network a route to the remote control
# plane, which is the exact exposure the forward exists to avoid.
LOCAL_PORT="$(( 20000 + RANDOM % 20000 ))"
TUNNEL_PID=""
cleanup() {
  if [ -n "$TUNNEL_PID" ] && kill -0 "$TUNNEL_PID" 2>/dev/null; then
    kill "$TUNNEL_PID" 2>/dev/null || true
    wait "$TUNNEL_PID" 2>/dev/null || true
    echo "ok  tunnel closed (pid $TUNNEL_PID)"
  fi
}
trap cleanup EXIT INT TERM

say "opening a control-plane forward on 127.0.0.1:$LOCAL_PORT"
ssh "${TUNNEL_SSH_OPTS[@]}" -N -L "127.0.0.1:$LOCAL_PORT:127.0.0.1:$REMOTE_CONTROL_PORT" "$HOST" &
TUNNEL_PID=$!

CONTROL="http://127.0.0.1:$LOCAL_PORT"
ready=0
for _ in $(seq 1 30); do
  # Liveness first: a dead forward and a slow one both look like a failed
  # curl, and only one of them is worth waiting on.
  kill -0 "$TUNNEL_PID" 2>/dev/null || fail "the ssh forward exited before it was ready"
  if curl -fsS -o /dev/null --max-time 3 "$CONTROL/readyz" 2>/dev/null; then ready=1; break; fi
  sleep 1
done
[ "$ready" = 1 ] || fail "control plane did not answer /readyz through the forward within 30s"
echo "ok  control plane answered /readyz through the forward"

# ------------------------------------------------------------------- deploy
say "deploying app $APP_ID"
"$CLI" deploy "$ZSHIP" --app="$APP_ID" --control="$CONTROL" \
  || fail "zeroship deploy failed"
echo "ok  deploy accepted"

# ------------------------------------------------------------------ migrate
#
# Deploy does NOT apply migrations, and an app that uses `env.db` is broken
# until something does: the per-app database role is created by the migration
# service's apply path and by nothing else, so the first `env.db` call on an
# unmigrated app fails with `role "app_..._role" does not exist` and the end
# user sees an opaque `internal error`. That is a live-site failure a green
# deploy here used to hide, which is why this step is not optional when the
# artifact exists.
#
# Presence of the file IS the condition: the build writes it only for an app
# with committed migrations, so an app without `env.db` has nothing here and
# skips. Do not turn this into a flag - a step you have to remember is the
# thing that failed.
IR_JSON=""
[ -n "$APP_DIR" ] && IR_JSON="$APP_DIR/generated/zeroship/migrations.ir.json"
if [ -n "$IR_JSON" ] && [ -f "$IR_JSON" ]; then
  say "applying migrations for app $APP_ID"
  "$CLI" migrate "$IR_JSON" --app="$APP_ID" --control="$CONTROL" \
    || fail "zeroship migrate failed - the app is deployed but its schema is not applied, so every env.db call will fail"
  echo "ok  migrations applied"
elif [ -n "$APP_DIR" ]; then
  echo "ok  no $IR_JSON - this app has no committed migrations, nothing to apply"
else
  # --zship without --dir: there is no app directory to find the artifact in.
  # Say so rather than printing nothing, or a db-backed app deployed this way
  # goes out unmigrated and silent.
  echo "note: --zship was used without --dir, so migrations were NOT applied."
  echo "      If this app uses env.db, run:  $CLI migrate <path-to-migrations.ir.json> --app=$APP_ID --control=$CONTROL"
fi

# ------------------------------------------------------------------- verify
if [ -n "$PROBE_URL" ]; then
  say "probing $PROBE_URL"
  if [ -n "$PROBE_SCRIPT" ]; then
    # An explicit behavioural check. This is a FLAG rather than the env var it
    # used to be: an undocumented variable that silently swaps a liveness check
    # for a functional one means two runs printing "ok" can have asserted very
    # different things, and nothing in the output says which.
    [ -f "$PROBE_SCRIPT" ] || fail "probe script not found: $PROBE_SCRIPT"
    node "$PROBE_SCRIPT" "$PROBE_URL" || fail "app probe failed after deploy"
  else
    # Generic liveness. Deliberately NOT called a functional check: a 200 from
    # an SPA shell says the asset route works and nothing about the app. For
    # anything stronger pass --probe-script (see examples/db-todos/scripts/
    # probe-live.mjs, which asserts a subscription carries a real write).
    code="$(curl -sS -o /dev/null -w '%{http_code}' --max-time 20 "$PROBE_URL" || echo 000)"
    [ "$code" = "200" ] || fail "probe returned HTTP $code from $PROBE_URL"
    echo "ok  $PROBE_URL returned 200 (liveness only, not a behavioural check)"
  fi
fi

say "deployed $ZSHIP to app $APP_ID on $HOST"
