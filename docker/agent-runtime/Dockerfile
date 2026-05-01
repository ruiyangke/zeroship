# syntax=docker/dockerfile:1.7
#
# Agent runtime image — what runs inside the libkrun microVM.
#
# Layers, bottom (rebuilt rarely) to top (rebuilt per agent change):
#
#   1. Debian bookworm-slim — base userland (~80 MiB)
#   2. apt + dev tools — git, python, build-essential needed by deepagents
#   3. Node.js + pnpm + vite — JS toolchain prewarmed
#   4. Python deps — common libs for deepagents tools
#   5. sandbox-agent binary — the only layer that rebuilds per agent commit
#
# This split means an agent-only release pushes a ~6 MiB layer, not the
# full 700 MiB image, to whatever registry the cluster pulls from.

# Trixie (Debian 13) carries glibc 2.41+, which the Nix-built agent
# binary (linked against glibc 2.42) needs. Bookworm's 2.36 is too
# old. A future static-musl build would let us drop back to slim
# distros (Alpine, distroless) without this pin.
FROM debian:trixie-slim

ENV DEBIAN_FRONTEND=noninteractive \
    NODE_VERSION=22.11.0 \
    PATH=/usr/local/bin:/usr/local/sbin:/usr/bin:/usr/sbin:/bin:/sbin

# 1+2: base userland.
RUN apt-get update && apt-get install -y --no-install-recommends \
        ca-certificates curl git python3 python3-pip xz-utils \
        coreutils procps dumb-init \
    && rm -rf /var/lib/apt/lists/*

# 3: Node.js (official tarball; debian's repos lag).
RUN curl -fsSL "https://nodejs.org/dist/v${NODE_VERSION}/node-v${NODE_VERSION}-linux-x64.tar.xz" \
      | tar -xJ -C /usr/local --strip-components=1 \
    && npm i -g pnpm@9 vite@5

# 4: Python deps frequently used by deepagents tools.
RUN pip3 install --break-system-packages --no-cache-dir \
        requests httpx pydantic pydantic-ai

# 5: the agent binary. Last layer so most rebuilds touch only this.
# Built outside the container by `cargo build --release --bin sandbox-agent`
# and copied in here.
COPY sandbox-agent /usr/local/bin/sandbox-agent
RUN chmod +x /usr/local/bin/sandbox-agent

# Workspace path — matches the controller's expectation. PVC will
# typically mount here in production; in dev / tests it's just a dir.
WORKDIR /workspace

# Default exposed port for the agent's HTTP API.
EXPOSE 7777

# `dumb-init` is a fallback PID-1 reaper if the agent ever crashes —
# the agent itself reaps via reap.rs but dumb-init keeps the VM
# from accumulating zombies during graceful drain.
ENTRYPOINT ["/usr/bin/dumb-init", "--", "/usr/local/bin/sandbox-agent"]
