# Browser-level E2E over a deployed stack (`tests/e2e_browser/`)

Playwright specs that drive a **real Chromium** against a **real, live zeroship
stack** (Control, a worker, the gateway, the CDC relay and an ephemeral
PostgreSQL), addressing deployed apps the way an end user's browser does. This
layer asserts the *browser/client* contract of a deployed app: that JavaScript
actually boots, the SPA mounts and round-trips through RPC into the DOM, SSG is
truly static, and an RPC stream renders incremental frames in a browser.

## What it covers

| Spec | Asserts (in a real browser) |
| --- | --- |
| `ssg.spec.ts` | Prerendered content is in the DOM; the page renders **with JavaScript disabled** (truly static, no hydration); an in-app `<nav>` link does a full document navigation (no SPA router). |
| `ssr.spec.ts` | Server-rendered post content is present on first paint (in the raw HTML response); **hydration actually boots** — a post page's prev/next button (onClick wired only after `hydrateRoot()`) navigates on click. |
| `csr.spec.ts` | The static shell has no app UI (JS-disabled); the **SPA mounts**; the todo list **round-trips through the `listTodos` RPC** into the DOM; Add-local is interactive; a deep SPA route serves the shell and the client router renders. |
| `streaming.spec.ts` | The `searchTodos` `stream()` RPC, consumed client-side, renders **incremental** frames — the rendered match count climbs through intermediate values, not one fused append. |

## How to run

From the repo root, inside `nix develop`, with Docker available:

```bash
pnpm install
pnpm build                                          # the SDK, vite-plugin and migration CLI dists setup uses
pnpm --filter @zeroship/e2e-browser-deployed test
```

That one command is the whole run; `node --run test` from this directory runs
the same script without a package manager. No `cargo xtask` area and no CI job
runs this suite. `global-setup.ts`:

1. launches Chromium once, so a browser that cannot start fails the run before
   anything is built;
2. builds `zeroship`, `zeroship-control`, `zeroship-worker`, `zeroship-gate` and
   `zeroship-data-cdc-server` from this checkout with `cargo build` (into
   whatever `CARGO_TARGET_DIR` names);
3. rebuilds `examples/{csr-todo,ssr-blog,ssg-docs}` from a clean `dist/` with
   `vite build`, and checks the ssg-docs manifest: no worker, every route served
   from assets, and the pages the specs load among them (`fixture/bundle.ts`);
4. starts PostgreSQL and the JWKS identity provider in containers
   (testcontainers), applies the platform migrations, and starts the services
   on free loopback ports;
5. creates one app per example, deploys its `dist/app.zship` with
   `zeroship deploy`, and waits until each answers through the gateway by the
   Host a browser sends (and, for the SPA, until its RPC answers);
6. writes `.stack.json` (gitignored), which `helpers.ts` reads for the gateway
   port and the app names.

The teardown it returns stops every service, removes the containers, the work
directory and `.stack.json`, and fails the run if a service died while the
specs ran.

The services run in process groups of their own, and the testcontainers reaper
is shared with every other testcontainers process of the same user, so neither
goes away with the runner. Every way out of the run takes them down:

- SIGINT during the specs: Playwright runs the teardown.
- SIGINT during bring-up: Playwright does not wait for the setup it
  interrupted, so the signal handler kills the services and removes the
  containers, the work directory and `.stack.json` synchronously.
- SIGTERM or SIGHUP at any point: Playwright handles neither, so the same
  handler does that and exits with `128 + signal number`.
- Any other exit that skipped the teardown: an `exit` listener does it.

A signal handler cannot wait on testcontainers' asynchronous client, so those
paths remove containers with the `docker` CLI. Setup looks each container up
through the CLI and fails if the CLI reaches a different daemon than the one
testcontainers chose.

`kill -9` of the runner is the exception. The containers carry the label
`ai.zeroship.fixture=e2e-browser-deployed`, so
`docker ps -a --filter label=ai.zeroship.fixture=e2e-browser-deployed` lists
what such a run left behind. Service, container, build and deploy logs stay in
`.artifacts/run-*/` (gitignored); a setup failure names the log to read.

There is no Playwright `webServer`: the stack is several processes, not one dev
server.

### Nothing is skipped

Every example the specs address is built and deployed, or setup fails naming
the example and the log. A build that exits cleanly without writing
`dist/app.zship` fails setup too. There is no state in which an example's specs
report `skipped`.

### The browser has to match the pinned Playwright

The browser comes from the development shell's `PLAYWRIGHT_BROWSERS_PATH`, and
`@playwright/test` has to be the release those browsers were built for; the
constraint and its check are explained in `xtask/tests/playwright_browsers.rs`.
When the directory does not hold the build, step 1 of setup fails, naming the
installed version and the path.

## `*.localhost` addressing

Browsers resolve `*.localhost` to 127.0.0.1 with no `/etc/hosts` entry. A
navigation to `http://<slug>.localhost:<gatePort>/` sends
`Host: <slug>.localhost:<port>`; the gateway strips `:port` and takes the first
label as the app name
(`crates/zeroship-gateway/src/router/dispatch.rs::extract_app_name`), then
routes to the deployed app. `helpers.ts::appUrl(kind, path)` builds these URLs
from the descriptor.

## Relationship to `tests/e2e-browser/`

`tests/e2e-browser/` runs each demo's `pnpm dev`: vite plus the local dev
runtime, no platform. This suite deploys the built bundles onto the platform
and goes through the gateway, so it is where the deployed shapes are checked:
a static-only manifest served with no worker, the SPA fallback for deep routes,
anonymous RPC and streaming admitted by the gateway, and SSR rendered by a
worker isolate.
