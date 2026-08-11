#!/usr/bin/env bash
# ============================================================================
# e2e_spend_state_transitions.sh — NEW-COVERAGE E2E for the spend-enforcement
# STATE MACHINE end to end: Allow → Warn → Degrade → Block.
#
# The other billing e2es only exercise the Block (402) leg. This walks ALL four
# spend states and asserts both the control-derived state AND the gateway's
# per-state behavior:
#
#   spend engine (crates/control/src/spend.rs derive_state):
#     pct = spend*100/limit ; Warn>=80, Degrade>=95, Block>=100
#   gateway enforcement (crates/gateway/src/enforce.rs + router/dispatch.rs):
#     Allow   -> pass, no header
#     Warn    -> pass + `x-zs-spend-warn: 1` response header
#     Degrade -> pass (throttled 1/DEGRADE_FACTOR, NOT blocked)
#     Block   -> 402 SPEND_LIMIT
#
# DETERMINISTIC design: drive a fixed usage ONCE, read its projected charge C
# (cents), then move through the bands by changing ONLY the spend_limit and
# re-reconciling (a limit change re-derives the state immediately, bypassing the
# anti-flap deadband). Enforcement is provider-independent, so this uses the
# `lite` provider (no external billing account).
#
# REFUSES (exit 1) when docker is unavailable - a run that asserted nothing
# is not a passing run. KEEP_WORK=1 preserves logs.
# ============================================================================
set -uo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"; BIN="$ROOT/target/release"
PASS=0; FAIL=0
pass(){ PASS=$((PASS+1)); echo "  ✓ $1"; }
fail(){ FAIL=$((FAIL+1)); echo "  ✗ $1"; }

echo "============================================"
echo "  zeroship E2E — spend-state transitions (Allow→Warn→Degrade→Block)"
echo "============================================"
# Docker unavailable is a REFUSAL, not a skip. This used to `exit 0` after
# printing a warning, so on any machine without docker the harness reported
# success having driven none of the four spend bands - the same "0 failed over
# 0 assertions" shape as tasks #102/#103/#279. Proven before changing it: with
# a stub `docker` returning 1 on PATH, the old arm printed
# "SKIP: docker unavailable." and exited 0.
if ! command -v docker >/dev/null 2>&1 || ! docker info >/dev/null 2>&1; then
  echo "  ✗ REFUSED: docker unavailable, so NONE of the Allow/Warn/Degrade/Block" >&2
  echo "    bands were driven. Exiting non-zero: a run that asserted nothing is" >&2
  echo "    not a passing run. Start docker and re-run." >&2
  exit 1
fi
for b in zeroship zeroship-control zeroship-gate zeroship-worker zeroship-platform-migrate; do [ -x "$BIN/$b" ] || { echo "missing $BIN/$b"; exit 2; }; done
command -v node >/dev/null && command -v openssl >/dev/null && command -v curl >/dev/null || { echo "need node/openssl/curl"; exit 2; }
PROBE="$ROOT/examples/metering-probe/dist/app.zship"; [ -f "$PROBE" ] || { echo "missing $PROBE"; exit 2; }
JOSE="$ROOT/node_modules/.pnpm/jose@6.2.3/node_modules/jose/dist/webapi/index.js"; [ -f "$JOSE" ] || { echo "missing jose"; exit 2; }

