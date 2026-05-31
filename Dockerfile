# syntax=docker/dockerfile:1
#
# zeroship platform image — builds all SIX binaries:
#   zeroship-control, zeroship-gate, zeroship-worker, zeroship-auth,
#   zeroship-sandbox, zeroship (CLI).
#
# Two-stage native build with a Node pre-stage:
#   1. `sdks` (node:22) runs `pnpm build` to emit the bootstrap dist files
#      (`sdks/bootstrap/dist/{runtime-entry,dispatcher}.js`). The runtime
#      crate `include_str!`s those at compile time
#      (crates/runtime/src/core/init.rs → "../../../../sdks/bootstrap/dist/…"),
#      so they MUST exist before cargo touches zeroship-runtime.
#   2. `builder` (rust) copies crates/, the freshly-built sdks/, and the
#      policies/ tree (crates/authz/build.rs parses ../../policies/*.cedar
#      at build time) and compiles all six binaries.
#   3. final (ubuntu) ships the six binaries + docker CLI for the sandbox.

# ---------------------------------------------------------------------------
# Stage 1 — build the SDK dist (bootstrap runtime-entry + dispatcher).
# ---------------------------------------------------------------------------
FROM node:22-bookworm AS sdks
WORKDIR /build
RUN corepack enable
# Workspace manifest + lockfile first for a cache-friendly install.
COPY package.json pnpm-lock.yaml pnpm-workspace.yaml ./
# pnpm-workspace.yaml globs sdks/*, apps/*, examples/* and the lockfile
# pins importers for all three. `pnpm install --frozen-lockfile` validates
# the on-disk workspace against the lockfile, so every workspace package
# must be present or the install fails. apps/ + examples/ are tiny
# (no committed node_modules); copying them keeps the workspace consistent.
COPY sdks/ sdks/
COPY apps/ apps/
COPY examples/ examples/
RUN pnpm install --frozen-lockfile
# Root build script only builds sdks/* (topo: @zeroship/db →
# @zeroship/bootstrap → rest); it does not touch apps/ or examples/.
RUN pnpm build
# Sanity: the two files the runtime crate include_str!s MUST exist.
RUN test -f sdks/bootstrap/dist/runtime-entry.js \
 && test -f sdks/bootstrap/dist/dispatcher.js
# Build the CONSOLE (apps/zeroship-builder) to a .zship. The console is now a
# regular gateway-fronted zeroship app; control's `--bootstrap-console` ingests
# this artifact at boot. The root `pnpm build` only builds sdks/*, so build the
# console app explicitly (its `build` = `tsc -b && vite build` → dist/app.zship).
RUN pnpm --filter zeroship-builder build \
 && test -f apps/zeroship-builder/dist/app.zship

# ---------------------------------------------------------------------------
# Stage 2 — native (Rust) build of all six binaries.
# ---------------------------------------------------------------------------
FROM rust:latest AS builder
WORKDIR /build
# System build deps not in rust:latest:
#   clang + libclang-dev  → bindgen (libsqlite3-sys, aws-lc-sys)
#   cmake                 → aws-lc-sys / ring native build
#   perl                  → aws-lc-sys / openssl-style asm generation
#   nasm                  → aws-lc-sys x86_64 assembly
#   pkg-config            → -sys crate probing
# (gcc/g++/make come with rust:latest's buildpack-deps base.)
RUN apt-get update && apt-get install -y --no-install-recommends \
    clang libclang-dev cmake perl nasm pkg-config \
    && rm -rf /var/lib/apt/lists/*
COPY Cargo.toml Cargo.lock ./
COPY crates/ crates/
# Cedar authz policies — crates/authz/build.rs parses these at build time.
COPY policies/ policies/
# sdks/ WITH the dist emitted by the node stage, so the runtime crate's
# include_str!("../../../../sdks/bootstrap/dist/{runtime-entry,dispatcher}.js")
# resolves against /build/sdks/bootstrap/dist/.
COPY --from=sdks /build/sdks/ sdks/
RUN cargo build --release \
    -p zeroship-control \
    -p zeroship-gateway \
    -p zeroship-worker \
    -p zeroship-auth \
    -p zeroship-sandbox \
    -p zeroship

# ---------------------------------------------------------------------------
# Stage 3 — runtime image.
# ---------------------------------------------------------------------------
# Use Ubuntu 24.04 (glibc 2.39) instead of Debian bookworm (glibc 2.36)
FROM ubuntu:24.04 AS runtime
# `docker.io` gives us the docker CLI inside the sandbox container so
# zeroship-sandbox can shell out via docker.sock (Docker-out-of-Docker).
RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates curl docker.io && rm -rf /var/lib/apt/lists/*
COPY --from=builder /build/target/release/zeroship-control /usr/local/bin/
COPY --from=builder /build/target/release/zeroship-gate /usr/local/bin/
COPY --from=builder /build/target/release/zeroship-worker /usr/local/bin/
COPY --from=builder /build/target/release/zeroship-auth /usr/local/bin/
COPY --from=builder /build/target/release/zeroship-sandbox /usr/local/bin/
COPY --from=builder /build/target/release/zeroship /usr/local/bin/
# The prebuilt console .zship (built in the `sdks` node stage). Control's
# `--bootstrap-console --console-zship /opt/zeroship/console/app.zship` ingests
# it at boot to seed the gateway-fronted console app.
COPY --from=sdks /build/apps/zeroship-builder/dist/app.zship /opt/zeroship/console/app.zship

# ---------------------------------------------------------------------------
# Stage 4 — frontend dev image: the runtime image (so it has the `zeroship`
# CLI built for THIS glibc) PLUS Node 22. The AI builder's `vite dev` runs the
# vite-plugin dev-bootstrap, which SPAWNS `zeroship` to host the app's server
# functions in the real runtime — so the builder container needs both Node
# (for vite) and the zeroship binary on PATH (built for ubuntu 24.04's glibc;
# a plain node:22-bookworm image is glibc 2.36 and can't run it).
# ---------------------------------------------------------------------------
FROM runtime AS frontend
RUN curl -fsSL https://deb.nodesource.com/setup_22.x | bash - \
 && apt-get update && apt-get install -y --no-install-recommends nodejs git \
 && rm -rf /var/lib/apt/lists/* \
 && corepack enable
