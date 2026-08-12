#!/usr/bin/env bash
# ============================================================================
# e2e_account_status_enforcement.sh — NEW-COVERAGE E2E for the ACCOUNT-status
# gateway gate (billing G2): Active/PastDue → served, Suspended → 402.
#
# This is the OTHER gateway enforcement gate (complements the spend-state one).
# `check_account` (crates/gateway/src/enforce.rs) runs BEFORE `check_spend`, so a
# Suspended creator's apps 402 `ACCOUNT_SUSPENDED` regardless of spend headroom.
# State is CREATOR-keyed (`zeroship.creator_billing_status.state` ∈
# active|past_due|suspended), surfaced per-app on the pulled `RouteEntry` via the
# app's `app_members(role='owner')` (registry.rs). Default (no row) = Active.
#
#   Active     → 200
#   PastDue    → 200   (dunning GRACE window — still served)
#   Suspended  → 402 { "code": "ACCOUNT_SUSPENDED" }
#   recover    → Active → 200
#
# Enforcement is provider-independent; uses the `lite` provider. No usage/pricing
# is needed — the account gate is orthogonal to spend.
#
# REFUSES (exit 1) when docker is unavailable - a run that asserted nothing
# is not a passing run. KEEP_WORK=1 preserves logs.
# ============================================================================
set -uo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"; BIN="$ROOT/target/release"
# shellcheck source=tests/lib/runtime_secrets.sh
source "$ROOT/tests/lib/runtime_secrets.sh"
PASS=0; FAIL=0
pass(){ PASS=$((PASS+1)); echo "  ✓ $1"; }
fail(){ FAIL=$((FAIL+1)); echo "  ✗ $1"; }

echo "============================================"
echo "  zeroship E2E — account-status gate (Active/PastDue/Suspended)"
echo "============================================"
# Docker unavailable is a REFUSAL, not a skip. This used to `exit 0` after a
# warning, so on any machine without docker the harness reported success having
# driven none of the account states - "0 failed over 0 assertions", the shape of
# tasks #102/#103/#279 and the one edafc8644 removed from the spend harness.
# RED-PROVEN before changing it: with a stub `docker` returning 1 on PATH, the
# old arm printed "SKIP: docker unavailable." and exited 0.
if ! command -v docker >/dev/null 2>&1 || ! docker info >/dev/null 2>&1; then
  echo "  x REFUSED: docker unavailable, so NONE of the Active/PastDue/Suspended" >&2
  echo "    states were driven. Exiting non-zero: a run that asserted nothing is" >&2
  echo "    not a passing run. Start docker and re-run." >&2
  exit 1
fi
for b in zeroship zeroship-control zeroship-gate zeroship-worker zeroship-platform-migrate; do [ -x "$BIN/$b" ] || { echo "missing $BIN/$b"; exit 2; }; done
command -v node >/dev/null && command -v openssl >/dev/null && command -v curl >/dev/null || { echo "need node/openssl/curl"; exit 2; }
PROBE="$ROOT/examples/metering-probe/dist/app.zship"; [ -f "$PROBE" ] || { echo "missing $PROBE"; exit 2; }
JOSE="$ROOT/node_modules/.pnpm/jose@6.2.3/node_modules/jose/dist/webapi/index.js"; [ -f "$JOSE" ] || { echo "missing jose"; exit 2; }

