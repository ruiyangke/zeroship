#!/usr/bin/env bash
# ---------------------------------------------------------------------------
# env.auth: dev vs deployed -- run ONE identical sequence of auth operations
# against `pnpm dev` and against the same app deployed behind the gateway, then
# diff the RESULTS.
#
# Scenario 6 of docs/pilot/e2e-scenarios.md. The sibling harnesses
# (e2e_dev_vs_deployed_{kv,storage,workflows}.sh) compare a data primitive
# across two backends; this one compares the AUTH CONTRACT SURFACE across two
# identity providers -- status codes, error `code` strings, the JSON error
# envelope, and the exact field set of `env.auth.getUser()`.
#
# WHAT ALREADY EXISTED, AND WHY IT IS NOT THIS.
#   tests/e2e_auth_rpc.sh proves the DEPLOYED half end-to-end (anon -> 401,
#   authed -> 200 with `env.auth.getUser()` == the caller). It does not run the
#   dev side at all, so it cannot see a seam. Nothing in this repo proved dev
#   ANSWERS THE SAME until this script.
#
# THE COMPARISON IS DELIBERATELY VALUE-EXACT, NOT NORMALISED.
#   Dev auth is a different PROVIDER by construction (docs/reference/auth-dev-tier.md),
#   so the naive move is to normalise identity values away and compare shapes.
#   That would have hidden the `avatar` divergence this script found, because a
#   normaliser that rewrites values decides for itself whether an absent key and
#   a null key are "the same". Instead both sides are configured to carry the
#   IDENTICAL identity: examples/auth-probe/vite.config.ts declares two dev users
#   and `mint_session()` below signs a gateway session cookie with the same
#   id/email/name/avatar/email_verified/scopes. Whatever still differs is a
#   contract difference, not an identity difference.
#
#   The identity MECHANISM is not compared and cannot be: the dev cookie is an
#   HMAC over a user JSON, the deployed cookie is an Ed25519 JWT. Only what the
#   two tiers ANSWER is compared.
#
# CREDENTIALS (6: anonymous, two live sessions, a bad-signature cookie, a
# non-token string, an expired one) x PROCEDURES (7, one per auth posture) = 42
# RPC rows, plus 6 browser-endpoint rows on `/__zeroship/auth/{session,signout}`.
# 48 total. See `probe()`.
#
# EXPECTED RESULT TODAY: RED, 27 of 48 rows divergent. That is the finding, not a
# broken test -- see docs/pilot/e2e-scenarios.md "Scenario 6, auth" for the
# row-by-row verdicts (which divergences are deliberate and which are defects,
# two of the defects being on the DEPLOYED side). Before weakening any assertion
# here, read that section. Two guards keep the red honest: a dev-vs-dev self-diff
# that must be EMPTY (a comparison that is red no matter what proves as little as
# one that is green no matter what), and MUTATE=declare-defaulted, which changes
# one posture and must move exactly the rows named for it (27 -> 24).
#
# Prereqs (docs/runbooks/local-dev.md):
#   pnpm build
#   cargo build --release -p zeroship-control -p zeroship-worker -p zeroship-gateway -p zeroship --bins
#   cargo build --release -p zeroship-migrate-adapter --features platform-cli --bin zeroship-platform-migrate
#   docker (this script starts its OWN ephemeral Postgres -- nothing to pre-start)
#   pnpm install --filter ./examples/auth-probe...
#
#   ./tests/e2e_dev_vs_deployed_auth.sh
# ---------------------------------------------------------------------------
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN="$ROOT/target/release"
APP="$ROOT/examples/auth-probe"
ZSHIP="$APP/dist/app.zship"

# A port band of its own: kv 9392/8392/8302/3011, storage 9396/8396/8306/3081,
# streaming 9394/8394/8304/3061, golden_path 9390/8390/8300.
export CONTROL_PORT="${CONTROL_PORT:-9398}"
export WORKER_PORT="${WORKER_PORT:-8398}"
export GATE_PORT="${GATE_PORT:-8308}"
export PG_PORT="${PG_PORT:-5458}"
export PG_CONTAINER="${PG_CONTAINER:-zs-devdeploy-auth-pg}"
DEV_PORT="${DEV_PORT:-3091}"   # examples/auth-probe AUTH_PROBE_API_PORT default
APP_SLUG="auth-probe-dd"
HOST="$APP_SLUG.localhost"
# OAC (the app's OAuth client id) is NOT set here on purpose -- it is READ from
# what the control plane brokered at app-create time. See the provisioning block.
MUTATE="${MUTATE:-none}"

