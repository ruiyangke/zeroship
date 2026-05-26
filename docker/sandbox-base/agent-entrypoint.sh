#!/bin/sh
# Seed an empty workspace with the default Builder project, then start
# the signed sandbox agent that the controller talks to.
set -eu

if [ ! -e /workspace/package.json ]; then
  echo "[sandbox-agent-entrypoint] empty workspace - seeding template" >&2
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
exec /usr/local/bin/zeroship-sandbox-agent "$@"