CONTROL_PORT=9177; WORKER_PORT=8077; GATE_PORT=8067; PG_PORT=5477; RP_PORT=19177
PGC=zs-e2e-acct-pg; RPC=zs-e2e-acct-redpanda
DBURL="postgres://postgres:zeroship@localhost:$PG_PORT/zeroship"; CONTROL_URL="http://localhost:$CONTROL_PORT"
RP_BROKERS="127.0.0.1:$RP_PORT"; USAGE_TOPIC="zeroship-usage-acct-e2e"
WORK="$(mktemp -d -t zs-e2e-acct-XXXXXX)"; mkdir -p "$WORK/blobs" "$WORK/blob-cache"; PIDFILE="$WORK/pids"; : > "$PIDFILE"
jget(){ node -e "let s='';process.stdin.on('data',d=>s+=d).on('end',()=>{try{const o=JSON.parse(s);process.stdout.write(String(o$1??'')+'\n')}catch(e){console.log('')}})"; }
psql_exec(){ docker exec -i "$PGC" psql -U postgres -d zeroship -v ON_ERROR_STOP=1 "$@"; }
set_acct(){ psql_exec >/dev/null 2>&1 <<SQL
INSERT INTO zeroship.creator_billing_status (creator_id,state) VALUES ('$1','$2')
  ON CONFLICT (creator_id) DO UPDATE SET state=EXCLUDED.state, updated_at=NOW();
SQL
}
# GET one probe request; echo "<status> <bodycode>"
probe_req(){ local out code body; out="$(curl -s -w $'\n%{http_code}' -H 'Host: acct-probe.localhost' "http://localhost:$GATE_PORT/probe/$1" 2>/dev/null)"; code="$(printf '%s' "$out" | tail -1)"; body="$(printf '%s' "$out" | sed '$d')"; local bc; bc="$(printf '%s' "$body" | jget '.code')"; echo "${code:-000} ${bc:-none}"; }

cleanup(){
  echo ""; echo "=== Cleanup ==="
  [ -f "$PIDFILE" ] && while read -r pid; do [ -n "$pid" ] && kill "$pid" 2>/dev/null || true; done < "$PIDFILE"
  wait 2>/dev/null || true
  if [ "${KEEP_WORK:-0}" = "1" ]; then echo "  KEEP_WORK=1 → $PGC/$RPC + $WORK preserved";
  else docker rm -f "$PGC" "$RPC" >/dev/null 2>&1 || true; rm -rf "$WORK"; echo "  stack down, $WORK cleaned"; fi
}
trap cleanup EXIT
for p in $CONTROL_PORT $WORKER_PORT $GATE_PORT; do lsof -ti :"$p" 2>/dev/null | xargs -r kill -9 2>/dev/null || true; done

echo ""; echo "=== Stage 1: infra + migrate + seed + stack (lite provider) ==="
docker rm -f "$PGC" >/dev/null 2>&1 || true
docker run --name "$PGC" -d -p "$PG_PORT:5432" -e POSTGRES_PASSWORD=zeroship -e POSTGRES_USER=postgres -e POSTGRES_DB=zeroship postgres:16 -c max_connections=200 >/dev/null || { fail "pg run"; exit 1; }
# Readiness = three CONSECUTIVE successful queries, not one pg_isready. Task #274:
# the postgres entrypoint runs an initdb-phase server and RESTARTS it, so there is
# a window where pg_isready answers yes and the very next psql fails. The fix lives
# in tests/lib/e2e_stack.sh stack_pg_up; this harness never sourced the library, so
# it kept the retired probe. Consecutive-ness is the point - a single successful
# select can land inside the same window.
PG_OK=0
for _ in $(seq 1 60); do
  if docker exec "$PGC" psql -U postgres -d zeroship -tAc 'select 1' >/dev/null 2>&1; then
    PG_OK=$((PG_OK + 1)); [ "$PG_OK" -ge 3 ] && break
  else
    PG_OK=0
  fi
  sleep 1
done
[ "$PG_OK" -ge 3 ] && pass "ephemeral PG on :$PG_PORT (3 consecutive selects)" \
  || { fail "PG never answered 3 consecutive selects in 60s"; exit 1; }

docker rm -f "$RPC" >/dev/null 2>&1 || true
docker run --name "$RPC" -d -p "$RP_PORT:$RP_PORT" docker.redpanda.com/redpandadata/redpanda:latest \
  redpanda start --overprovisioned --smp 1 --memory 512M --reserve-memory 0M --node-id 0 --check=false \
  --kafka-addr "external://0.0.0.0:$RP_PORT" --advertise-kafka-addr "external://127.0.0.1:$RP_PORT" \
  --set redpanda.auto_create_topics_enabled=true >/dev/null || { fail "redpanda run"; exit 1; }
for _ in $(seq 1 40); do docker exec "$RPC" rpk cluster health --exit-when-healthy >/dev/null 2>&1 && break; sleep 1.5; done
docker exec "$RPC" rpk cluster health --exit-when-healthy >/dev/null 2>&1 && pass "redpanda on $RP_BROKERS" || { fail "redpanda"; exit 1; }

MIG_LOG="$WORK/migrate.log"
"$BIN/zeroship-platform-migrate" \
  --database-url "$DBURL" --migrations-dir "$ROOT/db/migrations-ts" \
  --project-schema zeroship --project-id zeroship > "$MIG_LOG" 2>&1 \
  && pass "zeroship platform migrations applied" || { fail "migrate"; tail -20 "$MIG_LOG"; exit 1; }

PLAN_ID="pln_acct_e2e"
psql_exec >/dev/null 2>&1 <<SQL && pass "seeded plan (high spend limit — isolate the ACCOUNT gate from spend)" || { fail "plan seed"; exit 1; }
INSERT INTO zeroship.plans (id,name,base_fee_cents,included_units,fx_pico_cents_per_unit,runtime_limits_json,spend_limit_default_cents)
VALUES ('$PLAN_ID','acct-e2e',0,0,1000000000000,'{"cpu_limit_ms":5000,"wall_timeout_ms":30000,"heap_limit_mb":256}',100000000) ON CONFLICT (id) DO NOTHING;
INSERT INTO zeroship.pricing_config (id,fx_pico_cents_per_unit) VALUES ('global',1000000000000) ON CONFLICT (id) DO UPDATE SET fx_pico_cents_per_unit=EXCLUDED.fx_pico_cents_per_unit;
INSERT INTO zeroship.billing_metrics (metric,kind,unit) VALUES ('requests','platform','op') ON CONFLICT (metric) DO UPDATE SET kind='platform';
INSERT INTO zeroship.metric_weights (metric,units_per_op,per_units) VALUES ('requests',1,1) ON CONFLICT (metric) DO UPDATE SET units_per_op=1,per_units=1;
SQL

openssl genpkey -algorithm ed25519 -out "$WORK/sk.pem" 2>/dev/null; chmod 600 "$WORK/sk.pem"
openssl rand -base64 48 > "$WORK/gate-broker-secret"; chmod 600 "$WORK/gate-broker-secret"
CFG_TOML="$WORK/zeroship.toml"
printf '[metering]\nredpanda_brokers = "%s"\nusage_events_topic = "%s"\n' "$RP_BROKERS" "$USAGE_TOPIC" > "$CFG_TOML"

SIGNING_KEY_FILE="$WORK/sk.pem"
GATEWAY_SIGNING_KEY_FILE="$SIGNING_KEY_FILE"
GATEWAY_BROKER_SECRET_FILE="$WORK/gate-broker-secret"
e2e_export_runtime_secrets "$WORK" || exit 1
"$BIN/zeroship-control" --port "$CONTROL_PORT" --db "$DBURL" --config "$CFG_TOML" \
  --blob-store "$WORK/blobs" --signing-key-file "$WORK/sk.pem" \
  --stripe-base-url "http://127.0.0.1:1" --stripe-secret-key "sk_test_unused" \
  --meter-provider lite --invoicer-provider lite --allow-unsupported-billing \
  --spend-recompute-interval 2 > "$WORK/control.log" 2>&1 &
echo $! >> "$PIDFILE"
for _ in $(seq 1 30); do curl -sf "$CONTROL_URL/health" >/dev/null 2>&1 && break; sleep 1; done
curl -sf "$CONTROL_URL/health" >/dev/null 2>&1 && pass "control healthy (lite provider)" || { fail "control"; tail -30 "$WORK/control.log"; exit 1; }

USAGE_OUTBOX_WAL_PATH="$WORK/worker-outbox.redb" "$BIN/zeroship-worker" --port "$WORKER_PORT" --worker-threads 2 \
  --config "$CFG_TOML" --control "$CONTROL_URL" --db "$DBURL" --blob-store "$WORK/blobs" --poll-interval 2 > "$WORK/worker.log" 2>&1 &
echo $! >> "$PIDFILE"
for _ in $(seq 1 30); do curl -sf "http://localhost:$WORKER_PORT/health" >/dev/null 2>&1 && break; sleep 1; done
curl -sf "http://localhost:$WORKER_PORT/health" >/dev/null 2>&1 && pass "worker healthy" || { fail "worker"; tail -30 "$WORK/worker.log"; exit 1; }

USAGE_OUTBOX_WAL_PATH="$WORK/gate-outbox.redb" "$BIN/zeroship-gate" --port "$GATE_PORT" --control "$CONTROL_URL" \
  --config "$CFG_TOML" --workers "http://localhost:$WORKER_PORT" --blob-store "$WORK/blobs" --blob-cache-disk-root "$WORK/blob-cache" \
  --db "$DBURL" --poll-interval 2 --signing-key-file "$WORK/sk.pem" --gateway-broker-secret-file "$WORK/gate-broker-secret" > "$WORK/gate.log" 2>&1 &
echo $! >> "$PIDFILE"
for _ in $(seq 1 30); do curl -sf "http://localhost:$GATE_PORT/health" >/dev/null 2>&1 && break; sleep 1; done
curl -sf "http://localhost:$GATE_PORT/health" >/dev/null 2>&1 && pass "gateway healthy" || { fail "gateway"; tail -30 "$WORK/gate.log"; exit 1; }

echo ""; echo "=== Stage 2: PAT + creator + app + deploy + creator_billing ==="
POLICY_JSON='{"name":"e2e-acct","statements":[{"effect":"allow","actions":["apps:read","apps:write","apps:deploy","billing:read","billing:write"],"resources":[{"type":"any"}],"conditions":[]}]}'
POLICY_HASH="$(node -e 'const{createHash}=require("crypto");function c(v){if(v===null||typeof v==="number"||typeof v==="boolean")return JSON.stringify(v);if(typeof v==="string")return JSON.stringify(v);if(Array.isArray(v))return "["+v.map(c).join(",")+"]";return "{"+Object.keys(v).sort().map(k=>JSON.stringify(k)+":"+c(v[k])).join(",")+"}";}process.stdout.write(createHash("sha256").update(c(JSON.parse(process.argv[1]))).digest("hex"));' "$POLICY_JSON")"
CREATOR="$(node -e 'console.log(require("crypto").randomUUID())')"; TOKID="$(node -e 'console.log(require("crypto").randomUUID())')"; EXP=$(( $(date +%s) + 86400 ))
psql_exec >/dev/null 2>&1 <<SQL
INSERT INTO zeroship.users (id,email,name,email_verified_at) VALUES ('$CREATOR','e2e-acct-$CREATOR@zeroship.test'::citext,'E2E Acct',NOW());
INSERT INTO zeroship.platform_admin_roles (user_id,role,granted_by) VALUES ('$CREATOR','admin','$CREATOR');
INSERT INTO zeroship.permission_tokens (id,owner_id,kind,name,policies,policy_hash,expires_at) VALUES ('$TOKID','$CREATOR','pat','e2e acct','$POLICY_JSON'::jsonb,'$POLICY_HASH',to_timestamp($EXP));
INSERT INTO zeroship.creator_billing (creator_id) VALUES ('$CREATOR') ON CONFLICT (creator_id) DO NOTHING;
SQL
PAT="$(node --input-type=module -e 'import{readFileSync}from "node:fs";import{createHash,randomBytes}from "node:crypto";import{importPKCS8,exportJWK,SignJWT}from "file://'"$JOSE"'";const[pem,owner,tid,phash,exp]=process.argv.slice(1);const key=await importPKCS8(readFileSync(pem,"utf8"),"EdDSA",{extractable:true});const x=(await exportJWK(key)).x;const kid=createHash("sha256").update(`{"crv":"Ed25519","kty":"OKP","x":"${x}"}`).digest("base64url");const jwt=await new SignJWT({sub:owner,owner,tid,jti:tid,scope:"pat",policy_hash:phash,nonce:randomBytes(32).toString("base64url")}).setProtectedHeader({alg:"EdDSA",typ:"pat+jwt",kid}).setIssuer("https://api.zeroship.ai").setAudience("control.zeroship.ai").setIssuedAt(Math.floor(Date.now()/1000)).setExpirationTime(Number(exp)).sign(key);process.stdout.write(jwt);' "$WORK/sk.pem" "$CREATOR" "$TOKID" "$POLICY_HASH" "$EXP")"
[ "$(echo -n "$PAT" | awk -F. '{print NF}')" = "3" ] && pass "minted PAT + creator_billing row (creator=$CREATOR)" || { fail "PAT"; exit 1; }
APP="$(curl -s -X POST "$CONTROL_URL/api/apps" -H 'Content-Type: application/json' -H "Authorization: Bearer $PAT" -d "{\"name\":\"acct-probe\",\"plan_id\":\"$PLAN_ID\"}" | jget '.id')"
[ -n "$APP" ] && pass "created app $APP" || { fail "create app"; exit 1; }
psql_exec >/dev/null 2>&1 <<SQL
INSERT INTO zeroship.app_members (app_id,user_id,role) VALUES ('$APP','$CREATOR','owner') ON CONFLICT (app_id,user_id) DO UPDATE SET role='owner';
SQL
"$BIN/zeroship" deploy "$PROBE" --app="$APP" --control="$CONTROL_URL" --token="$PAT" 2>&1 | grep -q deploy_hash && pass "deployed probe" || { fail "deploy"; exit 1; }
sleep 5

echo ""; echo "=== Stage 3: reachable while Active (baseline) ==="
READY=0; for _ in $(seq 1 30); do [ "$(curl -s -o /dev/null -w '%{http_code}' -H 'Host: acct-probe.localhost' "http://localhost:$GATE_PORT/probe/ready")" = "200" ] && { READY=1; break; }; sleep 1; done
[ "$READY" = "1" ] && pass "app reachable (default Active — no status row)" || { fail "app never reachable"; tail -15 "$WORK/gate.log"; exit 1; }

echo ""; echo "=== Stage 4: walk account states (creator_billing_status.state) ==="
# Match: a served state needs only HTTP 200 (the probe's 200 body is arbitrary);
# a blocked state needs 402 AND the exact gateway body code. The gateway pulls
# account_state on its ~2s sync, so poll until it reflects the expected result.
matches(){ if [ "$2" = "402" ]; then [ "$3" = "$2" ] && [ "$4" = "$5" ]; else [ "$3" = "$2" ]; fi; }
assert_acct(){ # $1=label $2=state($2="" ⇒ no row) $3=expect_code $4=expect_bodycode
  local label="$1" state="$2" want_code="$3" want_bc="$4" rr code bc
  [ -n "$state" ] && set_acct "$CREATOR" "$state"
  code=000; bc=none
  for _ in $(seq 1 8); do rr="$(probe_req "$label")"; code="${rr%% *}"; bc="${rr##* }"; matches "$label" "$want_code" "$code" "$bc" "$want_bc" && break; sleep 1.5; done
  matches "$label" "$want_code" "$code" "$bc" "$want_bc" \
    && pass "$label (state='${state:-<none>}'): gateway HTTP $code$([ "$want_code" = 402 ] && echo " code=$bc")" \
    || fail "$label (state='${state:-<none>}'): got HTTP $code code=$bc (want $want_code$([ "$want_code" = 402 ] && echo " / $want_bc"))"
}
assert_acct "ACTIVE"     active    200 none
assert_acct "PAST_DUE"   past_due  200 none               # dunning grace — still served
assert_acct "SUSPENDED"  suspended 402 ACCOUNT_SUSPENDED  # hard gate
assert_acct "RECOVER"    active    200 none               # payment recovered → served again

echo ""; echo "=== Stage 5: Suspended 402s BEFORE spend is consulted (gate ordering) ==="
# Give the app a tiny spend limit (would 402 SPEND_LIMIT) AND suspend it; assert
# the 402 is ACCOUNT_SUSPENDED, proving check_account runs first (enforce.rs).
curl -s -o /dev/null -X PUT "$CONTROL_URL/api/apps/$APP/spend-limit" -H 'Content-Type: application/json' -H "Authorization: Bearer $PAT" -d '{"cents":1}'
curl -s -o /dev/null -X POST -H "Authorization: Bearer $CONTROL_KEY" "$CONTROL_URL/internal/spend/reconcile"
set_acct "$CREATOR" suspended
code=000; bc=none
for _ in $(seq 1 8); do rr="$(probe_req ordering)"; code="${rr%% *}"; bc="${rr##* }"; { [ "$code" = "402" ] && [ "$bc" = "ACCOUNT_SUSPENDED" ]; } && break; sleep 1.5; done
{ [ "$code" = "402" ] && [ "$bc" = "ACCOUNT_SUSPENDED" ]; } \
  && pass "Suspended + over-spend → 402 ACCOUNT_SUSPENDED (account gate precedes spend gate)" \
  || fail "gate ordering: got HTTP $code code=$bc (want 402 / ACCOUNT_SUSPENDED)"

echo ""; echo "============================================"
echo "  Results: $PASS passed, $FAIL failed"
echo "============================================"
# A floor on assertions that RAN, not that PASSED. Measured 2026-08-11 on a clean
# run: 16 assertions. PASS+FAIL because a mutation moves an outcome BETWEEN those
# columns; only a LOST assertion drops the sum (#285/#286). Not decorative here -
# disabling check_account in the gateway gave 14 passed / 2 failed = 16 RAN, so the
# denominator held while two verdicts flipped, which is exactly what a floor on
# PASS alone would have mistaken for a smaller run.
ACCT_MIN_RAN="${ACCT_MIN_RAN:-16}"
RAN=$((PASS + FAIL))
rc=0
[ "$FAIL" -eq 0 ] || rc=1
if [ "$RAN" -lt "$ACCT_MIN_RAN" ]; then
  echo "FAIL: only $RAN assertions RAN, fewer than the $ACCT_MIN_RAN this gate expects." >&2
  echo "      (passed $PASS, failed $FAIL - the floor counts both, because a failing" >&2
  echo "      assertion still ran and is already caught above.)" >&2
  rc=1
fi
exit $rc
