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
# WHAT IS ASSERTED, and the one-variable control that makes it mean something:
#
#   A  the stack's control plane, with the envelope tests/lib/e2e_stack.sh
#      declares, ADMITS an enrolment from loopback claiming the port the worker
#      is really listening on, and the row carries the DERIVED address.
#   B  a SECOND control plane, same binary, same database, same peer document,
#      same request - with the network half of the envelope cleared and NOTHING
#      ELSE CHANGED - refuses it with `envelope_unset` and writes no row.
#
# Without B, A cannot tell a working declaration from an endpoint that admits
# everything. The one variable is `control.worker_enrolment_networks`; the port
# half is left declared on purpose, so B's refusal names the deployment state
# rather than a second missing setting.
#
# THE PORT IS NOT A LITERAL ANYWHERE IN THIS PAIR. The declaration derives it
# from $ZEROSHIP_WORKER_PORT, the enrolment claims the same variable, and A
# additionally proves something is really listening there - so a declaration
# that drifted off the worker's port fails here instead of failing the first
# time a worker boots.
#
# NOT IN SCOPE, deliberately: the worker's own boot path. Nothing in the worker
# calls this endpoint yet. This harness drives it as the worker will.
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
                 PG_PORT CONTROL_UNDECLARED_PORT || exit 1
RUN_TOKEN="$$_$(date +%s%N)"
export PG_CONTAINER="zs-e2e-enrol-pg-$RUN_TOKEN"

# shellcheck source=tests/lib/e2e_stack.sh
source "$ROOT/tests/lib/e2e_stack.sh"

cleanup() {
  echo ""
  echo "=== Cleanup ==="
  stack_down
  zs_ports_release
}
trap cleanup EXIT

echo "=== Stack ==="
stack_up || exit 2

CONTROL_URL="http://localhost:$ZEROSHIP_CONTROL_PORT"
UNDECLARED_URL="http://localhost:$CONTROL_UNDECLARED_PORT"
psql_q() { docker exec -i "$PG_CONTAINER" psql -U postgres -d zeroship -tAc "$1"; }

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

# `svc/worker` is the role that holds CONTROL_WORKER_ENROL, and control is
# addressed as `svc/control`. A fresh assertion per call: `jti` is single use.
enrol_post() {
  local url="$1" out="$2" assertion
  assertion="$(e2e_mint_service_assertion "$ZEROSHIP_WORKER_SERVICE_KEY_FILE" \
                 svc/worker svc/control)" || return 1
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
ruled; printf '%s' "$A_ID" | grep -Eq '^wkr_[0-9A-Za-z]{22}$' \
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

echo ""
gate_arms_finish || FAIL=$((FAIL + 1))
echo "============================================"
echo " passed: $PASS   failed: $FAIL"
echo "============================================"
[ "$FAIL" -eq 0 ]
