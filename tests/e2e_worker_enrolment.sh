#!/usr/bin/env bash
# ============================================================================
# tests/e2e_worker_enrolment.sh
#
# THE QUESTION: does a worker instance actually ENROL against the stack this
# repository stands up, and is the row control writes the address control
# OBSERVED?
#
# Why this cannot be a crate-local test. `crates/zeroship-control/tests/
# worker_enrolment_test.rs` already drives the handler over a real socket
# against a real PostgreSQL, and it declares its own envelope in Rust. What it
# cannot see is whether any DEPLOYMENT declares one. Until tests/lib/e2e_stack.sh
# grew the declaration this harness depends on, NOTHING in this repository did:
# `EnrolmentEnvelope::is_declared` needs a non-empty network list AND a port
# range, so every control plane in the tree booted closed and answered
# `envelope_unset` with a 503 to every enrolment. A gap in configuration is
# invisible to a test that supplies its own configuration.
#
# WHAT IS ASSERTED, and the one-variable control that makes each pair mean
# something:
#
#   A  the stack's control plane, with the envelope tests/lib/e2e_stack.sh
#      declares, ADMITS an enrolment from loopback claiming the port the worker
#      is really listening on, and the row carries the DERIVED address.
#   B  a SECOND control plane, same binary, same database, same peer document,
#      same request - with the network half of the envelope cleared and NOTHING
#      ELSE CHANGED - refuses it with `envelope_unset` and writes no row.
#   C  the REAL `zeroship-worker` binary the stack booted enrolled ITSELF, and
#      the row it produced carries the derived address, a public key that is
#      NOT its unit's on-disk enroller key, and that enroller's id. A and B
#      drive the endpoint as the worker would, minting under the unit's
#      enroller; C is the only arm that rules on the worker's own boot path.
#   D  a real worker pointed at the UNDECLARED control plane REFUSES TO START -
#      it exits non-zero and never serves - against a control differing in the
#      `--control-url` and nothing else, which boots and enrols.
#
# Without B, A cannot tell a working declaration from an endpoint that admits
# everything. The one variable is `control.worker_enrolment_networks`; the port
# half is left declared on purpose, so B's refusal names the deployment state
# rather than a second missing setting.
#
# Without D's control arm, D's refusal is satisfied by a worker that cannot
# start anywhere. Without D at all, C is satisfied by a worker that enrols when
# it can and boots anyway when it cannot - which is the defect shape a fence
# that logs instead of exiting produces, because it prints what success prints.
#
# NO PORT IS A LITERAL ANYWHERE IN THIS FILE. The declaration derives its range
# from the two reserved worker ports, the enrolment claims the same variables,
# and A additionally proves something is really listening there - so a
# declaration that drifted off the worker's port fails here instead of failing
# the first time a worker boots.
#
# Run: tests/e2e_worker_enrolment.sh
# ============================================================================
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT" || exit 1

PASS=0
FAIL=0
pass() { PASS=$((PASS + 1)); echo "  RESULT ok   $1"; }
fail() { FAIL=$((FAIL + 1)); echo "  RESULT FAIL $1"; }
note() { echo "  ---- $1"; }

# shellcheck source=tests/lib/gate_arms.sh
source "$ROOT/tests/lib/gate_arms.sh"
gate_arms_init worker_enrolment

# Ports and the container name are PER RUN, so two agents running this file do
# not evict each other's control plane or Postgres. Reserved BEFORE the stack
# library is sourced: its `:=` defaults leave an allocation alone.
# shellcheck source=tests/lib/e2e_ports.sh
source "$ROOT/tests/lib/e2e_ports.sh"
zs_ports_reserve ZEROSHIP_CONTROL_PORT ZEROSHIP_WORKER_PORT ZEROSHIP_GATEWAY_PORT \
                 PG_PORT CONTROL_UNDECLARED_PORT WORKER_REFUSAL_PORT || exit 1
RUN_TOKEN="$$_$(date +%s%N)"
export PG_CONTAINER="zs-e2e-enrol-pg-$RUN_TOKEN"

# The declared port range has to cover BOTH workers this harness boots: the
# stack's own, and arm D's. Derived from the two allocations rather than
# written out, for the reason tests/lib/e2e_stack.sh gives about its own
# derivation - a literal drifts the first time a port moves, and control then
# refuses with `port_outside_envelope` while every health probe stays green.
if [ "$ZEROSHIP_WORKER_PORT" -le "$WORKER_REFUSAL_PORT" ]; then
  ZEROSHIP_CONTROL_WORKER_ENROLMENT_PORTS="$ZEROSHIP_WORKER_PORT-$WORKER_REFUSAL_PORT"