# --- the identity BOTH sides carry ------------------------------------------
# Mirrors examples/auth-probe/vite.config.ts exactly. `assert_identity_pair`
# below re-derives these from that file on every run, so the two cannot drift
# apart silently -- a drifted pair would show up as a "divergence" in every
# authenticated row and mean nothing.
#
# `sub` must satisfy crates/core is_pairwise_subject: "pws_" + exactly 20
# alphanumerics. ALPHA carries a non-null avatar, BETA a null one -- the
# one-variable control pair for the gateway's
# `#[serde(skip_serializing_if = "Option::is_none")]` on WorkerUser.avatar.
ALPHA_ID="pws_probealpha0000000000"
ALPHA_EMAIL="alpha@probe.zeroship.test"
ALPHA_NAME="Probe Alpha"
ALPHA_AVATAR="https://probe.zeroship.test/a.png"
BETA_ID="pws_probebeta00000000000"
BETA_EMAIL="beta@probe.zeroship.test"
BETA_NAME="Probe Beta"
DEV_PASSWORD="probe-pw"
SCOPES_JSON='["openid","profile","email"]'

PASS=0; FAIL=0; PIDS=()
pass() { PASS=$((PASS+1)); echo "  ok   $1"; }
fail() { FAIL=$((FAIL+1)); echo "  FAIL $1"; }

source "$ROOT/tests/lib/e2e_stack.sh"
cleanup() {
  for p in "${PIDS[@]:-}"; do kill "$p" 2>/dev/null || true; done
  lsof -ti :"$DEV_PORT" 2>/dev/null | xargs -r kill 2>/dev/null || true
  [ -f "${MUTATE_BAK:-}" ] && cp "$MUTATE_BAK" "$APP/src/server/config.ts"
  if [ "${KEEP_WORK:-0}" = "1" ]; then
    echo "  work dirs kept: dev=${DEV_WORK:-${WORK_EARLY:-<none>}} deployed=${WORK:-<none>}"
    if [ -n "${PIDFILE:-}" ] && [ -f "$PIDFILE" ]; then
      while read -r p; do [ -n "$p" ] && kill "$p" 2>/dev/null || true; done < "$PIDFILE"
    fi
    docker rm -f "$PG_CONTAINER" >/dev/null 2>&1 || true
  else
    stack_down 2>/dev/null || true
    rm -rf "${WORK_EARLY:-}"
  fi
}
trap cleanup EXIT

# --- 0. the two sides must be the same BUILD --------------------------------
# Shared with the other dev-vs-deployed harnesses; see tests/lib/binary_freshness.sh
# for the false finding that produced it. `zeroship` (the CLI) is in the list
# because `pnpm dev` spawns `zeroship serve` as the dev runtime, and
# crates/runtime/src/core/dev_auth.rs -- the dev half of THIS comparison -- ships
# inside it.
# shellcheck source=lib/binary_freshness.sh
source "$ROOT/tests/lib/binary_freshness.sh"
zs_check_binary_freshness "$ROOT" "$BIN" \
  "crates/runtime/src crates/worker/src crates/gateway/src crates/control/src crates/core/src sdks/auth/src sdks/bootstrap/src" \
  "zeroship zeroship-worker zeroship-gate zeroship-control" \
  || { [ "$?" -eq 2 ] && exit 2; }

echo "=== env.auth: dev vs deployed (auth-probe) ==="
echo "  mutation: $MUTATE"

# ---------------------------------------------------------------------------
# The probe. ONE function, both sides. `$1` is the base URL; `$2` names the
# side, which selects the cookie NAME and the credential minting -- the only
# two things that legitimately differ.
#
# Every line is `<credential> <op> <status> <body>`. Volatile fields are blanked;
# ANYTHING NOT BLANKED HERE IS BEING ASSERTED IDENTICAL across the two tiers.
# ---------------------------------------------------------------------------
PROCS="probe.public probe.defaulted probe.userDeclared probe.requireAnon probe.requireGated probe.appGate probe.userShape"

