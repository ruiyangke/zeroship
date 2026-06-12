#!/usr/bin/env bash
# ============================================================================
# e2e_uri.sh — complex-URI forwarding through the real gateway.
#
# Deploys `examples/uri-echo` (a public `{fetch}` app that reflects the exact
# URL its V8 sees) and hits it through control→gateway→worker with a battery of
# tricky paths + query strings: percent-encoding, repeated keys, `+`-as-space,
# reserved chars, unicode, empty values, encoded path segments, etc.
#
# Oracle: for each raw request, the WHATWG `new URL("http://<host>"+raw)` parse
# is the reference. The worker MUST report the same pathname/search/params — any
# divergence is a gateway/runtime URI-forwarding bug (this is the surface the
# ISS-70 query-string fix lives on).
#
# Bring-up is the shared library (tests/lib/e2e_stack.sh); needs a release build
# + a built examples/uri-echo/dist/app.zship (pnpm --filter ./examples/uri-echo build).
#
#   ./tests/e2e_uri.sh
# ============================================================================
set -uo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"

# Port offset so this can run alongside the other e2e harnesses.
export CONTROL_PORT=9150
export WORKER_PORT=8118
export GATE_PORT=8032
export PG_PORT=5446
export PG_CONTAINER="zs-e2e-uri-pg"

PASS=0; FAIL=0
pass() { PASS=$((PASS+1)); echo "  ✓ $1"; }
fail() { FAIL=$((FAIL+1)); printf "  ✗ %b\n" "$1"; }

source "$ROOT/tests/lib/e2e_stack.sh"

cleanup() { stack_down 2>/dev/null || true; }
trap cleanup EXIT

echo "============================================"
echo "  zeroship E2E — complex URI forwarding"
echo "============================================"

stack_up        || { echo "stack bring-up failed"; exit 1; }
mint_admin_pat  || exit 1

ZSHIP="$ROOT/examples/uri-echo/dist/app.zship"
[ -f "$ZSHIP" ] || { echo "missing $ZSHIP — run: pnpm --filter ./examples/uri-echo build"; exit 2; }

if ! deploy_zship "uri-echo-ux" "$ZSHIP" >/dev/null; then
  fail "deploy uri-echo"; exit 1
fi
pass "deployed uri-echo"
HOST="uri-echo-ux.localhost"

# Warm up: the worker loads the bundle on first hit, so the first few requests
# can race the cold start. Poll until the echo handler answers with its JSON.
warm=0
for i in $(seq 1 40); do
  if curl -s --path-as-is -H "Host: $HOST" "http://localhost:$GATE_PORT/__warmup" \
       | node -e 'let s="";process.stdin.on("data",d=>s+=d).on("end",()=>process.exit(s.includes("pathname")?0:1))'; then
    warm=1; break
  fi
  sleep 0.25
done
[ "$warm" = "1" ] && pass "app warm (echo handler answering)" || fail "app never warmed"

# check <desc> <raw-path-and-query>
# Computes the WHATWG reference parse, fetches the worker echo, compares
# pathname + search + params (ordered key/value pairs).
check() {
  local desc="$1" raw="$2"
  local oracle got body
  oracle="$(node -e '
    const u = new URL("http://'"$HOST"'" + process.argv[1]);
    process.stdout.write(JSON.stringify({pathname:u.pathname, search:u.search, params:[...u.searchParams.entries()]}));
  ' "$raw")"
  body="$(curl -s --path-as-is -H "Host: $HOST" "http://localhost:$GATE_PORT$raw")"
  got="$(printf '%s' "$body" | node -e '
    let s=""; process.stdin.on("data",d=>s+=d).on("end",()=>{
      try { const o=JSON.parse(s); process.stdout.write(JSON.stringify({pathname:o.pathname, search:o.search, params:o.params})); }
      catch(e){ process.stdout.write("PARSE_ERR:"+s.slice(0,120)); }
    });
  ')"
  if [ "$got" = "$oracle" ]; then
    pass "$desc"
  else
    fail "$desc\n      raw:    $raw\n      oracle: $oracle\n      worker: $got\n      body:   $(printf '%s' "$body" | head -c 160)"
  fi
}