else
  ZEROSHIP_CONTROL_WORKER_ENROLMENT_PORTS="$WORKER_REFUSAL_PORT-$ZEROSHIP_WORKER_PORT"
fi
export ZEROSHIP_CONTROL_WORKER_ENROLMENT_PORTS

# shellcheck source=tests/lib/e2e_stack.sh
source "$ROOT/tests/lib/e2e_stack.sh"

cleanup() {
  echo ""
  echo "=== Cleanup ==="
  stack_down
  zs_ports_release
}
trap cleanup EXIT

CONTROL_URL="http://localhost:$ZEROSHIP_CONTROL_PORT"
UNDECLARED_URL="http://localhost:$CONTROL_UNDECLARED_PORT"
psql_q() { docker exec -i "$PG_CONTAINER" psql -U postgres -d zeroship -tAc "$1"; }

echo "=== Stack ==="
stack_up || exit 2

# READ BEFORE ANYTHING ELSE WRITES. Arm C rules on what the stack's own worker
# did while it was booting, and arm A inserts a row with the same address a few
# lines below, so the snapshot has to be taken here or the two become
# indistinguishable.
BOOT_ROWS="$(psql_q "select id || '|' || host(advertise_host) || '|' || advertise_port || '|' || status || '|' || encode(public_key, 'base64') || '|' || enroller_id from zeroship.worker_instances order by registered_at")"
BOOT_ROW_COUNT="$(printf '%s' "$BOOT_ROWS" | grep -c '|' )"

# ---------------------------------------------------------------------------
# C: the REAL worker's own boot path
# ---------------------------------------------------------------------------
echo ""
echo "=== C: the zeroship-worker binary the stack booted enrolled itself ==="

# The unit's ENROLLER key, which is the one key on disk, split out of its
# credential document into a PEM the assertion helper below can sign with. The
# instance key is drawn at boot from the CSPRNG and never written anywhere, so
# the row must NOT carry the enroller's value. Without this comparison the arm
# is satisfied by a worker that enrolled the key it already held - which would
# make every instance of the unit one instance.
ENROLLER_ID="$(node -e 'process.stdout.write(JSON.parse(require("fs").readFileSync(process.argv[1], "utf8")).enroller_id)' "$ZEROSHIP_WORKER_ENROLLER_FILE")"
node -e 'require("fs").writeFileSync(process.argv[2], JSON.parse(require("fs").readFileSync(process.argv[1], "utf8")).private_key, { mode: 0o600 })' \
  "$ZEROSHIP_WORKER_ENROLLER_FILE" "$WORK/enroller.pem"
ROLE_PUB_B64="$(openssl pkey -in "$WORK/enroller.pem" -pubout -outform DER 2>/dev/null \
                 | tail -c 32 | openssl base64 -A)"
C_ROW="$(printf '%s\n' "$BOOT_ROWS" | grep "|$ZEROSHIP_WORKER_PORT|" | head -1)"
C_ID="$(printf '%s' "$C_ROW" | cut -d'|' -f1)"
note "C rows at boot=$BOOT_ROW_COUNT row=${C_ROW:-<none>}"
note "C enroller $ENROLLER_ID public key (must NOT be the row's)=$ROLE_PUB_B64"

c_facts=0
c_ruled() { c_facts=$((c_facts + 1)); }

c_ruled; printf '%s' "$C_ID" | grep -Eq '^wkr_[0-9a-z]{25}$' \
  && pass "C the worker enrolled itself at boot (id $C_ID)" \
  || fail "C no worker_instances row for the stack's worker on :$ZEROSHIP_WORKER_PORT"
c_ruled; [ "$(printf '%s' "$C_ROW" | cut -d'|' -f2)" = "127.0.0.1" ] \
  && pass "C the row carries the OBSERVED loopback address" \
  || fail "C advertise_host is '$(printf '%s' "$C_ROW" | cut -d'|' -f2)', expected 127.0.0.1"
c_ruled; [ "$(printf '%s' "$C_ROW" | cut -d'|' -f4)" = "active" ] \
  && pass "C the row is active" \
  || fail "C status is '$(printf '%s' "$C_ROW" | cut -d'|' -f4)', expected active"
