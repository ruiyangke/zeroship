# Browser-level E2E (`tests/e2e_browser/`)

Playwright specs that drive a **real Chromium** against a **real, live zeroship
stack** (control + worker + gateway + ephemeral Postgres), addressing deployed
apps the way an end user's browser does. This layer asserts the *browser/client*
contract that the curl harnesses cannot reach: that JavaScript actually boots,
the SPA mounts and round-trips through RPC into the DOM, SSG is truly static,
and an RPC stream renders incremental frames in a browser.

## What it covers

| Spec | Asserts (in a real browser) |
| --- | --- |
| `ssg.spec.ts` | Prerendered content is in the DOM; the page renders **with JavaScript disabled** (truly static, no hydration); an in-app `<nav>` link does a full document navigation (no SPA router). |
| `ssr.spec.ts` | Server-rendered post content is present on first paint (in the raw HTML response); **hydration actually boots** — a post page's prev/next button (onClick wired only after `hydrateRoot()`) navigates on click. |
| `csr.spec.ts` | The static shell has no app UI (JS-disabled); the **SPA mounts**; the todo list **round-trips through the `listTodos` RPC** into the DOM; Add-local is interactive; a deep SPA route serves the shell and the client router renders. |
| `streaming.spec.ts` | The `searchTodos` `stream()` RPC, consumed client-side, renders **incremental** frames — the rendered match count climbs through intermediate values, not one fused append. |

## How to run

You **must** be inside the nix env that exports `PLAYWRIGHT_BROWSERS_PATH`
(version-matched Chromium) and provides `playwright` 1.58.2 on PATH — the
npm-downloaded browsers can't link their libs on NixOS; the nix ones can. Do
**not** install a different Playwright version (skew breaks browser launch).

```bash
# from the repo root, inside the nix env:
cd tests/e2e_browser
playwright test                 # nix playwright on PATH
# or, equivalently:
pnpm exec playwright test
```

`global-setup.ts` spawns `scripts/up.sh` (full stack bring-up + deploys the
three render examples), and `global-teardown.ts` spawns `scripts/down.sh`
(kills the binaries, removes the ephemeral PG container, cleans the work dir).
There is **no Playwright `webServer`** — the stack is an external multi-process
deployment. The gateway port is dynamic; specs read it from `.stack.json`
(written by `up.sh`, gitignored) via `helpers.ts`.

### Prerequisites

- A release build: `target/release/{zeroship,zeroship-control,zeroship-gate,zeroship-worker}`
  (`cargo build --release`).
- `docker` (ephemeral Postgres), `node`, `openssl`.
- Built example artifacts: `examples/{csr-todo,ssr-blog,ssg-docs}/dist/app.zship`
  (`pnpm --filter <example> build`). A missing dist makes that example's specs
  `skip` rather than fail.

### Bring the stack up by hand (debugging)

```bash
bash tests/e2e_browser/scripts/up.sh     # leaves the stack running, writes .stack.json
bash tests/e2e_browser/scripts/down.sh   # tears it down
```

## `*.localhost` addressing

Browsers auto-resolve `*.localhost` → 127.0.0.1 (no `/etc/hosts` needed). A
navigation to `http://<slug>.localhost:<gatePort>/` sends
`Host: <slug>.localhost:<port>`; the gateway strips `:port` and extracts the
first subdomain as the app slug
(`crates/gateway/src/router/dispatch.rs::extract_app_name`), then routes to the
deployed app. `helpers.ts::appUrl(kind, path)` builds these URLs from the
descriptor.

## Division of labour: curl harness vs. browser harness

- **`tests/e2e_app_primitives_render.sh`** (curl) asserts the **server/wire**
  contract through the gateway: HTTP status, content-type, cache headers, the
  presence of SSR markup / hydration `<script>` tags / SPA shell bytes, and the
  raw SSE data-stream framing over the worker `/dispatch`. It never runs JS.
- **This Playwright layer** asserts the **browser/client** contract: that the
  shipped JS *boots* — hydration runs, the SPA mounts, RPC round-trips render
  into the live DOM, SSG stays static with JS off, and a stream renders frame by
  frame. It complements, and does not duplicate, the curl harness.

Both harnesses share one bring-up implementation: `tests/lib/e2e_stack.sh`
(`stack_up` / `mint_admin_pat` / `deploy_zship` / `stack_down`). The browser
harness uses a private port band (`GATE_PORT=8022`, etc.) and `*-bx` app slugs
so it can coexist with the `*-e2e` curl-harness apps on the same host.