# check_canon <desc> <raw> <expected-pathname>
# For paths the gateway DELIBERATELY canonicalizes (SEC-2): the gateway forwards
# the same normal form it matched auth against, so the worker's pathname can't
# disagree with the auth match (no path-trick bypass). Empty (`//`) + trailing-
# slash segments are dropped and `.`/`..` resolved. The query is still preserved.
check_canon() {
  local desc="$1" raw="$2" want="$3"
  local body got
  body="$(curl -s --path-as-is -H "Host: $HOST" "http://localhost:$GATE_PORT$raw")"
  got="$(printf '%s' "$body" | node -e '
    let s=""; process.stdin.on("data",d=>s+=d).on("end",()=>{
      try { process.stdout.write(JSON.parse(s).pathname || ""); } catch(e){ process.stdout.write("PARSE_ERR"); }
    });
  ')"
  if [ "$got" = "$want" ]; then
    pass "$desc"
  else
    fail "$desc\n      raw:      $raw\n      expected: $want (SEC-2 canonical)\n      worker:   $got"
  fi
}

# check_reject <desc> <raw>
# SEC-2 anti-traversal: a path with `.`/`..` (literal OR `%2e`-encoded) or an
# internal empty segment (`//`) is REJECTED at the gateway with 400 — never
# guessed/forwarded — so the worker's auth match can't be bypassed with a path
# a browser would silently rewrite.
check_reject() {
  local desc="$1" raw="$2"
  local code
  code="$(curl -s -o /dev/null -w '%{http_code}' --path-as-is -H "Host: $HOST" "http://localhost:$GATE_PORT$raw")"
  if [ "$code" = "400" ]; then
    pass "$desc → 400 rejected"
  else
    fail "$desc — expected 400 (traversal/empty-segment reject), got $code (raw=$raw)"
  fi
}

echo
echo "=== Query strings ==="
check "simple query"                 "/echo?q=hello"
check "multiple params (order)"      "/p?a=1&b=2&c=3"
check "repeated key (q×3)"           "/p?q=x&q=y&q=z"
check "percent-encoded space"        "/p?q=hello%20world"
check "plus-as-space"                "/p?q=hello+world"
check "encoded & and = in value"     "/p?q=a%26b%3Dc"
check "unicode value (café)"         "/p?q=caf%C3%A9"
check "empty value"                  "/p?q="
check "bare key no value"            "/p?flag"
check "encoded = in value"           "/p?eq=a%3Db"
check "encoded URL in value"         "/p?redirect=https%3A%2F%2Fex.com%2Fa%3Fx%3D1%26y%3D2"
check "semicolon not a separator"    "/p?a=1;b=2"
check "mixed encoded + plain"        "/search?q=red%20socks&page=2&sort=price"
check "ampersand-heavy"              "/p?a=1&b=&c=3&d="

echo
echo "=== Path encoding (preservation) ==="
check "encoded slash in segment"     "/files/a%2Fb"
check "encoded space in path"        "/my%20file"
check "unicode in path (café)"       "/caf%C3%A9"
check "encoded reserved in path"     "/a%3Ab%40c"
check "deep path + query"            "/a/b/c/d/e?x=1&y=2"
check "encoded percent literal"      "/100%25done"
check "path that looks like query"   "/a%3Fb"

echo
echo "=== SEC-2: trailing slash canonicalized (auth-match == worker view) ==="
check_canon "trailing slash dropped"        "/dir/?q=1"        "/dir"

echo
echo "=== SEC-2: traversal / empty-segment paths REJECTED at the edge (400) ==="
check_reject "double slash (//)"            "/a//b"
check_reject "single-dot segment (.)"       "/a/./b"
check_reject "double-dot segment (..)"      "/a/x/../b"
check_reject "traversal toward root"        "/../../../etc/passwd"
check_reject "encoded dot-dot (%2e%2e)"     "/a/%2e%2e/b"
check_reject "trailing dot-segment"         "/a/b/."

echo
echo "=== Roots + edges ==="
check "root with query"              "/?hello=world"
check "query only special chars"     "/p?q=%21%40%23%24%25%5E%26%2A%28%29"
check "long query"                   "/p?data=$(printf 'x%.0s' {1..500})"

echo
echo "============================================"
echo "  RESULTS: $PASS passed, $FAIL failed"
echo "============================================"
[ "$FAIL" -eq 0 ]
