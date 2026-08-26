# syntax=docker/dockerfile:1
#
# zeroship platform image - builds all service binaries:
#   zeroship-control, zeroship-gate, zeroship-worker, zeroship-auth,
#   zeroship (CLI), and zeroship-platform-migrate (the platform DB migration
#   one-shot - the compose `migrate` service runs it under the Platform trust
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
#      (crates/runtime/src/core/init.rs -> "../../../../sdks/bootstrap/dist/..."),
#      so they MUST exist before cargo touches zeroship-runtime.
#   2. `builder` (rust) copies crates/, the freshly-built sdks/, and the
#      deploy/policies/ tree (crates/authz/build.rs parses
#      ../../deploy/policies/*.cedar at build time) and compiles the seven
#      binaries below plus two the control crate carries and we do not ship
#      (zeroship-mock-stripe, dev-provision) - see the `--bin` note at the
#      cargo line for why they come along.
#   3. final (ubuntu) ships SEVEN binaries: zeroship-control, zeroship-gate,
#      zeroship-worker, zeroship-auth, zeroship, zeroship-platform-migrate,
#      zeroship-migrated (the creator migration service).
#      It ships NO console - see the apps/ note in stage 1.
#
# This header said "all seven binaries" until 2026-08-11, which was wrong in
# both directions: the count was never seven, and the cargo line was building
# exactly ONE.