probe() {
  local base="$1" side="$2"
  local rpc="$base/__zeroship/v1"
  local cookie_name proc cred hdr

  if [ "$side" = "dev" ]; then cookie_name="__zeroship_dev_session"; else cookie_name="zeroship_app_session"; fi

  # Blank only what a clock or a random id makes volatile. request ids leak into
  # some error envelopes; timestamps into the session projection.
  #
  # `stack` collapses to a PLACEHOLDER, not to nothing. Whether the error
  # envelope carries a `stack` key AT ALL is a contract, and `<STACK>` keeps
  # exactly that in the diff -- present on one side only still diverges.
  #
  # AS OF THE STACK STRIP in crates/runtime/src/core/dispatch.rs, no tier emits
  # a `stack` in an RPC error body at all (unless AUTH_INSECURE_DEV is set), so
  # this substitution is now a TRIPWIRE rather than a normaliser.
  #
  # DO NOT READ IT AS EVIDENCE ABOUT LEAKS EITHER WAY. Because the scrub runs on
  # BOTH sides, this diff goes GREEN when both tiers emit a stack just as
  # readily as when neither does -- which is exactly how a real 4xx stack leak
  # to anonymous callers survived here unnoticed until it was measured
  # absolutely. `tests/e2e_dev_vs_deployed_errors.sh` is the harness that
  # asserts the absolute property against the raw deployed bytes; this one
  # cannot, by construction.
  scrub() {
    sed -E \
      -e 's/"(expires_at|exp|iat|auth_time)":[0-9]+/"\1":<V>/g' \
      -e 's/"stack":"[^"]*"/"stack":"<STACK>"/g' \
      -e 's/[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}/<UUID>/g'
  }

  call() {  # call <proc> <cookie-value-or-empty>
    local args=(-s -o "$WORK/probe.body" -w '%{http_code}' -m 20 -X POST
                -H 'content-type: application/json' -H "Host: $HOST")
    [ -n "$2" ] && args+=(-H "Cookie: $cookie_name=$2" -H "Origin: http://$HOST")
    local code; code="$(curl "${args[@]}" "$rpc/$1" -d '{"json":{}}' 2>&1)"
    printf '%s %s\n' "$code" "$(tr -d '\n' < "$WORK/probe.body" | scrub)"
  }

  for cred in anon alpha beta forged garbage stale; do
    eval "hdr=\"\${CRED_${side}_${cred}}\""
    for proc in $PROCS; do
      printf '%-8s %-20s %s\n' "$cred" "$proc" "$(call "$proc" "$hdr")"
    done
  done

  # --- the browser-facing session endpoints (the OTHER half of the contract) --
  # GET /session is the SDK's identity read; POST /signout its logout. Both are
  # served by the dev provider in dev and by the gateway deployed, so they are
  # the same contract with two implementations.
  browser() {  # browser <method> <path> <cookie> <extra-header...>
    local method="$1" path="$2" ck="$3"; shift 3
    local args=(-s -o "$WORK/probe.body" -w '%{http_code}' -m 20 -X "$method" -H "Host: $HOST")
    [ -n "$ck" ] && args+=(-H "Cookie: $cookie_name=$ck")
    local h; for h in "$@"; do args+=(-H "$h"); done
    local code; code="$(curl "${args[@]}" "$base$path" 2>&1)"
    printf '%s %s\n' "$code" "$(tr -d '\n' < "$WORK/probe.body" | head -c 400 | scrub)"
  }
  eval "hdr=\"\${CRED_${side}_alpha}\""
  printf '%-8s %-20s %s\n' "anon"  "GET  /auth/session"  "$(browser GET /__zeroship/auth/session "")"
  printf '%-8s %-20s %s\n' "alpha" "GET  /auth/session"  "$(browser GET /__zeroship/auth/session "$hdr")"
  eval "local fg=\"\${CRED_${side}_forged}\""
  printf '%-8s %-20s %s\n' "forged" "GET  /auth/session" "$(browser GET /__zeroship/auth/session "$fg")"
  printf '%-8s %-20s %s\n' "alpha" "POST /auth/signout"  "$(browser POST /__zeroship/auth/signout "$hdr" "Origin: http://$HOST" "X-ZS-Auth: 1" "content-type: application/json")"
  printf '%-8s %-20s %s\n' "anon"  "POST /auth/signout"  "$(browser POST /__zeroship/auth/signout "" "Origin: http://$HOST" "X-ZS-Auth: 1" "content-type: application/json")"
  # Replay the SAME credential after signout. Both tiers hand the browser a
  # self-contained signed cookie, so this asks whether signout revokes anything
  # SERVER-SIDE or only clears the client's jar.
  #
  # READ THIS BEFORE TRUSTING THE DEPLOYED HALF OF THIS ROW: the gateway revokes
  # via the `__Host-zeroship_app_anchor` row, and an offline-minted session has
  # no anchor, so deployed signout here is the no-anchor arm (`signout_cleared`)
  # by construction. Deployed GLOBAL revocation is NOT covered by this harness.
  printf '%-8s %-20s %s\n' "replay" "GET  /auth/session"  "$(browser GET /__zeroship/auth/session "$hdr")"
}