CONTROL_PORT=9176; WORKER_PORT=8076; GATE_PORT=8066; PG_PORT=5476; RP_PORT=19176
PGC=zs-e2e-spend-pg; RPC=zs-e2e-spend-redpanda
DBURL="postgres://postgres:zeroship@localhost:$PG_PORT/zeroship"; CONTROL_URL="http://localhost:$CONTROL_PORT"
RP_BROKERS="127.0.0.1:$RP_PORT"; USAGE_TOPIC="zeroship-usage-spend-e2e"
WORK="$(mktemp -d -t zs-e2e-spend-XXXXXX)"; mkdir -p "$WORK/blobs" "$WORK/blob-cache"; PIDFILE="$WORK/pids"; : > "$PIDFILE"
jget(){ node -e "let s='';process.stdin.on('data',d=>s+=d).on('end',()=>{try{const o=JSON.parse(s);process.stdout.write(String(o$1??'')+'\n')}catch(e){console.log('')}})"; }
psql_exec(){ docker exec -i "$PGC" psql -U postgres -d zeroship -v ON_ERROR_STOP=1 "$@"; }
spend_state(){ psql_exec -tA -c "SELECT state::text FROM zeroship.app_spend_state WHERE app_id='$1'" 2>/dev/null | tr -d '[:space:]'; }
set_limit(){ curl -s -o /dev/null -X PUT "$CONTROL_URL/api/apps/$1/spend-limit" -H 'Content-Type: application/json' -H "Authorization: Bearer $PAT" -d "{\"cents\":$2}"; curl -s -o /dev/null -X POST "$CONTROL_URL/internal/spend/reconcile"; }
# GET one probe request; echo "<status> <warnheader:0|1>"
probe_req(){ local out; out="$(curl -s -D - -o /dev/null -H 'Host: spend-probe.localhost' "http://localhost:$GATE_PORT/probe/$1" 2>/dev/null)"; local code warn; code="$(printf '%s' "$out" | awk 'NR==1{print $2}')"; warn=0; printf '%s' "$out" | grep -qiE '^x-zs-spend-warn:' && warn=1; echo "${code:-000} $warn"; }

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
# Readiness = three CONSECUTIVE successful queries, not one pg_isready. This is
# the same fix tests/lib/e2e_stack.sh's stack_pg_up carries, and the same bug
# tests/e2e_db_app_end_to_end.sh had (task #274): the postgres entrypoint runs
# an initdb-phase server on the unix socket and RESTARTS it, so there is a
# window where `pg_isready` answers yes and the very next `psql` fails. This
# harness never sourced the library, so it kept the old probe after the library
# was fixed. Consecutive-ness is the point: a single successful select can land
# inside the same window.
PG_OK=0
for _ in $(seq 1 60); do
  if docker exec "$PGC" psql -U postgres -d zeroship -tAc 'select 1' >/dev/null 2>&1; then
    PG_OK=$((PG_OK + 1))
    [ "$PG_OK" -ge 3 ] && break
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

PLAN_ID="pln_spend_e2e"
# FX 1e12 pico-cents/unit = 1 cent per CU; requests weight 1 CU/op → 1 cent/request.
psql_exec >/dev/null 2>&1 <<SQL && pass "seeded plan + pricing (1 cent/request) + weights" || { fail "plan seed"; exit 1; }
INSERT INTO zeroship.plans (id,name,base_fee_cents,included_units,fx_pico_cents_per_unit,runtime_limits_json,spend_limit_default_cents)
VALUES ('$PLAN_ID','spend-e2e',0,0,1000000000000,'{"cpu_limit_ms":5000,"wall_timeout_ms":30000,"heap_limit_mb":256}',100000000) ON CONFLICT (id) DO NOTHING;
INSERT INTO zeroship.pricing_config (id,fx_pico_cents_per_unit) VALUES ('global',1000000000000) ON CONFLICT (id) DO UPDATE SET fx_pico_cents_per_unit=EXCLUDED.fx_pico_cents_per_unit;
INSERT INTO zeroship.billing_metrics (metric,kind,unit) VALUES ('requests','platform','op') ON CONFLICT (metric) DO UPDATE SET kind='platform';
INSERT INTO zeroship.metric_weights (metric,units_per_op,per_units) VALUES ('requests',1,1) ON CONFLICT (metric) DO UPDATE SET units_per_op=1,per_units=1;
SQL

openssl genpkey -algorithm ed25519 -out "$WORK/sk.pem" 2>/dev/null; chmod 600 "$WORK/sk.pem"
openssl rand -base64 48 > "$WORK/gate-broker-secret"; chmod 600 "$WORK/gate-broker-secret"
CFG_TOML="$WORK/zeroship.toml"
printf '[metering]\nredpanda_brokers = "%s"\nusage_events_topic = "%s"\n' "$RP_BROKERS" "$USAGE_TOPIC" > "$CFG_TOML"

"$BIN/zeroship-control" --port "$CONTROL_PORT" --db "$DBURL" --config "$CFG_TOML" \
  --blob-store "$WORK/blobs" --signing-key-file "$WORK/sk.pem" \
  --stripe-base-url "http://127.0.0.1:1" --stripe-secret-key "sk_test_unused" \
  --meter-provider lite --invoicer-provider lite --allow-unsupported-billing \
  --spend-recompute-interval 2 --dev-insecure > "$WORK/control.log" 2>&1 &
echo $! >> "$PIDFILE"
for _ in $(seq 1 30); do curl -sf "$CONTROL_URL/health" >/dev/null 2>&1 && break; sleep 1; done
curl -sf "$CONTROL_URL/health" >/dev/null 2>&1 && pass "control healthy (lite provider, stream=redpanda)" || { fail "control"; tail -30 "$WORK/control.log"; exit 1; }

USAGE_OUTBOX_WAL_PATH="$WORK/worker-outbox.redb" "$BIN/zeroship-worker" --port "$WORKER_PORT" --worker-threads 2 \
  --config "$CFG_TOML" --control "$CONTROL_URL" --db "$DBURL" --blob-store "$WORK/blobs" --poll-interval 2 --dev-insecure > "$WORK/worker.log" 2>&1 &
echo $! >> "$PIDFILE"
for _ in $(seq 1 30); do curl -sf "http://localhost:$WORKER_PORT/health" >/dev/null 2>&1 && break; sleep 1; done
curl -sf "http://localhost:$WORKER_PORT/health" >/dev/null 2>&1 && pass "worker healthy" || { fail "worker"; tail -30 "$WORK/worker.log"; exit 1; }

USAGE_OUTBOX_WAL_PATH="$WORK/gate-outbox.redb" "$BIN/zeroship-gate" --port "$GATE_PORT" --control "$CONTROL_URL" \
  --config "$CFG_TOML" --workers "http://localhost:$WORKER_PORT" --blob-store "$WORK/blobs" --blob-cache-disk-root "$WORK/blob-cache" \
  --db "$DBURL" --poll-interval 2 --signing-key-file "$WORK/sk.pem" --gateway-broker-secret-file "$WORK/gate-broker-secret" --dev-insecure > "$WORK/gate.log" 2>&1 &
echo $! >> "$PIDFILE"
for _ in $(seq 1 30); do curl -sf "http://localhost:$GATE_PORT/health" >/dev/null 2>&1 && break; sleep 1; done
curl -sf "http://localhost:$GATE_PORT/health" >/dev/null 2>&1 && pass "gateway healthy" || { fail "gateway"; tail -30 "$WORK/gate.log"; exit 1; }

echo ""; echo "=== Stage 2: PAT + creator + app + deploy ==="
POLICY_JSON='{"name":"e2e-spend","statements":[{"effect":"allow","actions":["apps:read","apps:write","apps:deploy","billing:read","billing:write"],"resources":[{"type":"any"}],"conditions":[]}]}'
POLICY_HASH="$(node -e 'const{createHash}=require("crypto");function c(v){if(v===null||typeof v==="number"||typeof v==="boolean")return JSON.stringify(v);if(typeof v==="string")return JSON.stringify(v);if(Array.isArray(v))return "["+v.map(c).join(",")+"]";return "{"+Object.keys(v).sort().map(k=>JSON.stringify(k)+":"+c(v[k])).join(",")+"}";}process.stdout.write(createHash("sha256").update(c(JSON.parse(process.argv[1]))).digest("hex"));' "$POLICY_JSON")"
CREATOR="$(node -e 'console.log(require("crypto").randomUUID())')"; TOKID="$(node -e 'console.log(require("crypto").randomUUID())')"; EXP=$(( $(date +%s) + 86400 ))
psql_exec >/dev/null 2>&1 <<SQL
INSERT INTO zeroship.users (id,email,name,email_verified_at) VALUES ('$CREATOR','e2e-spend-$CREATOR@zeroship.test'::citext,'E2E Spend',NOW());
INSERT INTO zeroship.platform_admin_roles (user_id,role,granted_by) VALUES ('$CREATOR','admin','$CREATOR');
INSERT INTO zeroship.permission_tokens (id,owner_id,kind,name,policies,policy_hash,expires_at) VALUES ('$TOKID','$CREATOR','pat','e2e spend','$POLICY_JSON'::jsonb,'$POLICY_HASH',to_timestamp($EXP));
SQL
PAT="$(node --input-type=module -e 'import{readFileSync}from "node:fs";import{createHash,randomBytes}from "node:crypto";import{importPKCS8,exportJWK,SignJWT}from "file://'"$JOSE"'";const[pem,owner,tid,phash,exp]=process.argv.slice(1);const key=await importPKCS8(readFileSync(pem,"utf8"),"EdDSA",{extractable:true});const x=(await exportJWK(key)).x;const kid=createHash("sha256").update(`{"crv":"Ed25519","kty":"OKP","x":"${x}"}`).digest("base64url");const jwt=await new SignJWT({sub:owner,owner,tid,jti:tid,scope:"pat",policy_hash:phash,nonce:randomBytes(32).toString("base64url")}).setProtectedHeader({alg:"EdDSA",typ:"pat+jwt",kid}).setIssuer("https://api.zeroship.ai").setAudience("control.zeroship.ai").setIssuedAt(Math.floor(Date.now()/1000)).setExpirationTime(Number(exp)).sign(key);process.stdout.write(jwt);' "$WORK/sk.pem" "$CREATOR" "$TOKID" "$POLICY_HASH" "$EXP")"
[ "$(echo -n "$PAT" | awk -F. '{print NF}')" = "3" ] && pass "minted PAT (creator=$CREATOR)" || { fail "PAT"; exit 1; }
APP="$(curl -s -X POST "$CONTROL_URL/api/apps" -H 'Content-Type: application/json' -H "Authorization: Bearer $PAT" -d "{\"name\":\"spend-probe\",\"plan_id\":\"$PLAN_ID\"}" | jget '.id')"
[ -n "$APP" ] && pass "created app $APP" || { fail "create app"; exit 1; }
psql_exec >/dev/null 2>&1 <<SQL
INSERT INTO zeroship.app_members (app_id,user_id,role) VALUES ('$APP','$CREATOR','owner') ON CONFLICT (app_id,user_id) DO UPDATE SET role='owner';
SQL
"$BIN/zeroship" deploy "$PROBE" --app="$APP" --control="$CONTROL_URL" --token="$PAT" 2>&1 | grep -q deploy_hash && pass "deployed probe" || { fail "deploy"; exit 1; }
sleep 5

echo ""; echo "=== Stage 3: drive fixed usage → read projected charge C ==="
N_REQ=100; BODY='{"hello":"spend"}'
READY=0; for _ in $(seq 1 30); do [ "$(curl -s -o /dev/null -w '%{http_code}' -H 'Host: spend-probe.localhost' -H 'content-type: application/json' --data "$BODY" "http://localhost:$GATE_PORT/probe/ready")" = "200" ] && { READY=1; break; }; sleep 1; done
[ "$READY" = "1" ] && pass "app reachable" || { fail "app never reachable"; tail -15 "$WORK/gate.log"; exit 1; }
GW_OK=0; for i in $(seq 1 $N_REQ); do for _ in 1 2 3; do [ "$(curl -s -o /dev/null -w '%{http_code}' -H 'Host: spend-probe.localhost' -H 'content-type: application/json' --data "$BODY" "http://localhost:$GATE_PORT/probe/$i")" = "200" ] && { GW_OK=$((GW_OK+1)); break; }; sleep 0.2; done; done
[ "$GW_OK" = "$N_REQ" ] && pass "drove $GW_OK/$N_REQ requests" || { fail "traffic $GW_OK/$N_REQ"; exit 1; }
# Wait for recompute → usage_aggregates, then read the projected charge (cents).
C=0
for _ in $(seq 1 30); do C="$(curl -s "$CONTROL_URL/api/apps/$APP/projected-charge" -H "Authorization: Bearer $PAT" | jget '.projected_charge_cents')"; [ -n "$C" ] && [ "$C" -ge 20 ] 2>/dev/null && break; sleep 2; done
echo "    projected charge C = $C cents (for $N_REQ requests)"
[ -n "$C" ] && [ "$C" -ge 20 ] 2>/dev/null && pass "projected charge readable + non-trivial ($C cents ≥ 20)" || { fail "projected charge too small/absent (C=$C); can't form clean bands"; tail -20 "$WORK/control.log"; exit 1; }

echo ""; echo "=== Stage 4: walk the bands by moving the spend limit (spend ≈ C) ==="
# Each band re-reads the CURRENT projected charge and sizes the limit for a target
# pct: limit = curC*100/P → pct = curC*100/limit ≈ P. Re-reading per band makes
# the narrow Degrade band [95,100) robust to the ~1-request usage drift the single
# probe per band adds.
read_charge(){ curl -s "$CONTROL_URL/api/apps/$APP/projected-charge" -H "Authorization: Bearer $PAT" | jget '.projected_charge_cents'; }
assert_band(){ # $1=label $2=target_pct $3=expect_state $4=expect_gw_code $5=expect_warn(0|1)
  local label="$1" pct="$2" want_state="$3" want_code="$4" want_warn="$5" curC lim st code warn rr
  curC="$(read_charge)"; [ -n "$curC" ] && [ "$curC" -ge 1 ] 2>/dev/null || { fail "$label: projected charge unreadable ($curC)"; return; }
  lim=$(( curC*100/pct )); [ "$lim" -lt 1 ] && lim=1
  set_limit "$APP" "$lim"
  st=""; for _ in $(seq 1 15); do st="$(spend_state "$APP")"; [ "$st" = "$want_state" ] && break; curl -s -o /dev/null -X POST "$CONTROL_URL/internal/spend/reconcile"; sleep 1; done
  [ "$st" = "$want_state" ] && pass "$label: charge=${curC}c limit=${lim}c (~${pct}%) → control state='$st'" || { fail "$label: state='$st' (want '$want_state'; charge=$curC limit=$lim)"; return; }
  # The gateway PULLS the spend_state on its ~2s poll, so probe results lag the
  # control state. Poll the probe until it reflects the expected code + header
  # (bounded), rather than racing the sync with a single request.
  code=000; warn=0
  for _ in $(seq 1 8); do rr="$(probe_req "$label")"; code="${rr%% *}"; warn="${rr##* }"; { [ "$code" = "$want_code" ] && [ "$warn" = "$want_warn" ]; } && break; sleep 1.5; done
  [ "$code" = "$want_code" ] && pass "$label: gateway returned HTTP $code" || fail "$label: gateway HTTP $code (want $want_code)"
  if [ "$want_warn" = "1" ]; then
    [ "$warn" = "1" ] && pass "$label: gateway stamped x-zs-spend-warn header" || fail "$label: missing x-zs-spend-warn header"
  else
    [ "$warn" = "0" ] && pass "$label: no x-zs-spend-warn header (correct for $want_state)" || fail "$label: unexpected x-zs-spend-warn header in $want_state"
  fi
}
assert_band "ALLOW"   20  allow   200 0   # ~20% of limit
assert_band "WARN"    85  warn    200 1   # ~85% → Warn + header
assert_band "DEGRADE" 97  degrade 200 0   # ~97% → Degrade (throttled, not blocked)
assert_band "BLOCK"   105 block   402 0   # ~105% → Block (402)

echo ""; echo "=== Stage 5: recovery — raise the limit back → Allow (deadband bypassed on limit change) ==="
assert_band "RECOVER" 20 allow 200 0

echo ""; echo "============================================"
echo "  Results: $PASS passed, $FAIL failed"
echo "============================================"

# A floor on assertions that RAN, not that PASSED. Measured 2026-08-11 on a
# clean run: 28 assertions. The floor counts PASS+FAIL because a mutation moves
# an outcome BETWEEN those columns; only a LOST assertion drops the sum, which
# is the case this catches (tasks #285/#286).
#
# It is not decorative here. Mutating the spend engine so the Block band cannot
# be reached (`pct >= t.block_pct * 2` in crates/control/src/spend.rs) gave
# 25 passed / 1 failed = 26 RAN, not 27: the band's state check failed and its
# TWO gateway-behaviour assertions never ran at all. So the denominator moves
# when a band breaks, and a floor phrased on PASS alone would not have seen it.
SPEND_MIN_RAN="${SPEND_MIN_RAN:-28}"
RAN=$((PASS + FAIL))
rc=0
[ "$FAIL" -eq 0 ] || rc=1
if [ "$RAN" -lt "$SPEND_MIN_RAN" ]; then
  echo "FAIL: only $RAN assertions RAN, fewer than the $SPEND_MIN_RAN this gate expects." >&2
  echo "      (passed $PASS, failed $FAIL -- the floor counts both, because a failing" >&2
  echo "      assertion still ran and is already caught above. A band whose state check" >&2
  echo "      fails silently drops its two behaviour assertions; that is what this sees.)" >&2
  rc=1
fi
exit $rc