C_ROW_PUB="$(printf '%s' "$C_ROW" | cut -d'|' -f5)"
c_ruled; [ -n "$C_ROW_PUB" ] && [ -n "$ROLE_PUB_B64" ] && [ "$C_ROW_PUB" != "$ROLE_PUB_B64" ] \
  && pass "C the enrolled key is NOT the on-disk enroller key, so it was drawn at boot" \
  || fail "C enrolled key '${C_ROW_PUB:-<none>}' vs enroller key '${ROLE_PUB_B64:-<none>}'"
c_ruled; [ -n "$ENROLLER_ID" ] && [ "$(printf '%s' "$C_ROW" | cut -d'|' -f6)" = "$ENROLLER_ID" ] \
  && pass "C the row is bound to the unit's enroller $ENROLLER_ID" \
  || fail "C enroller_id is '$(printf '%s' "$C_ROW" | cut -d'|' -f6)', expected '${ENROLLER_ID:-<none>}'"
# The worker has to LEARN its instance id, not merely cause a row: everything
# it mints afterwards is signed under `svc/worker/<that id>`. Its own log is
# where that shows, and grepping it also proves the row came from THIS process
# rather than from a stale one in the shared database.
#
# THE EMPTINESS GUARD IS NOT DEFENSIVE PADDING. Without it `grep -q ""` matches
# every line of any non-empty file, so this assertion PASSED on the pre-change
# tree - where there was no row at all and its four siblings had just failed.
c_ruled; [ -n "$C_ID" ] && grep -q "$C_ID" "$WORK/worker.log" 2>/dev/null \
  && pass "C the worker's own log names the instance id it now mints under" \
  || fail "C $WORK/worker.log never names an instance id (row id '${C_ID:-<none>}')"

# Floor 3 of the assertions above: a collapse of the enumeration - the row
# shape changing, the log line going away - cannot read as clean, and dropping
# one assertion stays a decision rather than a break.
gate_arm worker_enrolled_at_boot "$c_facts" 3 || true

# `:-` so that DELETING the declaration from tests/lib/e2e_stack.sh - the
# mutation this pair exists to be checked against - reaches the assertions below
# and is reported as a refused enrolment, rather than aborting here under `set
# -u` with an unbound-variable message that names no verdict.
note "declared networks=${ZEROSHIP_CONTROL_WORKER_ENROLMENT_NETWORKS:-<undeclared>} ports=${ZEROSHIP_CONTROL_WORKER_ENROLMENT_PORTS:-<undeclared>}"
note "worker listening port=$ZEROSHIP_WORKER_PORT"

# A worker instance keypair is generated AT BOOT, IN MEMORY, NEVER ON DISK, and
# only the public half leaves the process. This is that public half: 32 raw
# ed25519 bytes, base64url without padding. An ed25519 SPKI DER is a fixed
# 12-byte prefix plus the key, so the raw half is the last 32 bytes.
openssl genpkey -algorithm ed25519 -out "$WORK/instance.pem" 2>/dev/null
openssl pkey -in "$WORK/instance.pem" -pubout -outform DER 2>/dev/null \
  | tail -c 32 > "$WORK/instance.pub.raw"
INSTANCE_PUB="$(openssl base64 -A -in "$WORK/instance.pub.raw" | tr '+/' '-_' | tr -d '=')"

# One request body, used against BOTH control planes. There is no host field
# and control offers no way to supply one: the address is derived from the peer
# socket, and the claimed port is the only thing a registrant contributes.
printf '{"port":%s,"public_key":"%s"}' "$ZEROSHIP_WORKER_PORT" "$INSTANCE_PUB" \
  > "$WORK/enrol.json"

# Only an enroller holds CONTROL_WORKER_ENROL, and it mints under an instance
# identifier of its role naming its own row; control is addressed as
# `svc/control`. A fresh assertion per call: `jti` is single use.
enrol_post() {
  local url="$1" out="$2" assertion
  assertion="$(e2e_mint_service_assertion "$WORK/enroller.pem" \
                 "svc/worker-enroller/$ENROLLER_ID" svc/control)" || return 1
  curl -s -o "$out" -w '%{http_code}' -X POST \
    -H "authorization: Bearer $assertion" \
    -H 'content-type: application/json' \
    --data-binary "@$WORK/enrol.json" \
    "$url/internal/workers/enrol"
}

