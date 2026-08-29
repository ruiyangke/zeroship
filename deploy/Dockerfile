# syntax=docker/dockerfile:1
#
# zeroship platform image - builds all service binaries:
#   zeroship-control, zeroship-gate, zeroship-worker, zeroship-auth,
#   zeroship (CLI), zeroship-migrate-server (the creator migration service).
#
# THE PLATFORM MIGRATION ONE-SHOT IS NO LONGER A RUST BINARY IN THIS IMAGE.
# `zeroship-platform-migrate` was deleted on 2026-08-28; the platform schema is
# applied by the general `zero-migrate` CLI, which is a NODE program. It cannot
# ship in the `runtime` stage: that stage is ubuntu:24.04 plus six static
# binaries and deliberately carries no Node, and adding Node there would put a
# JavaScript runtime into the control, gateway, worker and auth containers, none
# of which migrate anything. So the one-shot gets its OWN stage (`migrate`,
# below) and the compose `migrate` service builds that target.
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
#   3. `runtime` (ubuntu) ships SIX binaries: zeroship-control, zeroship-gate,
#      zeroship-worker, zeroship-auth, zeroship, zeroship-migrate-server.
#      It ships NO console - see the apps/ note in stage 1.
#   3b. `migrate` (node:22-bookworm-slim) carries the `zero-migrate` CLI, the
#      napi addon the node stage already built, the migration corpus and the
#      platform charter. It is the compose `migrate` one-shot's image and
#      nothing else uses it.
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
# sdks/vite-plugin imports `zeroship-migrate-node`, a Rust N-API addon, and that
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
# pnpm-workspace.yaml lists packages/* and crates/zeroship-migrate-node as workspace
# members, and the lockfile pins them as importers - so --frozen-lockfile needs
# them PRESENT, and the root build script compiles the addon out of this tree.
# The engine's crates were a submodule under third_party/ and are now ordinary
# members, so these two COPYs replace the one that brought the submodule in.
COPY packages/ packages/
# `pnpm build` starts by compiling the Rust N-API addon. Its metadata pass
# resolves every local dependency kind of every copied Cargo workspace member,
# including dev dependencies, so a curated crate closure silently couples this
# Docker stage to future manifest changes. Keep the workspace root and its full
# `crates/*` membership together, as the native builder stage does below.
#
# THIS REPLACED A CURATED NINE-CRATE LIST, and the numbers are why. Measured
# 2026-08-29 on cold, separately-built BuildKit builders:
#
#   curated list (the addon's true closure):  728.17 MB of build context
#   wholesale `COPY crates/`:                 728.51 MB   (+0.34 MB, ~0.05%)
#
# because THE CURATED LIST HAD GROWN TO 34 OF THE 35 CRATES. It began as nine,
# computed from `cargo metadata` on `crates/zeroship-migrate-node`, and the
# addon's dependency closure widened underneath it. A list that names all but one
# member is not a minimisation; it is a coupling with a rounding error attached.
#
# The list was also a deliberate tripwire - its own comment said "a tenth crate
# entering the closure fails this build loudly, which is the point" - and it DID
# fire: `zeroship-core` entered the closure and this stage stopped building. Nobody
# heard it, because nothing builds `--target migrate` by default; the corpus gate's
# image arm is opt-in behind `--with-container`. A tripwire on a path no one walks
# reports to no one, which is why that arm now runs in the gate rather than being
# left to refuse in silence.
#
# One mechanical trap the old list was right about, if anyone reinstates a subset:
# `COPY crates/zeroship-migrate* crates/` FLATTENS the matched directories into the
# destination and produces a tree cargo cannot read. Copy the parent, or list each
# directory on its own line.
COPY Cargo.toml Cargo.lock ./
COPY crates/ crates/
# `libs/` (4.6 MB of standalone driver source, no path deps, nothing here builds
# it) exists ONLY to satisfy the root manifest's second members glob. Cargo does
# not treat a glob that matches nothing as empty - it falls back to the literal
# path and reports "failed to load manifest for workspace member /build/libs/*".
# Measured 2026-08-28 on a scratch workspace: absent -> exit 101; present but
# EMPTY -> exit 101 with the identical message; populated -> exit 0. So `mkdir
# libs` would not have worked and the directory has to arrive with crates in it.
COPY libs/ libs/
# The platform schema corpus is a workspace member too (pnpm-workspace.yaml says
# why), so --frozen-lockfile needs it present or the install fails on a missing
# importer. Copying it HERE rather than only in the `migrate` stage is also what
# produces `db/migrations-ts/node_modules/zero-migrate` - the link the corpus's
# own `import ... from "zero-migrate"` resolves through.
COPY db/migrations-ts/ db/migrations-ts/
RUN pnpm install --frozen-lockfile
# The root build compiles the native addon before the JavaScript packages.
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
# XXH_versionNumber with external linkage. Any target linking BOTH needs the
# flag: pg_query arrives via zero-migrate, rdkafka via zeroship-runtime ->
# zeroship-metering -> zeroship-stream.
#
# THE TARGET THAT FIRST HIT THIS IS GONE AND THE FLAG IS STILL NEEDED. It was
# `zeroship-platform-migrate`, deleted 2026-08-28. Do not read its absence as
# "the collision belonged to that binary": both carriers are ordinary
# dependencies of the servers this stage still builds. Dropping this COPY was
# MEASURED on the host to fail the link:
#   rust-lld: error: duplicate symbol: XXH_versionNumber
#     >>> defined at rdxxhash.c  in librdkafka_sys-*.rlib
#     >>> defined at xxhash.c    in libpg_query-*.rlib
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
# `-p zeroship-migrate-adapter --features .../platform-cli` used to trail this
# list. Both went with the binary: the adapter is a library-only crate now, and
# it still gets built because zeroship-migrate-server depends on it for
# `CompioPgSession`.
RUN cargo build --release \
    -p zeroship-control \
    -p zeroship-gateway \
    -p zeroship-worker \
    -p zeroship-auth \
    -p zeroship \
    -p zeroship-migrate-server

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
# one; it is ZEROSHIP_MIGRATE_SERVER_PROVISION_DATABASE_URL and it belongs here.
COPY --from=builder /build/target/release/zeroship-migrate-server /usr/local/bin/
# The migration SOURCE, baked in rather than bind-mounted, so a host running this
# image does not also have to hold the source tree and the migrations are pinned
# to the same commit as the code.
#
# THIS COPY IS NOT THE PLATFORM APPLY PATH, and nothing in this image can apply
# it. The runtime stage has no Node, so the `zero-migrate` CLI cannot run here at
# all; and these files sit at `/db` with no `node_modules` above them, so their
# `import ... from "zero-migrate"` would not resolve even if it could. The
# platform one-shot is the separate `migrate` stage below, which places the same
# corpus at `/app/db/migrations-ts` for exactly that reason. As of 2026-08-28 no
# compose service reads this path (`grep -n '/db/migrations-ts'` over
# deploy/compose/docker-compose.yml finds only comments), so it is a candidate for
# deletion - left in place here because proving nothing outside deploy/ reads it
# is a separate piece of work from restoring the apply.
COPY db/migrations-ts /db/migrations-ts

