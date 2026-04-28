# zeroship-sandbox-base — Docker image for editor sessions.
#
# One container is spawned per active editor session. The container
# stays alive (sleep infinity) and the agent shells in via `docker
# exec` to run npm install, vite build, git commit, etc. The
# `/workspace` mount is bind-mounted from the host so files survive
# container restarts and can be inspected directly by the sandbox
# service.

FROM node:22-alpine

# Tools the agent reaches for through `run_command`.
RUN apk add --no-cache \
      git \
      curl \
      bash \
      tar \
      gzip \
    && npm config set fund false \
    && npm config set update-notifier false

# Sensible git defaults so `git commit` doesn't error on first use.
RUN git config --system user.email "agent@zeroship.local" \
 && git config --system user.name  "zeroship agent" \
 && git config --system init.defaultBranch main \
 && git config --system advice.detachedHead false

# Pre-seeded starter template. When the entrypoint sees an empty
# /workspace it copies this in so the very first edit operates on a
# real, runnable Vite + React + Tailwind project.
COPY templates/default /opt/templates/default
RUN chmod -R a+rwX /opt/templates/default

# Entrypoint script — copy template if /workspace is empty, init git,
# then sleep until docker stop.
COPY entrypoint.sh /usr/local/bin/sandbox-entrypoint.sh
RUN chmod +x /usr/local/bin/sandbox-entrypoint.sh

WORKDIR /workspace

# Agent will reach the dev server on :5173 once it runs `npm run dev`.
EXPOSE 5173

ENTRYPOINT ["/usr/local/bin/sandbox-entrypoint.sh"]
CMD ["sleep", "infinity"]