# ---------------------------------------------------------------------------
# A: the declared control plane admits, and writes the DERIVED address
# ---------------------------------------------------------------------------
echo ""
echo "=== A: enrolment against the stack's control plane (envelope declared) ==="

# The port claimed below is only meaningful if something is really listening on
# it. This is that check, and it is what binds the declaration to the worker
# rather than to another copy of the same variable.
WORKER_READY="$(curl -s -o /dev/null -w '%{http_code}' \
  "http://localhost:$ZEROSHIP_WORKER_PORT/readyz")"

A_CODE="$(enrol_post "$CONTROL_URL" "$WORK/enrol-a.json")"
A_BODY="$(cat "$WORK/enrol-a.json" 2>/dev/null)"
A_ID="$(printf '%s' "$A_BODY" | _stk_jget '.instance_id')"
note "A status=$A_CODE body=$A_BODY"

# READ THEN DELETE, BEFORE ANY ASSERTION. A failing assertion aborts nothing
# here, but a `set -e` reader or a future `exit 1` would skip whatever follows
# it, and the row would outlive the run - the reason
# crates/zeroship-control/tests/worker_enrolment_test.rs puts its cleanup first.
A_ROW=""
if [ -n "$A_ID" ]; then
  A_ROW="$(psql_q "select host(advertise_host) || '|' || advertise_port || '|' || status
                     from zeroship.worker_instances where id = '$A_ID'")"
  psql_q "delete from zeroship.worker_instances where id = '$A_ID'" >/dev/null
fi
note "A row(host|port|status)=${A_ROW:-<none>}"

# `a_facts` COUNTS ASSERTIONS THAT RAN, NEVER ASSERTIONS THAT PASSED. A
# mutation moves an outcome between the pass and fail columns and must leave the
# sum alone: only a LOST assertion drops it. Counting passes instead would make
# a real refusal print "this arm examined too little", which names the wrong
# defect - the enumeration would be fine and the tree would not.
a_facts=0
ruled() { a_facts=$((a_facts + 1)); }

ruled; [ "$WORKER_READY" = "200" ] \
  && pass "A the worker really answers on :$ZEROSHIP_WORKER_PORT, so the declared port range covers a live listener" \
  || fail "A nothing answered /readyz on :$ZEROSHIP_WORKER_PORT (HTTP $WORKER_READY); the claimed port would prove nothing"
ruled; [ "$A_CODE" = "201" ] \
  && pass "A enrolment ADMITTED (HTTP 201)" \
  || fail "A enrolment refused (HTTP $A_CODE): $A_BODY"
ruled; printf '%s' "$A_ID" | grep -Eq '^wkr_[0-9a-z]{25}$' \
  && pass "A control minted a typed instance id ($A_ID)" \
  || fail "A instance id is not a wkr_ typed id: '${A_ID:-<none>}'"
ruled; [ "${A_ROW%%|*}" = "127.0.0.1" ] \
  && pass "A advertise_host is the OBSERVED loopback peer, not a claimed host" \
  || fail "A advertise_host is '${A_ROW%%|*}', expected the observed 127.0.0.1"
ruled; [ "$(printf '%s' "$A_ROW" | cut -d'|' -f2)" = "$ZEROSHIP_WORKER_PORT" ] \
  && pass "A advertise_port is the claimed $ZEROSHIP_WORKER_PORT" \
  || fail "A advertise_port is '$(printf '%s' "$A_ROW" | cut -d'|' -f2)', expected $ZEROSHIP_WORKER_PORT"
ruled; [ "$(printf '%s' "$A_ROW" | cut -d'|' -f3)" = "active" ] \
  && pass "A the row is written active" \
  || fail "A status is '$(printf '%s' "$A_ROW" | cut -d'|' -f3)', expected active"

# Floor 4 of the 6 assertions above: enough that a collapse of the enumeration -
# a renamed column, a body shape that stopped parsing - cannot read as clean,
# far enough under 6 that dropping one is a decision rather than a break.
gate_arm admitted_enrolment "$a_facts" 4 || true

# ---------------------------------------------------------------------------
# B: the SAME request against a control plane with the networks half cleared
# ---------------------------------------------------------------------------
echo ""
echo "=== B: one-variable control (worker_enrolment_networks cleared) ==="