# ---------------------------------------------------------------------------
# Stage 3b - the platform migration one-shot.
#
# WHY A SEPARATE IMAGE. The `zero-migrate` CLI is a Node program that loads a
# native napi addon; the runtime stage is ubuntu:24.04 with six static binaries
# and no Node. Installing Node into `runtime` would hand a JavaScript runtime to
# control, gateway, worker and auth, none of which migrate anything, so the
# one-shot gets its own image and the compose `migrate` service builds
# `--target migrate`.
#
# WHY node:22-bookworm-slim AND NOT THE RUNTIME BASE. The addon is a native
# `.node` compiled in the `sdks` stage, which is node:22-bookworm (glibc 2.36).
# Staying on that base keeps the ABI the addon was built against rather than
# relying on ubuntu 24.04's newer glibc to run it.
#
# WHAT IS COPIED, and why the whole store rather than a prune. pnpm links
# workspace packages by symlink into `node_modules/.pnpm`, so the CLI, the addon,
# `pg` and the authoring DSL are one connected store; copying the store plus the
# package directories is the shape that actually resolves. A `pnpm deploy`-style
# prune is smaller and is a SEPARATE change - it needs its own verification, and
# an image that resolves is worth more than a smaller one that does not.
#
# WHY THE CORPUS LIVES UNDER /app AND NOT AT /db. It used to be copied to
# `/db/migrations-ts` while `node_modules` sat at `/app/node_modules`, and that
# layout cannot work: the CLI loads each migration with a plain `import()` under
# tsx, so `import ... from "zero-migrate"` resolves by Node's UPWARD WALK from the
# migration's own directory - `/db/migrations-ts/node_modules`, `/db/node_modules`,
# `/node_modules` - none of which exist. There was also no `package.json` at `/db`
# or `/`, so tsx read the `.ts` files as CJS and failed with `Cannot require() ES
# Module ... in a cycle` before resolution was even reached.
#
# Copying it to `/app/db/migrations-ts` fixes both at once and needs no env var:
# the member's own `node_modules/zero-migrate` comes across as the RELATIVE
# symlink pnpm wrote (`../../../packages/zero-migrate`), which resolves here to
# `/app/packages/zero-migrate` because the repo-relative layout is preserved; and
# `db/migrations-ts/package.json` supplies `"type": "module"`.
#
# MEASURED 2026-08-28, and this is the first time this target has ever been
# built. `docker build --target migrate`, then the image run against an empty
# database on a fresh PostgreSQL 17.11: 35 of 35 files applied and exit 0,
# producing 104 tables, 63 functions and the schemas `public`, `service_authn`,
# `zeroship`, `zeroship_migrations` - byte-identical counts to the host apply. An
# immediate second run of the image reported `"applied":[]` for all 35 and exited
# 0. `tests/platform_migration_corpus_gate.sh --with-container` is that run.
#
# Getting here took three build failures that no one had seen, because nothing had
# ever built this: the missing Cargo workspace root, the addon's missing crate
# closure, and `libs/`. See the `sdks` stage.
#
# The `--version` line below still earns its keep - it turns a resolution mistake
# into a build failure - but it is no longer the only thing behind this target.
# ---------------------------------------------------------------------------
FROM node:22-bookworm-slim AS migrate
WORKDIR /app
COPY --from=sdks /build/node_modules /app/node_modules
COPY --from=sdks /build/packages /app/packages
COPY --from=sdks /build/crates/zeroship-migrate-node /app/crates/zeroship-migrate-node
COPY --from=sdks /build/package.json /build/pnpm-workspace.yaml /app/
# The corpus, from the `sdks` stage rather than the build context, so its
# `node_modules/zero-migrate` link comes with it. Copying it from the context
# would bring the 35 `.ts` files and NOTHING to resolve their import against.
COPY --from=sdks /build/db/migrations-ts /app/db/migrations-ts
# The two files that decide what a platform migration may do: the Platform charter
# (the guard and executor posture) and the table-to-owner registry, without which
# every op is refused as targeting a table owned by nobody.
COPY policies/platform.policy.toml /policies/platform.policy.toml
COPY policies/platform-table-owners.json /policies/platform-table-owners.json
# The entrypoint. It takes `--database-url-file PATH` - the same spelling the
# retired Rust one-shot took and the same `migrate-dsn` mount `migrated` reads -
# and turns it into the 0600 `zero-migrate.toml` the CLI reads with `--config`,
# so the SUPERUSER DSN never reaches a compose `command:` and therefore never
# reaches `docker inspect` or /proc/<pid>/cmdline.
COPY deploy/ops/migrate-entrypoint.sh /usr/local/bin/zeroship-migrate-platform
RUN chmod 0755 /usr/local/bin/zeroship-migrate-platform
# Fail at BUILD time if the CLI is not resolvable, rather than at the first
# deploy. `--version` loads the addon, so this also proves the native `.node`
# came across and matches this base's glibc.
RUN node /app/packages/zero-migrate-cli/dist/cli-bin.js --version
# ENTRYPOINT, not CMD, so the compose `command:` supplies FLAGS to the wrapper
# rather than replacing it. A CMD here would let a `docker compose run --rm
# migrate <anything>` bypass the wrapper entirely and lose the DSN handling that
# is its only reason to exist.
ENTRYPOINT ["/usr/local/bin/zeroship-migrate-platform"]

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