# ---------------------------------------------------------------------------
# 1. Build the app, and assert the FIXTURE INVARIANTS the comparison rests on.
# ---------------------------------------------------------------------------
WORK_EARLY="$(mktemp -d -t zs-authprobe-XXXXXX)"
WORK="$WORK_EARLY"   # stack_up replaces this; the build needs a scratch dir now

if [ "$MUTATE" = "declare-defaulted" ]; then
  # ROW-LEVEL SENSITIVITY CONTROL: give `probe.defaulted` an explicit
  # `auth:"anon"` posture, changing ONE variable, and check that exactly the
  # rows named `probe.defaulted` move and nothing else does. A diff that reports
  # 27 divergences either way would prove nothing about which row measures what.
  #
  # The edit lands in the shared source, so BOTH sides are rebuilt from it -- and
  # only the DEPLOYED side changes behaviour. That asymmetry is #163 restated as
  # an experiment: the posture declaration is load-bearing deployed and inert in
  # dev. Expect the anon/forged/garbage `probe.defaulted` rows to become
  # identical (27 -> 24). `stale probe.defaulted` stays divergent for the OTHER
  # reason (dev sessions never expire), which is the point of running it.
  MUTATE_BAK="$WORK/config.ts.bak"
  cp "$APP/src/server/config.ts" "$MUTATE_BAK"
  sed -i 's|"rpc:probe.public": { auth: "anon", publiclyAccessible: true },|"rpc:probe.public": { auth: "anon", publiclyAccessible: true },\n    "rpc:probe.defaulted": { auth: "anon", publiclyAccessible: true },|' \
    "$APP/src/server/config.ts"
  grep -q 'rpc:probe.defaulted' "$APP/src/server/config.ts" \
    && echo "  MUTATED: probe.defaulted declared auth:anon (both builds; only deployed changes)" \
    || { fail "mutation did not apply"; exit 1; }
fi

( cd "$APP" && pnpm build ) > "$WORK/build.log" 2>&1
[ -f "$ZSHIP" ] && pass "built app.zship ($(du -k "$ZSHIP" | cut -f1)KB)" \
  || { fail "build produced no app.zship"; tail -30 "$WORK/build.log"; exit 1; }

# The #163 probe only measures anything while `probe.defaulted` has NO declared
# posture. If someone "tidies" src/server/config.ts by adding it, the row goes
# green for the wrong reason and the harness silently stops testing the thing it
# was written for. Assert the absence in the BUILT manifest, not in the source.
d="$WORK/unpack"; mkdir -p "$d"; tar -xf "$ZSHIP" -C "$d"
declared_default=$(grep -oE '"rpc:probe\.defaulted":\{[^}]*"auth":' "$d/manifest.json" | wc -l)
if [ "$MUTATE" = "none" ]; then
  [ "$declared_default" -eq 0 ] \
    && pass "probe.defaulted carries NO auth posture (the #163 probe is live)" \
    || fail "probe.defaulted now declares an auth posture -- the #163 probe is dead, see src/server/config.ts"
fi
for p in probe.public probe.requireAnon probe.appGate probe.userShape; do
  grep -qE "\"rpc:$p\":\{[^}]*\"auth\":\"anon\"" "$d/manifest.json" \
    || fail "$p is not anon in the manifest -- the deployed side will never reach the worker for it"
