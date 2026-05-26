# SDK distribution to generated apps — private npm registry

**Date:** 2026-05-26
**Status:** accepted (implementing)
**Context branch:** builder/ui-design

## Problem

Generated apps run in an isolated sandbox (docker / k8s / nomad-ch) where the Builder
agent writes a `package.json` and the sandbox runs `pnpm install`
(`apps/zeroship-builder/src/server/internal/sandbox-backend.ts`). The sandbox base image
(`crates/sandbox/docker/Dockerfile.sandbox-base`, `node:22-bookworm-slim`) pre-installs no
`@zeroship/*` packages, and there is no registry / workspace-link / vendoring mechanism. So
a generated app **cannot consume any `@zeroship/*` package** — not `@zeroship/ui`, nor
`@zeroship/db` / `rpc` / `auth` / `kv` / `storage`.

This blocks the design-system "moat" (`docs/design/ui-design-flow.md` §3: generated apps
*compose from* `@zeroship/ui`) and, more broadly, blocks generated apps from using the SDKs
the platform is built around.

## Decision

Stand up a **private npm registry (Verdaccio)** scoped to `@zeroship`, publish every
`sdks/*` package to it, and have the sandbox resolve the `@zeroship` scope from it via a
scoped `.npmrc`. This mirrors how creators' apps will consume `@zeroship/*` packages in
production (a registry), so it is the production-shaped answer, not a dev hack.

Rejected:
- **Pre-install in the sandbox base image** — fast cold-start but versions are frozen to
  the image; every SDK change needs a rebake; no per-app version control. Keep as a *cache*
  optimization later (pre-warm the store), not the source of truth.
- **Vendor into each generated app** — no infra, but design-system/SDK updates never
  propagate, apps bloat, and it diverges from the production consumption model.

## Design

### Registry
- **Verdaccio**, default port **4873**, `@zeroship:registry` scope. Uplinks to the public
  npm registry so non-`@zeroship` deps (react, vite…) still resolve through it (or are
  proxied) — generated apps point only the `@zeroship` scope at Verdaccio and keep npmjs as
  the default registry; both work.
- Pre-launch: anonymous read allowed; publish requires a token held by the publish script
  / control plane (not by sandboxes — sandboxes only read).
- Runs as a service: a local process for `zeroship serve` dev, and a `verdaccio` service in
  `docker-compose` for multi-node. Storage persisted to a volume.

### Publishing
- Every `sdks/*` package: `publishConfig.registry` → the Verdaccio URL, `private` removed/
  false, correct `files`/`exports`, build before publish (the SDKs already build via tsup).
- `scripts/publish-sdks.sh` (or a root pnpm script): build all SDKs in dependency order
  (bootstrap → db → …), then `pnpm -r --filter "./sdks/*" publish --no-git-checks` to the
  registry. Pre-launch uses a fixed/auto-bumped version (no semver ceremony; break freely).
- Dev runbook `docs/runbooks/private-registry.md`: start Verdaccio, publish, verify.

### Sandbox consumption (Phase 2)
- The per-app sandbox workspace (or base image) writes `.npmrc`:
  `@zeroship:registry=http://<registry-host>:4873` (+ anonymous read; auth token only if
  enabled). The Builder includes the needed `@zeroship/*` in the generated `package.json`.
- Network reachability documented per backend: docker (compose network / host gateway),
  k8s (Service DNS), nomad-ch (the VM must route to the registry host).
- Acceptance: a generated app with `@zeroship/ui` in deps runs `pnpm install` + builds in
  the sandbox and renders a themed component.

### The moat (Phase 3)
- Builder system prompt + scaffolding: compose UI from `@zeroship/ui`, wrap in
  `ThemeProvider`; declare `@zeroship/*` deps + the scoped `.npmrc`.
- Critic gains dimensions (composed-from-system / states / responsive / WCAG-AA / content);
  Reviewer gains blocker kinds for §4 violations. Update `ui-design-flow.md` §3 with the
  concrete names.

## Phases
1. **Registry + publish pipeline + runbook** (no sandbox/builder changes). ← implementing
2. Sandbox consumption (.npmrc + network + Builder deps); prove install+build in sandbox.
3. Moat: Builder composes from `@zeroship/ui`; Critic/Reviewer §4 gates.

## Verification (Phase 1, no OpenAI)
- Start Verdaccio; run the publish script; `npm view @zeroship/ui --registry
  http://localhost:4873` resolves; a throwaway dir `pnpm add @zeroship/ui` (scope pointed
  at Verdaccio) installs it and imports resolve.
- `docker compose up verdaccio` healthy; publish into the composed registry works.
