#!/usr/bin/env bash
set -euo pipefail

URL="${ZEROSHIP_URL:-http://localhost:3011}"
RPC="${URL}/__zeroship/v1"
FAILED=0

rpc() {
  local proc="$1"; shift
  local body="${1-}"
  [ -z "$body" ] && body="{}"
  curl --fail-with-body -sS -X POST -H 'content-type: application/json' \
    "${RPC}/${proc}" -d "{\"json\":${body}}"
}

check() {
  local name="$1"; shift
  if "$@"; then
    echo "  [ok] $name"
  else
    echo "  [fail] $name"
    FAILED=$((FAILED + 1))
  fi
}

echo "[setup]"
rpc kv.clear > /dev/null

echo "[check 1] visit counter"
VISIT=$(rpc kv.visit)
check "visit returns snapshot with one visit" \
  bash -c "echo '$VISIT' | grep -q '\"visits\":1'"

echo "[check 2] feature flag"
FLAG=$(rpc kv.flag.set '{"enabled":true}')
check "flag writes boolean value" \
  bash -c "echo '$FLAG' | grep -q '\"checkoutEnabled\":true'"

echo "[check 3] fixed-window rate limit"
ACTOR="smoke-$(date +%s%N)"
for _ in 1 2 3 4 5; do
  rpc kv.rate.hit "{\"actor\":\"${ACTOR}\"}" > /dev/null
done
RATE=$(rpc kv.rate.hit "{\"actor\":\"${ACTOR}\"}")
check "sixth hit is blocked" \
  bash -c "echo '$RATE' | grep -q '\"allowed\":false'"
check "rate response carries reset TTL" \
  bash -c "echo '$RATE' | grep -q '\"resetMs\":'"

echo "[check 4] cache miss then hit"
SKU="sku-$(date +%s%N)"
MISS=$(rpc kv.cache.quote "{\"sku\":\"${SKU}\"}")
HIT=$(rpc kv.cache.quote "{\"sku\":\"${SKU}\"}")
check "first quote is a miss" \
  bash -c "echo '$MISS' | grep -q '\"source\":\"miss\"'"
check "second quote is a hit" \
  bash -c "echo '$HIT' | grep -q '\"source\":\"hit\"'"

echo "[check 5] getOrSet memo"
MEMO_LABEL="memo-$(date +%s%N)"
MEMO_MISS=$(rpc kv.memo.get "{\"label\":\"${MEMO_LABEL}\"}")
MEMO_HIT=$(rpc kv.memo.get "{\"label\":\"${MEMO_LABEL}\"}")
check "first memo is computed" \
  bash -c "echo '$MEMO_MISS' | grep -q '\"source\":\"miss\"'"
check "second memo is cached" \
  bash -c "echo '$MEMO_HIT' | grep -q '\"source\":\"hit\"'"
check "memo response carries cached nonce" \
  bash -c "echo '$MEMO_HIT' | grep -q '\"nonce\":'"

echo "[check 6] setIfAbsent lease"
LEASE_A=$(rpc kv.lease.acquire '{"owner":"owner-a"}')
LEASE_B=$(rpc kv.lease.acquire '{"owner":"owner-b"}')
check "first owner acquires lease" \
  bash -c "echo '$LEASE_A' | grep -q '\"acquired\":true'"
check "second owner is rejected while lease is held" \
  bash -c "echo '$LEASE_B' | grep -q '\"acquired\":false'"
rpc kv.lease.clear > /dev/null
LEASE_C=$(rpc kv.lease.acquire '{"owner":"owner-b"}')
check "lease can be acquired after reset" \
  bash -c "echo '$LEASE_C' | grep -q '\"acquired\":true'"

echo "[check 7] getString, has, expire, ttl, persist, delete"
TEXT=$(rpc kv.string.set '{"value":"hello smoke","ttlMs":60000}')
check "string set returns getString value" \
  bash -c "echo '$TEXT' | grep -q '\"value\":\"hello smoke\"'"
check "has reports true after set" \
  bash -c "echo '$TEXT' | grep -q '\"has\":true'"
check "ttl is present after set" \
  bash -c "echo '$TEXT' | grep -q '\"ttlMs\":'"
EXPIRE=$(rpc kv.string.expire '{"ttlMs":120000}')
check "expire updates existing key" \
  bash -c "echo '$EXPIRE' | grep -q '\"updated\":true'"
PERSIST=$(rpc kv.string.persist)
check "persist removes ttl" \
  bash -c "echo '$PERSIST' | grep -q '\"updated\":true' && echo '$PERSIST' | grep -q '\"ttlMs\":null'"
DELETE_TEXT=$(rpc kv.string.delete)
check "delete removes string key" \
  bash -c "echo '$DELETE_TEXT' | grep -q '\"deleted\":true' && echo '$DELETE_TEXT' | grep -q '\"has\":false'"

echo "[check 8] sessions and key listing"
SESSION=$(rpc kv.session.create '{"name":"Smoke User"}')
TOKEN=$(echo "$SESSION" | grep -oE '"token":"[^"]+"' | head -1 | sed 's/^"token":"//;s/"$//')
check "session returns token" bash -c "[ -n '$TOKEN' ]"
KEYS=$(rpc kv.keys.list '{"prefix":"session:","limit":20}')
check "session list includes token" \
  bash -c "echo '$KEYS' | grep -q '$TOKEN'"
DELETE=$(rpc kv.session.delete "{\"token\":\"${TOKEN}\"}")
check "session delete reports deleted" \
  bash -c "echo '$DELETE' | grep -q '\"deleted\":true'"

echo
if [ "$FAILED" -eq 0 ]; then
  echo "smoke: all checks passed"
  exit 0
else
  echo "smoke: $FAILED check(s) failed"
  exit 1
fi