# Same binary, same database, same peer document, same port range. The command
# prefix scopes the cleared value to THIS child; nothing is exported, so the
# stack's own control plane above is untouched.
ZEROSHIP_CONTROL_WORKER_ENROLMENT_NETWORKS= \
  "$E2E_BIN/zeroship-control" --port "$CONTROL_UNDECLARED_PORT" \
  --blob-store "$WORK/blobs" > "$WORK/control-undeclared.log" 2>&1 &
echo $! >> "$PIDFILE"
for _ in $(seq 1 30); do
  curl -sf "$UNDECLARED_URL/readyz" >/dev/null 2>&1 && break
  sleep 1
done

B_ROWS_BEFORE="$(psql_q "select count(*) from zeroship.worker_instances")"
if curl -sf "$UNDECLARED_URL/readyz" >/dev/null 2>&1; then
  B_CODE="$(enrol_post "$UNDECLARED_URL" "$WORK/enrol-b.json")"
  B_BODY="$(cat "$WORK/enrol-b.json" 2>/dev/null)"
else
  B_CODE="000"
  B_BODY="the undeclared control plane never became ready"
  tail -20 "$WORK/control-undeclared.log" | sed 's/^/      /'
fi
B_ROWS_AFTER="$(psql_q "select count(*) from zeroship.worker_instances")"
note "B status=$B_CODE body=$B_BODY rows_before=$B_ROWS_BEFORE rows_after=$B_ROWS_AFTER"

b_facts=0
b_ruled() { b_facts=$((b_facts + 1)); }

b_ruled; [ "$B_CODE" = "503" ] \
  && pass "B an undeclared envelope answers 503 - a DEPLOYMENT state, not a credential verdict" \
  || fail "B expected HTTP 503, got $B_CODE: $B_BODY"
b_ruled; printf '%s' "$B_BODY" | grep -q '"reason":"envelope_unset"' \
  && pass "B the refusal names envelope_unset, so A's 201 came from the DECLARATION" \
  || fail "B refusal did not name envelope_unset: $B_BODY"
b_ruled; [ -n "$B_ROWS_BEFORE" ] && [ "$B_ROWS_AFTER" = "$B_ROWS_BEFORE" ] \
  && pass "B a refused enrolment wrote no row" \
  || fail "B row count moved $B_ROWS_BEFORE -> $B_ROWS_AFTER on a refused enrolment"

gate_arm refused_without_declaration "$b_facts" 2 || true

# ---------------------------------------------------------------------------
# D: a refused enrolment must REFUSE STARTUP, not be logged and survived
# ---------------------------------------------------------------------------
echo ""
echo "=== D: a real worker whose enrolment is refused does not start ==="

e2e_start_cdc_relay "$E2E_BIN/zeroship-data-cdc-server" || exit 1
# THE TWO LAUNCHES DIFFER IN `--control-url` AND IN NOTHING ELSE. D1 points at
# the control plane arm B left running with its network declaration cleared, so
# its enrolment comes back `envelope_unset`; D2 points at the stack's declared
# one. Both bind the same port, in turn, so the port is not the variable.
#
# WHY THE PAIR AND NOT JUST D1. A worker that cannot start for any reason at all
# satisfies D1 - a broken binary, a missing peer document, a typo in a flag all
# print exactly what a working fence prints. D2 is what says the refusal came
# from the enrolment verdict.
#
# WHY THE EXIT CODE AND NOT A LOG LINE. A fence that logs its refusal and then
# carries on emits the same line as one that exits; the difference is only
# visible in whether the process is still there and whether anything answers on
# its port. Both are asserted.
#
# Sets WORKER_PID rather than echoing it: a command substitution would run the
# launch in a SUBSHELL, the worker would be that subshell's child rather than
# this one's, and `wait` would then refuse the pid instead of returning the exit
# status the whole arm turns on.
worker_launch() {
  local control="$1" log="$2"
  "$E2E_BIN/zeroship-worker" --port "$WORKER_REFUSAL_PORT" --threads 1 \
    --control-url "$control" --blob-store "$WORK/blobs" --poll-interval 2 \
    > "$log" 2>&1 &
  WORKER_PID=$!
}

worker_launch "$UNDECLARED_URL" "$WORK/worker-refused.log"
D1_PID="$WORKER_PID"
echo "$D1_PID" >> "$PIDFILE"
D1_EXIT="still-running"
for _ in $(seq 1 45); do
  kill -0 "$D1_PID" 2>/dev/null || { wait "$D1_PID"; D1_EXIT="$?"; break; }
  sleep 1