done
pass "manifest postures match src/server/config.ts"

# The dev users the vite config declares MUST equal the claims minted for the
# deployed cookie, or every authenticated row diverges on identity rather than
# on contract. Re-derive from the config file so the pair cannot drift.
assert_identity_pair() {
  local cfg="$APP/vite.config.ts" v ok=1
  for v in "$ALPHA_ID" "$ALPHA_EMAIL" "$ALPHA_NAME" "$ALPHA_AVATAR" \
           "$BETA_ID" "$BETA_EMAIL" "$BETA_NAME" "$DEV_PASSWORD"; do
    grep -qF -- "$v" "$cfg" || { fail "identity drift: '$v' is not in $cfg"; ok=0; }
  done
  [ "$ok" = "1" ] && pass "dev users and minted deployed claims carry the same identity"
}
assert_identity_pair

# ---------------------------------------------------------------------------
# 2. Dev side. `pnpm dev` spawns `zeroship serve`; the dev-auth provider lives in
#    that child (sdks/bootstrap/src/dev-auth.ts, reached via dev-entry.ts), so
#    DEV_PORT is the AUTH_PROBE_API_PORT the vite plugin gave the child.
# ---------------------------------------------------------------------------
lsof -ti :"$DEV_PORT" 2>/dev/null | xargs -r kill -9 2>/dev/null || true
( cd "$APP" && ./node_modules/.bin/vite > "$WORK/dev.log" 2>&1 ) & PIDS+=($!)
for _ in $(seq 1 25); do
  curl -sf -o /dev/null -m 2 -X POST -H 'content-type: application/json' \
    "http://localhost:$DEV_PORT/__zeroship/v1/probe.public" -d '{"json":{}}' && break
  sleep 2
done
curl -sf -o /dev/null -m 5 -X POST -H 'content-type: application/json' \
  "http://localhost:$DEV_PORT/__zeroship/v1/probe.public" -d '{"json":{}}' \
  && pass "dev app reachable on :$DEV_PORT" \
  || { fail "dev app never came up"; tail -30 "$WORK/dev.log"; exit 1; }

# Drive the REAL dev login flow -- GET /authorize (form + CSRF cookie) ->
# POST /authorize (credential check) -> POST /session (code exchange). No
# short-cut that mints the cookie directly: the credential check and the code
# exchange are the dev tier's own contract and a shortcut would skip them.
#
# `dev_submit <email> <password> <jar>` runs the GET+POST pair with a VALID
# CSRF token and echoes "<status> <redirect_url>". Both the happy path and the
# wrong-password control go through it, so the two differ in exactly ONE
# variable -- the password. An earlier version posted the wrong password with no
# CSRF cookie at all and "passed" on a 400 from the CSRF guard, which proves the
# endpoint rejects SOMETHING and says nothing about the credential check.
dev_submit() {  # dev_submit <email> <password> <jar> -> "<status> <redirect_url>"
  # One `local` per line ON PURPOSE: bash expands every word of a `local`
  # statement BEFORE the builtin assigns any of them, so `local a="$1" b="$a"`
  # reads an unset `a` (and aborts outright under `set -u`).
  local email="$1"
  local password="$2"
  local jar="$3"
  local base="http://localhost:$DEV_PORT"
  local redirect="$base/__zeroship/auth/popup-callback"
  rm -f "$jar"
  local form
  form="$(curl -s -c "$jar" -m 15 \
    "$base/__zeroship/auth/authorize?state=probe-state&redirect_uri=$redirect")"
  local csrf
  csrf="$(printf '%s' "$form" | grep -oE 'name="csrf" value="[^"]*"' | sed -E 's/.*value="([^"]*)".*/\1/')"
  [ -n "$csrf" ] || { echo "000 NO-CSRF-IN-FORM"; return 1; }
  curl -s -b "$jar" -c "$jar" -m 15 -o /dev/null -w '%{http_code} %{redirect_url}' \
    -X POST "$base/__zeroship/auth/authorize" \
    --data-urlencode "csrf=$csrf" --data-urlencode "state=probe-state" \
    --data-urlencode "redirect_uri=$redirect" \
    --data-urlencode "email=$email" --data-urlencode "password=$password"
}
dev_login() {  # dev_login <email> -> echoes the __zeroship_dev_session value
  local email="$1"
  local jar="$WORK/jar-$email.txt"
  local base="http://localhost:$DEV_PORT"
  local res
  res="$(dev_submit "$email" "$DEV_PASSWORD" "$jar")"
  local code
  code="$(printf '%s' "$res" | sed -nE 's/.*[?&]code=([^&]*).*/\1/p')"
  [ -n "$code" ] || { echo "DEVLOGIN-NO-CODE($res)"; return 1; }
  curl -s -b "$jar" -c "$jar" -m 15 -o /dev/null -X POST \
    -H 'content-type: application/json' "$base/__zeroship/auth/session" \
    -d "{\"code\":\"$code\"}"
  awk '$6 == "__zeroship_dev_session" { print $7 }' "$jar"
}
CRED_dev_alpha="$(dev_login "$ALPHA_EMAIL")"
CRED_dev_beta="$(dev_login "$BETA_EMAIL")"
[ "${CRED_dev_alpha%%.*}" != "$CRED_dev_alpha" ] \
  && pass "dev login (real form + CSRF + credential check + code exchange) minted a session" \
  || { fail "dev login failed: $CRED_dev_alpha"; tail -30 "$WORK/dev.log"; exit 1; }