# ---------------------------------------------------------------------------
# Stage 1 - build the SDK dist (bootstrap runtime-entry + dispatcher).
# ---------------------------------------------------------------------------
FROM node:22-bookworm AS sdks
WORKDIR /build
RUN corepack enable
# Rust, in the NODE stage. This looks wrong and is not.
# sdks/vite-plugin imports `zero-migrate-node`, a Rust N-API addon, and that
# addon's build is `napi build --platform --release`. `napi` is @napi-rs/cli,
# a NODE cli (its package.json:61 devDependency), and it compiles a Rust crate.
# So the addon needs BOTH toolchains in ONE stage - which is why the obvious
# fix (build it in the rust `builder` stage and copy the artifacts here) does
# not work: rust:latest has no Node, so it cannot invoke napi at all.
# Measured 2026-08-11 (task #318/#265).
# The SAME native deps the rust `builder` stage installs, and for the same
# reason: the addon's dependency tree reaches pg_query, whose build script
# drives bindgen, which dlopens libclang. MEASURED, not guessed - without
# these the build dies with:
#   bindgen-0.66.1/lib.rs:604:31: Unable to find libclang: "couldn't find any
#   valid shared libraries matching: ['libclang.so', ...]"
# A Rust toolchain alone is not enough; -sys crates need the C side too.
RUN apt-get update && apt-get install -y --no-install-recommends \
    clang libclang-dev cmake perl nasm pkg-config \
    && rm -rf /var/lib/apt/lists/*
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
    | sh -s -- -y --profile minimal --default-toolchain stable
ENV PATH="/root/.cargo/bin:${PATH}"
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
# pnpm-workspace.yaml lists packages/* and crates/zero-migrate-node as workspace
# members, and the lockfile pins them as importers - so --frozen-lockfile needs
# them PRESENT, and the root build script compiles the addon out of this tree.
# The engine's crates were a submodule under third_party/ and are now ordinary
# members, so these two COPYs replace the one that brought the submodule in.
COPY packages/ packages/
COPY crates/zero-migrate-node/ crates/zero-migrate-node/
RUN pnpm install --frozen-lockfile
# Root build script only builds sdks/* (topo: @zeroship/db ->
# @zeroship/bootstrap -> rest); it does not touch apps/ or examples/.
RUN pnpm build
# Sanity: the two files the runtime crate include_str!s MUST exist.
RUN test -f sdks/bootstrap/dist/runtime-entry.js \
 && test -f sdks/bootstrap/dist/dispatcher.js

# ---------------------------------------------------------------------------
# Stage 2 - native (Rust) build of all seven binaries.
# ---------------------------------------------------------------------------
FROM rust:latest AS builder
WORKDIR /build
# System build deps not in rust:latest:
#   clang + libclang-dev  -> bindgen (libsqlite3-sys, aws-lc-sys)
#   cmake                 -> aws-lc-sys / ring native build
#   perl                  -> aws-lc-sys / openssl-style asm generation
#   nasm                  -> aws-lc-sys x86_64 assembly
#   pkg-config            -> -sys crate probing
# (gcc/g++/make come with rust:latest's buildpack-deps base.)
RUN apt-get update && apt-get install -y --no-install-recommends \
    clang libclang-dev cmake perl nasm pkg-config \
    && rm -rf /var/lib/apt/lists/*
COPY Cargo.toml Cargo.lock ./
# The workspace's OWN linker flag, and the build cannot link without it.
#
# .cargo/config.toml sets `-Wl,--allow-multiple-definition` on linux, because
# libpg_query and librdkafka each vendor a copy of xxhash and export XXH32 /
# XXH_versionNumber with external linkage. `zeroship-platform-migrate` links
# BOTH: pg_query arrives via zero-migrate (always), and rdkafka arrives via
# zeroship-runtime -> zeroship-metering -> zeroship-stream, which the
# platform-cli feature turns on for the V8 authoring front-end. Verified with
# `cargo tree -i`, both edges.
#
# MEASURED on the host, without this COPY:
#   rust-lld: error: duplicate symbol: XXH_versionNumber
#     >>> defined at rdxxhash.c  in librdkafka_sys-*.rlib
#     >>> defined at xxhash.c    in libpg_query-*.rlib
#   error: could not compile `zeroship-migrate-adapter`
#         (bin "zeroship-platform-migrate")
#
# The collision was already diagnosed and resolved in-repo by acd5c2d72; the
# image simply never carried the config that resolves it. So this is NOT a
# product defect - a host `cargo build` of the same binary links fine, and I
# confirmed the flag reaches that link (`cargo build -v` on the host prints
# --allow-multiple-definition, no error) - it is the same shape as the
# third_party/ and libs/ COPYs BELOW: a root the build needs that the context
# never received.
COPY .cargo/ .cargo/
COPY crates/ crates/
# The migration engine needs no COPY of its own any more: its nine crates are
# ordinary `crates/*` members, so the `COPY crates/ crates/` above already
# carries them. It used to be a submodule outside that glob, which is why a
# separate COPY existed and why cargo could not resolve the workspace without it.
# Cedar authz policies - crates/authz/build.rs parses these at build time
# (and engine.rs include_str!s them), both via `../../deploy/policies`.
# The standalone compio drivers. crates/auth (and others) depend on
# `compio-postgres` by path into libs/, and Cargo.toml globs crates/* as
# workspace members, so cargo cannot even LOAD the workspace without libs/.
# MEASURED, not guessed - without this the release build dies at once with:
#   error: failed to load manifest for workspace member `/build/crates/auth`
#   Caused by: failed to read `/build/libs/compio-postgres/Cargo.toml`
#   Caused by: No such file or directory (os error 2)
COPY libs/ libs/
COPY deploy/policies/ deploy/policies/
# sdks/ WITH the dist emitted by the node stage, so the runtime crate's
# include_str!("../../../../sdks/bootstrap/dist/{runtime-entry,dispatcher}.js")
# resolves against /build/sdks/bootstrap/dist/.
COPY --from=sdks /build/sdks/ sdks/
# NO `--bin` HERE, and that is the whole point of this comment.
#
# This line used to end `-p zeroship-migrate-adapter --bin
# zeroship-platform-migrate`. `--bin` is a TARGET FILTER for the ENTIRE
# invocation, not for the -p it trails, so cargo built that one binary and
# ignored the other five package selections - AND EXITED 0. The image then
# died three stages later on a COPY of a binary nothing had produced:
#
#   ERROR: failed to compute cache key: "/build/target/release/zeroship":
#          not found
#
# MEASURED in that build's own log, which is what identifies this rather than
# the `Finished` line: `Finished release profile ... in 2m 39s` with ZERO
# occurrences of `Compiling zeroship-control`, `zeroship-gateway`,
# `zeroship-worker`, `zeroship-auth` or `zeroship v0`. The target dir was
# empty, so every crate the build needed had to print a Compiling line. Those
# five never did. One of six binaries, reported as success.
#
# Dropping `--bin` builds every target of every selected package, which is the
# seven we ship plus zeroship-control's `zeroship-mock-stripe` and
# `dev-provision`. Two extra links against libraries already compiled, and
# neither is COPYed into the runtime stage. The alternative - two cargo
# invocations - is correct too but costs a second full pass over the shared
# dependency graph, because the platform-cli feature set differs and cargo
# fingerprints them separately.
#
# `required-features` still keeps zeroship-platform-migrate honest: it is
# declared `required-features = ["platform-cli"]`, so the --features flag
# below is what makes it build at all, not --bin.
RUN cargo build --release \
    -p zeroship-control \
    -p zeroship-gateway \
    -p zeroship-worker \
    -p zeroship-auth \
    -p zeroship \
    -p zeroship-migrated \
    -p zeroship-migrate-adapter \
    --features zeroship-migrate-adapter/platform-cli

# ---------------------------------------------------------------------------
# Stage 3 - runtime image.
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
# over the native compio-postgres seam - no recorder-child subprocess.
COPY --from=builder /build/target/release/zeroship-platform-migrate /usr/local/bin/
# The CREATOR migration service. Distinct from the platform one-shot above:
# that one applies db/migrations-ts/*.ts (the platform's own schema) once at
# boot; this is a long-running HTTP service that applies a DEPLOYED APP's
# migrations into its per-app schema, under the operator-ceiling trust profile.
#
# It was missing from this image until 2026-08-11, and its absence is invisible
# until you deploy an app that has migrations: the `.zship` deliberately does
# NOT carry migration documents (crates/bundle/src/manifest.rs - they "are
# applied through the standalone migration service"), so the deploy SUCCEEDS,
# the app serves its assets and RPCs, and every env.db call then fails against
# tables that were never created. Compose even set the provisioning superuser
# DSN this service needs on the CONTROL service - which has no code that reads
# one; it is ZEROSHIP_MIGRATED_PROVISION_DATABASE_URL and it belongs here.
COPY --from=builder /build/target/release/zeroship-migrated /usr/local/bin/
# The migration SOURCE the one-shot above reads, baked in rather than
# bind-mounted. The compose `migrate` service used to mount
# `../../db/migrations-ts` from a repo checkout, which meant a host running this
# image also had to hold the source tree. Baking it makes the image the whole
# deploy unit, and pins the migrations to the same commit as the binary that
# applies them - a bind mount let the two drift.
COPY db/migrations-ts /db/migrations-ts

# ---------------------------------------------------------------------------
# Stage 4 - frontend dev image: the runtime image (so it has the `zeroship`
# CLI built for THIS glibc) PLUS Node 22. The AI builder's `vite dev` runs the
# vite-plugin dev-bootstrap, which SPAWNS `zeroship` to host the app's server
# functions in the real runtime - so the builder container needs both Node
# (for vite) and the zeroship binary on PATH (built for ubuntu 24.04's glibc;
# a plain node:22-bookworm image is glibc 2.36 and can't run it).
# ---------------------------------------------------------------------------
FROM runtime AS frontend
RUN curl -fsSL https://deb.nodesource.com/setup_22.x | bash - \
 && apt-get update && apt-get install -y --no-install-recommends nodejs git \
 && rm -rf /var/lib/apt/lists/* \
 && corepack enable