done
D1_LISTENING="$(curl -s -o /dev/null -w '%{http_code}' --max-time 3 \
  "http://localhost:$WORKER_REFUSAL_PORT/readyz")"
note "D1 exit=$D1_EXIT readyz=$D1_LISTENING log tail:"
tail -3 "$WORK/worker-refused.log" 2>/dev/null | sed 's/^/      /'

# D1's verdict is already captured above. Reap it before D2 binds the SAME
# port: a D1 that did not exit still holds the socket, so D2 would fail to bind
# and then report D1's own readiness as its own - which is how the pre-change
# run printed `D2 readyz=200` with no second worker in existence.
if kill -0 "$D1_PID" 2>/dev/null; then
  kill -9 "$D1_PID" 2>/dev/null
  wait "$D1_PID" 2>/dev/null
fi
for _ in $(seq 1 20); do
  curl -s -o /dev/null --max-time 2 "http://localhost:$WORKER_REFUSAL_PORT/readyz" || break
  sleep 1
done

D2_ROWS_BEFORE="$(psql_q "select count(*) from zeroship.worker_instances where advertise_port = $WORKER_REFUSAL_PORT")"
worker_launch "$CONTROL_URL" "$WORK/worker-admitted.log"
D2_PID="$WORKER_PID"
echo "$D2_PID" >> "$PIDFILE"
D2_READY="000"
for _ in $(seq 1 45); do
  D2_READY="$(curl -s -o /dev/null -w '%{http_code}' --max-time 3 \
    "http://localhost:$WORKER_REFUSAL_PORT/readyz")"
  [ "$D2_READY" = "200" ] && break
  kill -0 "$D2_PID" 2>/dev/null || break
  sleep 1
done
D2_ROWS_AFTER="$(psql_q "select count(*) from zeroship.worker_instances where advertise_port = $WORKER_REFUSAL_PORT")"
note "D2 readyz=$D2_READY rows on :$WORKER_REFUSAL_PORT $D2_ROWS_BEFORE -> $D2_ROWS_AFTER"
if [ "$D2_READY" != "200" ]; then
  tail -5 "$WORK/worker-admitted.log" 2>/dev/null | sed 's/^/      /'
fi
kill "$D2_PID" 2>/dev/null || true

d_facts=0
d_ruled() { d_facts=$((d_facts + 1)); }

d_ruled; [ "$D1_EXIT" != "still-running" ] && [ "$D1_EXIT" != "0" ] \
  && pass "D1 the worker EXITED non-zero ($D1_EXIT) when control refused its enrolment" \
  || fail "D1 worker exit was '$D1_EXIT'; a refused enrolment must refuse startup"
d_ruled; [ "$D1_LISTENING" = "000" ] \
  && pass "D1 nothing ever answered on :$WORKER_REFUSAL_PORT, so no traffic could reach it" \
  || fail "D1 something answered /readyz on :$WORKER_REFUSAL_PORT (HTTP $D1_LISTENING)"
d_ruled; grep -qi "enrol" "$WORK/worker-refused.log" 2>/dev/null \
  && pass "D1 the refusal names enrolment, so it is not an unrelated boot failure" \
  || fail "D1 $WORK/worker-refused.log never mentions enrolment"
d_ruled; [ "$D2_READY" = "200" ] \
  && pass "D2 the SAME command against the declared control plane serves, so D1 is about the verdict" \
  || fail "D2 the control worker never became ready (HTTP $D2_READY)"
d_ruled; [ -n "$D2_ROWS_BEFORE" ] && [ -n "$D2_ROWS_AFTER" ] && [ "$D2_ROWS_AFTER" -gt "$D2_ROWS_BEFORE" ] \
  && pass "D2 and it enrolled a row for :$WORKER_REFUSAL_PORT" \
  || fail "D2 row count for :$WORKER_REFUSAL_PORT stayed $D2_ROWS_BEFORE -> $D2_ROWS_AFTER"

# Floor 4 of 5: D1 alone is three of these and is worthless without D2's pair,
# so the floor sits above what either half contributes on its own.
gate_arm refuses_startup_on_refused_enrolment "$d_facts" 4 || true

echo ""
gate_arms_finish || FAIL=$((FAIL + 1))
echo "============================================"
echo " passed: $PASS   failed: $FAIL"
echo "============================================"
[ "$FAIL" -eq 0 ]