# The one-variable control: same form, same CSRF, WRONG password. A green login
# proves the happy path only; this is what separates "the dev tier validates
# credentials" from "the dev tier auto-logs-in whoever asks".
bad_login="$(dev_submit "$ALPHA_EMAIL" "definitely-not-the-password" "$WORK/jar-bad.txt")"
case "$bad_login" in
  401*) pass "dev rejects a WRONG password with 401 and no code (same form + valid CSRF)" ;;
  *)    fail "dev answered '$bad_login' to a wrong password (want 401, no redirect) -- the credential check is not discriminating" ;;
esac

CRED_dev_anon=""
CRED_dev_garbage="not-a-token"
# Same payload, the signature's last character changed -- a credential that is
# structurally a dev session and cryptographically not one. `flip_last` picks a
# replacement that differs from what is there, so the "forged" credential can
# never accidentally equal the real one.
flip_last() { printf '%s' "${1%?}"; case "${1: -1}" in a) printf b;; *) printf a;; esac; }
CRED_dev_forged="$(flip_last "$CRED_dev_alpha")"
[ "$CRED_dev_forged" != "$CRED_dev_alpha" ] \
  && pass "forged dev cookie differs from the real one" \
  || fail "forged dev cookie is identical to the real one"
# `stale`: a credential the issuing tier considered valid a day ago. The dev
# token has no time claim at all (payload = the user JSON), so the only way to
# express "stale" in dev is to replay the same cookie -- which is itself the
# finding the deployed row makes visible.
CRED_dev_stale="$CRED_dev_alpha"

probe "http://localhost:$DEV_PORT" dev > "$WORK/dev.txt" 2>&1
grep -q 'probe.public' "$WORK/dev.txt" && pass "dev answered the probe ($(wc -l < "$WORK/dev.txt") rows)" \
  || { fail "dev probe produced nothing"; tail -20 "$WORK/dev.log"; exit 1; }

# --- can this comparison PASS? ---------------------------------------------
# The final diff below is expected to be RED today, and a comparison that is red
# no matter what proves exactly as little as one that is green no matter what.
# So run the identical probe against the identical server a second time: the two
# outputs MUST be byte-identical. That establishes two things the divergence
# report depends on -- the probe is deterministic (no clock, counter or ordering
# noise leaking into the rows), and `diff` reports nothing when the two inputs
# agree. Every row still differing afterwards is content, not instrument.
probe "http://localhost:$DEV_PORT" dev > "$WORK/dev-again.txt" 2>&1
if diff -q "$WORK/dev.txt" "$WORK/dev-again.txt" >/dev/null 2>&1; then
  pass "probe is deterministic: dev-vs-dev self-diff is empty (the comparison CAN go green)"
else
  fail "dev disagrees with ITSELF across two runs -- the rows below are instrument noise, not findings"
  diff "$WORK/dev.txt" "$WORK/dev-again.txt" | head -20
