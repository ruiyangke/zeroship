# Codex brief — private registry Phase 2: generated apps consume @zeroship/* in the sandbox

## Goal
Make a generated app actually install and build `@zeroship/*` packages (esp.
`@zeroship/ui`) from the Verdaccio registry **inside the sandbox**. Phase 1 (registry +
publish pipeline) is committed; this phase wires the consumer side.

Worktree: `-C .worktrees/ui-design` (branch `builder/ui-design`). **DO NOT commit / merge /
push** — the pilot reviews and commits.

## Context (already in place)
- Verdaccio compose service + `config/verdaccio/config.yaml` + `scripts/publish-sdks.sh`
  (`pnpm publish:sdks`) + `docs/runbooks/private-registry.md`. `@zeroship` scope is hosted;
  everything else proxies npmjs.
- The sandbox installs deps via `pnpm install --frozen-lockfile || pnpm install`
  (`apps/zeroship-builder/src/server/internal/sandbox-backend.ts` ~line 457). The Builder
  agent writes the generated app's files incl. `package.json`. Sandbox base image:
  `crates/sandbox/docker/Dockerfile.sandbox-base` (`node:22-bookworm-slim`).
- Read `docs/decisions/2026-05-26-sdk-distribution-private-registry.md` (Phase 2 section).

## Do
1. **Scoped `.npmrc` in the generated app workspace.** The app needs
   `@zeroship:registry=<reachable-registry-url>` (anonymous read; NO publish token). Decide
   the cleanest source of truth and implement it:
   - Preferred: the Builder/scaffold writes `.npmrc` into the sandbox workspace so the
     scope→registry mapping travels with the app, with the URL from a config/env knob (e.g.
     `ZEROSHIP_SDK_REGISTRY`) so it differs by environment. Keep npmjs as the default
     registry (only the `@zeroship` scope points at Verdaccio) so react/vite/etc. still
     resolve.
2. **Registry reachable from the sandbox.** The URL the sandbox uses is NOT `localhost:4873`
   (that's the host). Implement + document per backend:
   - **docker** (the local-dev backend — implement + TEST this one): the sandbox container
     must reach the host/compose Verdaccio — e.g. join the compose network and use
     `http://verdaccio:4873`, or `host.docker.internal` / `--add-host`, whichever fits how
     `sandbox-backend.ts` launches containers. Wire it so the `.npmrc` URL and container
     networking agree.
   - **k8s** and **nomad-ch**: DOCUMENT the reachable URL (Service DNS / routable host) in
     the runbook; implement if low-risk, otherwise leave a clear TODO. Do not break them.
3. **Builder declares the deps.** When a generated app uses `@zeroship/ui` (or db/rpc/…),
   its `package.json` must list them. Update the Builder so the scaffold/`package.json` it
   writes can include `@zeroship/*` (at minimum: don't strip them; ideally the system prompt
   knows they're available — but PROMPT/agent-behavior changes that need OpenAI to validate
   are Phase 3; here just ensure the plumbing supports it).
4. Update `docs/runbooks/private-registry.md` with the Phase 2 consumer wiring (sandbox
   `.npmrc`, the env knob, per-backend reachable URL).

## Acceptance test (FAITHFUL e2e — NO OpenAI, this is the core deliverable)
Add a test (script or harness, runnable without the LLM) that exercises the REAL sandbox
install/build path — NOT a shim:
- Start Verdaccio (committed config) and `pnpm publish:sdks` (or reuse a running one).
- Create a sandbox via the real local (docker) backend with a MINIMAL generated app:
  `package.json` depending on `@zeroship/ui` (+ react/react-dom/vite), a scoped `.npmrc`
  pointing at the sandbox-reachable registry URL, a vite app whose entry imports a themed
  `@zeroship/ui` component wrapped in `ThemeProvider`.
- Run the sandbox's real install + build. ASSERT: `@zeroship/ui` resolved FROM Verdaccio
  (not bundled/shimmed), `pnpm install` + the build succeed, and the built output references
  the component (e.g. a token class / themed markup present).
- Tear down; stop anything you started by NUMERIC pid.
Capture all output. If the docker backend can't run in this environment, say so explicitly
and provide the closest faithful substitute (e.g. run the same install/build in a
`node:22` container against the registry), and document what a full sandbox run needs.

## Verify (NO OpenAI)
- The acceptance test passes (or the documented substitute does), with captured logs proving
  `@zeroship/ui` came from Verdaccio.
- `docker compose config` still valid; `pnpm --filter zeroship-builder build` still green.
- No regression to existing builder/sandbox behavior.

## Report (stdout)
- Where the `.npmrc` is written + the env knob; the per-backend reachable URL + what you
  implemented vs documented; the Builder dep-plumbing change; the acceptance test location +
  full output proving sandbox install/build of `@zeroship/ui` from the registry.
- What remains for Phase 3 (Builder composes from `@zeroship/ui`; Critic/Reviewer §4 gates).

## Constraints
- Pre-launch, no back-compat. Network + docker available (bypass flag). NO OpenAI usage.
- Faithful e2e only — the acceptance test must run the real install/build path, no stubs
  (see the repo's testing discipline).
- **DO NOT commit / merge / push.**
