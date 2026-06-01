# Console → pure creator app (sandbox-craft only)

**Status:** approved (interactive, 2026-05-31). Supersedes the "act-as-creator
service-privilege removal" track for the console.

## Decision

The console (`apps/zeroship-builder`) becomes a **pure creator app**: it crafts
and previews apps **in a sandbox**, holds **no control-plane credential**, and
makes **zero control calls**. Platform-deploy is **removed** (deferred to a
future device-code flow). This cancels the act-as-creator privilege rework — the
need disappears rather than being reworked.

Rationale: the only thing that required the console's broad control service PAT
was the deploy + deployed-app dashboard. With deploy gone, the console is just an
authenticated creator app like any other (identity via the platform BFF session,
`currentUser()`), so it should carry no standing credential. Pre-launch, no
back-compat: rename/delete freely.

## Locked choices

1. **Remove the control surface.** Delete `server/control-client.ts` (+ test) and
   drop `@zeroship/control` from the builder's deps. Strip every control-backed
   action from `server/apps.ts`. Remove `control.deploy`/`control.getApp` from
   `server/internal/tools.ts`.
2. **Deploy tool → reviewer-only.** Remove the *ship* (no `control.deploy`). Keep
   the LLM reviewer as a standalone "review my app" quality tool that produces
   findings and ships nothing.
3. **Env + Logs via files-API conventions** (no new sandbox-controller endpoints —
   the sandbox is a 3-backend orchestrator and env only applies at launch, so
   dedicated endpoints are high-cost/low-value):
   - **Env** → `EnvCanvas` reads/writes a `.env` in the sandbox project root via
     the existing `/sandboxes/:id/files/{path}` GET/PUT. Vite auto-restarts the
     dev server on `.env` change, so edits apply on the next preview reload.
     vars+secrets collapse to one `.env` (a preview sandbox has no secret vault).
   - **Logs** → the preview-start command redirects the dev-server stdout/stderr
     to `.zeroship/dev.log`; `LogsCanvas` tails that file via the files API (it
     already polls every 2s).
4. **Home + Settings → builder projects (KV-local).** "My Apps" lists builder
   **projects** (per-thread sandbox sessions, already KV-backed via
   `internal/persist.ts`), not deployed apps. Settings keeps project identity +
   archive/delete (KV-local); plan/billing are removed.
5. **Seed (`crates/control/src/bootstrap_console.rs`).** Delete the service-PAT
   mint (`control.permission_tokens` insert) and the
   `ZEROSHIP_CONTROL_SERVICE_TOKEN` secret/expose injection. **Keep** the app row,
   the public PKCE client, the `.zship` ingest, and the `OPENAI_API_KEY` /
   `SANDBOX_*` runtime env (the console still needs those to craft). Drop the now-
   unused `ZEROSHIP_CONTROL_URL` from the runtime-env spec.

## Invariants (must hold after the change)

- The console imports `@zeroship/control` **nowhere**; `grep` proves zero hits.
- The console boots with **no** `ZEROSHIP_CONTROL_SERVICE_TOKEN` in its env, and
  the seed mints **no** PAT.
- Creator identity comes **only** from the platform BFF session (`currentUser()`).
- `server/apps.ts` (renamed `server/projects.ts`) talks only to the sandbox and KV.
- The reviewer tool ships nothing.

## Work breakdown

- **A — Builder server:** delete `control-client.ts`(+test); `apps.ts` →
  `projects.ts` (KV project list/archive/delete + `.env` and `.zeroship/dev.log`
  helpers over the sandbox files API); `sandbox-backend.ts` preview-start redirect
  to `.zeroship/dev.log`; `tools.ts` deploy tool → reviewer-only; drop the dep.
- **B — Builder client:** `EnvCanvas` → `.env` over files API; `LogsCanvas` →
  `.zeroship/dev.log` tail; `Home`/`SettingsCanvas` → projects (KV); update
  `client/api.ts` re-exports; remove deploy/plan/billing call sites.
- **C — Seed:** strip PAT + service-token env; keep the rest.
- **D — Sweep:** dangling imports, config (`server/config.ts` control refs),
  dead types, package.json dep, README/docs.

## Testing

Per fix, a regression test that fails pre-change:
- builder server: `.env` round-trip + `.zeroship/dev.log` tail over a fake/real
  sandbox files API; project list/archive over KV; reviewer tool ships nothing.
- seed: `bootstrap_console_test` asserts **no** PAT row and **no**
  `ZEROSHIP_CONTROL_SERVICE_TOKEN` in the app env (was present pre-change).
- a guard test/grep asserting the builder bundle has no `@zeroship/control`.