fi

# ---------------------------------------------------------------------------
# 3. Deployed side: real stack, real deploy, real gateway session validation.
# ---------------------------------------------------------------------------
DEV_WORK="$WORK"
stack_up || { fail "stack bring-up failed"; exit 1; }   # stack_up resets $WORK
cp "$DEV_WORK/dev.txt" "$WORK/dev.txt"
mint_admin_pat || exit 1

APP_ID="$(deploy_zship "$APP_SLUG" "$ZSHIP")" || { fail "deploy auth-probe"; exit 1; }
pass "deployed auth-probe ($APP_ID)"

# The per-app OAuth client. Since cd54028e7 the CONTROL PLANE brokers one at
# app-create time, so the app already has a `oac_...` and the session cookie's
# `app` claim must match THAT id -- a harness that inserts its own client_id
# instead gets `app mismatch: token "oac_mine" != expected "oac_<brokered>"` on
# every authed call and never recovers. So: read what control provisioned, and
# only fall back to inserting one if it provisioned nothing.
OAC=""
for _ in $(seq 1 20); do
  OAC="$(docker exec -i "$PG_CONTAINER" psql -U postgres -d zeroship -tAc \
    "SELECT client_id FROM zeroship.app_oauth_clients WHERE app_id = '$APP_ID';" 2>/dev/null | tr -d '[:space:]')"
  [ -n "$OAC" ] && break
  sleep 1
done
if [ -n "$OAC" ]; then
  pass "control brokered the app's OAuth client ($OAC)"
else
  OAC="oac_devdeploy_auth"
  docker exec -i "$PG_CONTAINER" psql -U postgres -d zeroship -v ON_ERROR_STOP=1 >/dev/null 2>&1 <<SQL
INSERT INTO zeroship.oauth_clients (client_id, client_name, redirect_uris, scopes)
VALUES ('$OAC', 'dev-vs-deployed auth', ARRAY['https://$HOST/__zeroship/auth/callback'],
        ARRAY['openid','email','profile'])
ON CONFLICT (client_id) DO NOTHING;
INSERT INTO zeroship.app_oauth_clients (app_id, client_id, sector_identifier)
VALUES ('$APP_ID', '$OAC', 'https://$HOST');
SQL
  pass "control brokered nothing; harness provisioned app_oauth_clients ($OAC)"
fi
# The gateway fails CLOSED with 503 client_not_provisioned when the route has no
# sector_identifier (it cannot derive the pairwise subject), which would read as
# an auth divergence and is a missing fixture. Fill it if control left it null.
docker exec -i "$PG_CONTAINER" psql -U postgres -d zeroship >/dev/null 2>&1 <<SQL
UPDATE zeroship.app_oauth_clients SET sector_identifier = 'https://$HOST'
WHERE app_id = '$APP_ID' AND (sector_identifier IS NULL OR sector_identifier = '');
SQL

