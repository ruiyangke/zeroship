#!/usr/bin/env bash
# One-shot: tell all 3 Dragonfly nodes about the cluster topology.
# Slots 0–5460 → node-0 (:7000), 5461–10922 → node-1 (:7001),
# 10923–16383 → node-2 (:7002).
#
# Dragonfly's cluster_mode=yes ships empty until it receives a
# DFLYCLUSTER CONFIG push. Each node needs the full topology so it
# knows which slots it owns + where to redirect foreign requests.
#
# Uses a local `redis-cli` if available, otherwise falls back to the
# upstream redis image via docker run.
set -euo pipefail

config_json=$(cat <<'JSON'
[
  {"slot_ranges":[{"start":0,"end":5460}],    "master":{"id":"node-0","ip":"127.0.0.1","port":7000},"replicas":[]},
  {"slot_ranges":[{"start":5461,"end":10922}],"master":{"id":"node-1","ip":"127.0.0.1","port":7001},"replicas":[]},
  {"slot_ranges":[{"start":10923,"end":16383}],"master":{"id":"node-2","ip":"127.0.0.1","port":7002},"replicas":[]}
]
JSON
)

if command -v redis-cli >/dev/null 2>&1; then
  RCLI=(redis-cli)
else
  RCLI=(docker run --rm --network host redis:7-alpine redis-cli)
fi

for port in 7000 7001 7002; do
  echo "-- pushing cluster config to :$port --"
  "${RCLI[@]}" -p "$port" DFLYCLUSTER CONFIG "$config_json"
done

echo "-- CLUSTER SLOTS from :7000 --"
"${RCLI[@]}" -p 7000 CLUSTER SLOTS
