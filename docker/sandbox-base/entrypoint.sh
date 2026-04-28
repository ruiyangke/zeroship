#!/bin/sh
# Sandbox entrypoint: bootstrap an empty workspace then hand control
# to the requested command (default: `sleep infinity`, which keeps
# the container alive for `docker exec` calls from the agent).
set -eu

if [ ! -e /workspace/package.json ]; then
  echo "[sandbox-entrypoint] empty workspace — seeding template" >&2
  cp -a /opt/templates/default/. /workspace/
  cd /workspace
  if [ ! -d .git ]; then
    git init -q
    git add -A
    git -c user.email=agent@zeroship.local -c user.name="zeroship agent" \
      commit -q -m "initial commit (zeroship template)" || true
  fi
fi

cd /workspace
exec "$@"
