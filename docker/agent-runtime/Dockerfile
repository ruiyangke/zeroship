# syntax=docker/dockerfile:1.7
#
# Agent runtime image — what runs inside the libkrun / Cloud-Hypervisor
# microVM as PID 1. Targets the AI-driven Vite/Node dev-env workload:
# the user's project gets a Node 22 + pnpm + git toolchain, and the
# sandbox-agent binary serves /exec, /files, /tree, /metrics on :7777.
#
# Layers, bottom (rebuilt rarely) to top (rebuilt per agent change):
#
#   1. Debian trixie-slim — base userland with ca-certs + git
#   2. Node.js + pnpm — JS toolchain prewarmed
#   3. sandbox-agent binary — only layer that rebuilds per agent commit
#
# ~330 MiB image (rootfs ~320 MiB on disk). Deliberately omitted:
#
#   - Python (use a per-workload base if a Python AI tool runs in-VM —
#     the AI usually runs server-side and the in-VM agent only runs
#     the user's resulting code).
#   - npm + corepack (pnpm is the chosen package manager).
#   - Node.js headers /usr/local/include (only needed for node-gyp /
#     native module compilation; workloads can `apt-get install` them
#     on demand inside their session).
#   - dumb-init (the agent's `reap` module is PID-1 correct on its own;
#     see crates/sandbox-agent/src/reap.rs).
#   - Globally-installed vite (each project pins its own via package.json).
#   - apt cache, /usr/share/{doc,man,info}, locales other than en/en_US,
#     timezones other than UTC.
#
# The agent binary is stripped (~3 MiB).

# Trixie (Debian 13) carries glibc 2.41+, which the Nix-built agent
# binary (linked against glibc 2.42) needs. Bookworm's 2.36 is too
# old.
FROM debian:trixie-slim AS base

ENV DEBIAN_FRONTEND=noninteractive \
    NODE_VERSION=22.11.0 \
    PATH=/usr/local/bin:/usr/local/sbin:/usr/bin:/usr/sbin:/bin:/sbin \
    LANG=en_US.UTF-8 \
    LC_ALL=C.UTF-8

# 1: minimum runtime userland.
#   - ca-certificates: HTTPS roots (HSTS, npm registry, github).
#   - git: AI workflow inside the VM does git commits.
#   - curl, xz-utils: BUILD-TIME ONLY (download + extract Node).
#     Removed via apt-get purge in the same layer below to avoid
#     leaving them in the final image.
RUN apt-get update \
 && apt-get install -y --no-install-recommends \
        ca-certificates git curl xz-utils \
 && rm -rf /var/lib/apt/lists/*

# 2: Node 22 + pnpm. Keep the binary, drop everything else from the
#    Node tarball that we don't run at request time. npm itself is
#    only needed long enough to install pnpm globally; deleted after.
RUN curl -fsSL "https://nodejs.org/dist/v${NODE_VERSION}/node-v${NODE_VERSION}-linux-x64.tar.xz" \
      | tar -xJ -C /usr/local --strip-components=1 \
 && /usr/local/bin/npm i -g --silent pnpm@9 \
 && rm -rf \
        /usr/local/include \
        /usr/local/share/doc /usr/local/share/man /usr/local/share/systemtap \
        /usr/local/lib/node_modules/npm/docs \
        /usr/local/lib/node_modules/npm/man \
        /usr/local/lib/node_modules/npm \
        /usr/local/lib/node_modules/corepack \
        /usr/local/CHANGELOG.md /usr/local/README.md /usr/local/LICENSE \
        /root/.npm

# 3: aggressive base-image cleanup.
#    - Remove curl + xz-utils (build-time only — keep agent footprint
#      small; if a workload needs them, it `apt-get install`s them).
#    - Strip /usr/share/{doc,man,info,locale,zoneinfo}; keep en_US +
#      UTC for sane defaults.
#    - Drop apt cache, log files, dpkg metadata that's not load-bearing.
RUN apt-get -y purge curl xz-utils \
 && apt-get -y autoremove --purge \
 && apt-get clean \
 && rm -rf \
        /var/cache/apt/* \
        /var/lib/apt/lists/* \
        /var/log/* \
        /usr/share/doc/* \
        /usr/share/man/* \
        /usr/share/info/* \
 && find /usr/share/locale -mindepth 1 -maxdepth 1 \
        -not -name 'en' -not -name 'en_US' \
        -exec rm -rf {} + \
 && find /usr/share/zoneinfo -mindepth 1 -maxdepth 1 \
        -not -name UTC -not -name posix -not -name right \
        -exec rm -rf {} + 2>/dev/null || true

# 4: the agent binary. Built outside this container, copied in. We
#    `strip` it inside the build layer so the layer is small without
#    needing strip in the runtime image.
COPY sandbox-agent /usr/local/bin/sandbox-agent
RUN chmod +x /usr/local/bin/sandbox-agent \
 && (command -v strip >/dev/null && strip /usr/local/bin/sandbox-agent || true)

WORKDIR /workspace
EXPOSE 7777

# No dumb-init wrapper: the agent's `reap` module handles SIGCHLD as
# PID 1 itself (see crates/sandbox-agent/src/reap.rs).
ENTRYPOINT ["/usr/local/bin/sandbox-agent"]