# Offline-mint an app session cookie by signing with the harness-controlled
# gateway Ed25519 key -- the same technique tests/e2e_auth_rpc.sh uses, and the
# same one that mints the admin PAT. The gateway's REAL cookie validation runs
# (signature, kid, iss, exp, app binding, pws_ sanity, revocation gate); nothing
# is bypassed and no --dev-insecure auth escape hatch is added to the binaries.
# What is skipped is only the interactive OIDC dance that would otherwise need a
# live OP standing beside the stack.
mint_session() {  # mint_session <sub> <email> <name> <avatar-json> <exp-offset-secs>
  node --input-type=module -e '
import { readFileSync } from "node:fs";
import { createHash } from "node:crypto";
import { importPKCS8, exportJWK, SignJWT } from "file://'"$E2E_JOSE_JS"'";
const [pem, app, sub, email, name, avatarJson, scopesJson, off] = process.argv.slice(1);
const key = await importPKCS8(readFileSync(pem,"utf8"), "EdDSA", { extractable:true });
const x = (await exportJWK(key)).x;
const kid = createHash("sha256").update(`{"crv":"Ed25519","kty":"OKP","x":"${x}"}`).digest("base64url");
const now = Math.floor(Date.now()/1000) + Number(off);
const jwt = await new SignJWT({
  app, sub, email, email_verified: true, name,
  avatar: JSON.parse(avatarJson), scopes: JSON.parse(scopesJson),
  auth_time: now, amr: ["pwd"],
})
  .setProtectedHeader({ alg:"EdDSA", typ:"zeroship-sess+jwt", kid })
  .setIssuer("https://api.zeroship.ai")
  .setIssuedAt(now).setExpirationTime(now + 3600)
  .sign(key);
process.stdout.write(jwt);
' "$WORK/signing-key.pem" "$OAC" "$1" "$2" "$3" "$4" "$SCOPES_JSON" "$5"
}
CRED_deployed_alpha="$(mint_session "$ALPHA_ID" "$ALPHA_EMAIL" "$ALPHA_NAME" "\"$ALPHA_AVATAR\"" 0)"
CRED_deployed_beta="$(mint_session "$BETA_ID" "$BETA_EMAIL" "$BETA_NAME" "null" 0)"
# `stale`: issued and expired a day ago. Same key, same claims shape.
CRED_deployed_stale="$(mint_session "$ALPHA_ID" "$ALPHA_EMAIL" "$ALPHA_NAME" "\"$ALPHA_AVATAR\"" -90000)"
CRED_deployed_anon=""
CRED_deployed_garbage="not-a-token"
CRED_deployed_forged="$(flip_last "$CRED_deployed_alpha")"
[ "$(echo -n "$CRED_deployed_alpha" | awk -F. '{print NF}')" = "3" ] \
  && pass "minted deployed session cookies (alpha, beta, stale)" \
  || { fail "session mint failed: $CRED_deployed_alpha"; exit 1; }

# The gateway re-pulls routes from control on an interval; wait for the route to
# carry the oauth_client_id (before that an authed call is 503/redirect, which
# would read as a divergence and is a race).
ready=0
for _ in $(seq 1 25); do
  c="$(curl -s -o /dev/null -w '%{http_code}' -m 10 -X POST -H 'content-type: application/json' \
      -H "Host: $HOST" -H "Origin: http://$HOST" -H "Cookie: zeroship_app_session=$CRED_deployed_alpha" \
      "http://localhost:$GATE_PORT/__zeroship/v1/probe.userDeclared" -d '{"json":{}}')"
  [ "$c" = "200" ] && { ready=1; break; }
  sleep 2
done
[ "$ready" = "1" ] && pass "gateway accepts the minted session (authed gated call -> 200)" \
  || { fail "gateway never accepted the session (last code=$c)"
       grep -iE 'session|cookie|kid|verif|sector|unauth|client_not' "$WORK/gate.log" 2>/dev/null | tail -15 | sed 's/^/    /'; }

probe "http://localhost:$GATE_PORT" deployed > "$WORK/deployed.txt" 2>&1
grep -q 'probe.public' "$WORK/deployed.txt" && pass "deployed app answered the probe ($(wc -l < "$WORK/deployed.txt") rows)" \
  || { fail "deployed probe produced nothing"; tail -20 "$WORK/worker.log"; }

# ---------------------------------------------------------------------------
# 4. THE POINT: identical operations must produce identical results.
# ---------------------------------------------------------------------------
echo ""
if diff -q "$WORK/dev.txt" "$WORK/deployed.txt" >/dev/null 2>&1; then
  pass "dev and deployed agree on every probed auth operation"
else
  n=$(diff "$WORK/dev.txt" "$WORK/deployed.txt" | grep -c '^<')
  fail "dev and deployed DIVERGE on $n of $(wc -l < "$WORK/dev.txt") rows (< dev, > deployed)"
  diff "$WORK/dev.txt" "$WORK/deployed.txt"
  echo ""
  echo "  A divergence here is the finding, not a flaky test. Both tiers are"
  echo "  individually coherent; disagreeing on the CONTRACT SURFACE (status,"
  echo "  error code, envelope shape, user field set) is the defect. The identity"
  echo "  VALUES are configured identical on both sides on purpose, so a value"
  echo "  difference is drift in the fixture, not a platform finding."
  echo "  Read docs/pilot/e2e-scenarios.md scenario 6 before weakening anything."
fi

echo ""
echo "  auth dev vs deployed: $PASS passed, $FAIL failed"
[ "$FAIL" -eq 0 ]
