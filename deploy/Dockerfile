# syntax=docker/dockerfile:1
#
# zeroship platform image — builds all service binaries:
#   zeroship-control, zeroship-gate, zeroship-worker, zeroship-auth,
#   zeroship (CLI), and zeroship-platform-migrate (the platform DB migration
#   one-shot — the compose `migrate` service runs it under the Platform trust
#   profile). The migration path now runs on the published `zero-migrate` engine
#   via the `zeroship-migrate-adapter` crate: it authors `db/migrations-ts/*.ts`
#   in-process with zeroship-runtime's own V8 (no sandboxed recorder-child
#   subprocess) and applies over the native compio-postgres `SqlSession` seam.
#
# NOTE: the sandbox/preview backend (zeroship-sandbox + agent + nomad-driver-ch)
# now lives in the standalone `zeroship-sandbox` project and is built/shipped
# there. The control plane talks to it over HTTP (SANDBOX_URL / SANDBOX_TOKEN).
#
# Two-stage native build with a Node pre-stage:
#   1. `sdks` (node:22) runs `pnpm build` to emit the bootstrap dist files
#      (`sdks/bootstrap/dist/{runtime-entry,dispatcher}.js`). The runtime
#      crate `include_str!`s those at compile time
#      (crates/runtime/src/core/init.rs → "../../../../sdks/bootstrap/dist/…"),
#      so they MUST exist before cargo touches zeroship-runtime.
#   2. `builder` (rust) copies crates/, the freshly-built sdks/, and the
#      deploy/policies/ tree (crates/authz/build.rs parses
#      ../../deploy/policies/*.cedar at build time) and compiles all seven
#      binaries.
#   3. final (ubuntu) ships the six binaries. It ships NO console - see the
#      apps/ note in stage 1.

# ---------------------------------------------------------------------------
# Stage 1 — build the SDK dist (bootstrap runtime-entry + dispatcher).
# ---------------------------------------------------------------------------
FROM node:22-bookworm AS sdks
WORKDIR /build
RUN corepack enable
# Workspace manifest + lockfile first for a cache-friendly install.
COPY package.json pnpm-lock.yaml pnpm-workspace.yaml ./
# pnpm-workspace.yaml globs sdks/* and examples/*, and the lockfile pins
# importers for both. `pnpm install --frozen-lockfile` validates the on-disk
# workspace against the lockfile, so every workspace package must be present
# or the install fails. examples/ is tiny (no committed node_modules).
#
# apps/ USED to be copied here. apps/zeroship-builder - the creator console -
# was extracted out of this repo by 8dbe4a8d4 and the COPY was left behind, so
# this image could not build AT ALL until 2026-08-11 (task #318): the build
# died with `"/apps": not found`. The console is NOT part of this image any
# more and control no longer gets --bootstrap-console. Restoring it means
# fetching the extracted builder repo, not re-adding this COPY.
COPY sdks/ sdks/
COPY examples/ examples/
RUN pnpm install --frozen-lockfile
# Root build script only builds sdks/* (topo: @zeroship/db →
# @zeroship/bootstrap → rest); it does not touch apps/ or examples/.
RUN pnpm build
# Sanity: the two files the runtime crate include_str!s MUST exist.
RUN test -f sdks/bootstrap/dist/runtime-entry.js \
 && test -f sdks/bootstrap/dist/dispatcher.js

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
# The vendored migration engine. Cargo.toml lists `third_party/zero-migrate` as a
# WORKSPACE MEMBER (and names zero-migrate/-ir/-policy by path), so cargo cannot
# even resolve the workspace without it - this is not an optional dependency of
# one crate, it is a member manifest the whole build needs.
COPY third_party/ third_party/
# Cedar authz policies — crates/authz/build.rs parses these at build time
# (and engine.rs include_str!s them), both via `../../deploy/policies`.
COPY deploy/policies/ deploy/policies/
# sdks/ WITH the dist emitted by the node stage, so the runtime crate's
# include_str!("../../../../sdks/bootstrap/dist/{runtime-entry,dispatcher}.js")
# resolves against /build/sdks/bootstrap/dist/.
COPY --from=sdks /build/sdks/ sdks/
RUN cargo build --release \
    -p zeroship-control \
    -p zeroship-gateway \
    -p zeroship-worker \
    -p zeroship-auth \
    -p zeroship \
    -p zeroship-migrate-adapter --bin zeroship-platform-migrate \
    --features zeroship-migrate-adapter/platform-cli

# ---------------------------------------------------------------------------
# Stage 3 — runtime image.
# ---------------------------------------------------------------------------
# Use Ubuntu 24.04 (glibc 2.39) instead of Debian bookworm (glibc 2.36)
FROM ubuntu:24.04 AS runtime
RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates curl && rm -rf /var/lib/apt/lists/*
COPY --from=builder /build/target/release/zeroship-control /usr/local/bin/
COPY --from=builder /build/target/release/zeroship-gate /usr/local/bin/
COPY --from=builder /build/target/release/zeroship-worker /usr/local/bin/
COPY --from=builder /build/target/release/zeroship-auth /usr/local/bin/
COPY --from=builder /build/target/release/zeroship /usr/local/bin/
# The platform DB migration one-shot the compose `migrate` service invokes. It
# authors `db/migrations-ts/*.ts` in-process via zeroship-runtime's V8 and applies
# over the native compio-postgres seam — no recorder-child subprocess.
COPY --from=builder /build/target/release/zeroship-platform-migrate /usr/local/bin/

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
