#!/usr/bin/env bash
# Scenario 22, DEV half: the creator edits a server file while requests are in
# flight and vite reloads. Deployed half measured zero transport failures and
# zero 5xx (a48ec788f). This asks whether dev answers the SAME.
set -uo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
APP="$ROOT/examples/starter"
W="$(mktemp -d)"
LOG="$W/dev22-vite.log"
SRC="$APP/src/server.ts"
BAK="$W/server.ts.bak"

cp "$SRC" "$BAK"
restore() {
  cp "$BAK" "$SRC"
  [ -n "${VPID:-}" ] && kill -- -"$VPID" 2>/dev/null
  pkill -f "examples/starter" 2>/dev/null
  true
}
trap restore EXIT

cd "$APP"
setsid pnpm dev > "$LOG" 2>&1 &
VPID=$!
PORT=""
for _ in $(seq 1 60); do
  # Strip ANSI first: vite prints `localhost:<ESC>[1m5173`, so a naive
  # `localhost:[0-9]+` matches nothing (the #211 class). And the RPC endpoint
  # is the zeroship API server's port, not vite's.
  PORT=$(sed 's/\x1b\[[0-9;]*m//g' "$LOG" | grep -oE "API server starting on :[0-9]+" | head -1 | grep -oE "[0-9]+$")
  [ -n "$PORT" ] && break
  sleep 1
done
[ -n "$PORT" ] || { echo "BLOCKER: dev server never printed a port"; tail -20 "$LOG"; exit 2; }
echo "dev server port $PORT"

URL="http://localhost:$PORT/__zeroship/v1/getMessages"
for _ in $(seq 1 40); do
  C=$(curl -sS -m 5 -o /dev/null -w '%{http_code}' -X POST -H 'content-type: application/json' \
      "$URL" -d '{"json":{}}' 2>/dev/null)
  [ "$C" = "200" ] && break
  sleep 1
done
[ "$C" = "200" ] || { echo "BLOCKER: dev RPC never answered 200 (last=$C)"; tail -20 "$LOG"; exit 2; }
echo "dev RPC answering 200"

TRAF="$W/dev22.tsv"; : > "$TRAF"
STOP="$W/dev22.stop"; rm -f "$STOP"
(
  while [ ! -f "$STOP" ]; do
    _b=$(curl -sS -m 15 -w '\n%{http_code}' -X POST \
      -H 'content-type: application/json' "$URL" -d '{"json":{}}' 2>/dev/null)
    _rc=$?
    _c=$(printf '%s' "$_b" | tail -1)
    # WHICH build answered. Without this the run cannot tell "no failures
    # because the reload was clean" from "no failures because the reload never
    # happened" -- the same vacuity that made the deployed guard decoration.
    if printf '%s' "$_b" | grep -qF "EDITED-BY-SCENARIO22"; then _v=new; else _v=old; fi
    printf '%s\t%s\t%s\n' "${_c:-000}" "$_rc" "$_v" >> "$TRAF"
  done
) & TPID=$!

sleep 3
# THE EDIT: the creator changes a server file. This is the dev counterpart of
# the deployed redeploy.
perl -pi -e 's|Build locally with an AI coding agent.|EDITED-BY-SCENARIO22|' "$SRC"
sleep 12
touch "$STOP"; wait "$TPID" 2>/dev/null

TOT=$(wc -l < "$TRAF" | tr -d ' ')
BAD=$(awk -F'\t' '$2!=0 || $1 ~ /^5/' "$TRAF" | wc -l | tr -d ' ')
echo "dev traffic: $TOT requests"
echo "dev codes:   $(awk -F'\t' '{print $1}' "$TRAF" | sort | uniq -c | tr '\n' ' ')"
echo "dev bad:     $BAD (transport failure or 5xx)"
OLD=$(awk -F'\t' '$3=="old"' "$TRAF" | wc -l | tr -d ' ')
NEW=$(awk -F'\t' '$3=="new"' "$TRAF" | wc -l | tr -d ' ')
echo "dev span:    old=$OLD new=$NEW  (both must be >0 or the reload never landed)"
echo "reload seen in vite log: $(grep -c "hmr\|reload\|restart" "$LOG")"

# VERDICT. Two conditions, and the span one is not decoration: the first
# version of this experiment edited a console.log, which scenario 20 already
# proved dev DISCARDS, so the edit was invisible in the response and a clean
# result was equally consistent with a reload that never happened. Changing an
# observable response field is what makes old/new countable.
RC=0
if [ "${BAD:-1}" -ne 0 ]; then
  echo "FAIL: $BAD request(s) hit a transport failure or 5xx across the dev reload"; RC=1
fi
if [ "${OLD:-0}" -eq 0 ] || [ "${NEW:-0}" -eq 0 ]; then
  echo "FAIL: traffic did not span the reload (old=$OLD new=$NEW), so the clean result above is one-sided"; RC=1
fi
[ "$RC" -eq 0 ] && echo "OK: dev reload under load - no in-flight request cut, and the traffic spanned it"
exit "$RC"
