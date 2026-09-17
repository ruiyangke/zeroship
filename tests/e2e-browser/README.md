# Browser-tier e2e — one suite per demo under `examples/`

Runs each demo's **real** dev server, drives its **real** UI in a **real**
browser, and asserts on what a user would see. One summary line at the end says
how many demos actually work.

```bash
pnpm --filter @zeroship/e2e-browser test          # every demo
pnpm --filter @zeroship/e2e-browser test suites/starter.test.ts
```

## Why this tier exists

`examples/db-todos` boots, serves `index.html` with HTTP 200, and registers all
five plugins — and every procedure that touches `env.db` throws, so the app is a
shell. Nothing caught it. A check that fetches the page cannot: the page is fine.
Only clicking the button catches this.

So the bar here is not "the tests pass". It is that a demo which cannot work
**fails, loudly, naming what was missing**.

## Rules

- **Never skip.** There is no `it.skip`, no conditional suite, no "ignored"
  state. A skipped demo is indistinguishable from a working one, which is the
  exact hole this tier fills. A demo whose dependency is missing fails naming the
  dependency (`DATABASE_URL unset`); a demo that cannot boot fails with the
  dev-server log attached.
- **Never retry** (`retry: 0`). A retry turns a flaky demo green, and "does this
  demo work" is the one question we refuse to blur.
- **No stubs.** No mock server, no fixture standing in for the app. Every suite
  runs `pnpm dev` in the demo's own directory.
- **A demo with any failing test is BROKEN**, not partially working.

## Where things are

| File | Responsibility |
| --- | --- |
| `src/demos.ts` | The demo registry — ports, env vars, required dependencies |
| `src/dev-server.ts` | Boot a demo's dev server, wait for real readiness, tear it down |
| `src/browser.ts` | Resolve a chromium this machine can actually run |
| `src/suite.ts` | `describeDemo` — boot, page-per-test, error capture |
| `src/ui.ts` | Interaction helpers that report failure instead of throwing |
| `src/reporter.ts` | The per-demo summary table |
| `suites/<demo>.test.ts` | One suite per demo |

## Three environment hazards this tier handles

**1. NixOS cannot run Playwright's own browsers.** They are linked against a
standard FHS layout that does not exist here. Measured with
`PLAYWRIGHT_BROWSERS_PATH` unset:

```
Error: browserType.launch: Executable doesn't exist at
~/.cache/ms-playwright/chromium_headless_shell-1208/.../chrome-headless-shell
```

`src/browser.ts` uses the **system** chromium via `executablePath` (Nix built it
and patchelf'd it, so it works with no ambient environment), and falls back to
the Nix `playwright-browsers` derivation when `PLAYWRIGHT_BROWSERS_PATH` is set
(i.e. inside `nix develop`). If neither launches it throws with every attempt's
error — it never silently proceeds without a browser.

Override with `ZEROSHIP_E2E_CHROMIUM=/path/to/chromium`.

**2. Port collisions.** Every example that does not pass `devServerPort` falls
back to the plugin default of `3001`, so two examples running at once fight over
one port and the loser fails opaquely. Each demo here gets an explicit unique
pair — vite on `5310+n`, the zeroship dev runtime on `3310+n` — passed through
the env var that demo's `vite.config.ts` reads.

**3. Shared on-disk state.** Ports are not the only shared resource. The dev
runtime takes an **exclusive file lock** on `.zeroship/kv.redb` inside the
example directory, so two runs of the same demo crash-loop with
`Database already open. Cannot acquire lock.` no matter what the ports are. Each
run gets a private state directory via `ZEROSHIP_KV_PATH` and a fresh
`DATABASE_URL` sqlite file, which also makes "reload and it is still there" a
real assertion rather than a read of last run's leftovers.

## Readiness is stricter than "the port answers"

A crash-looping runtime still serves the vite shell perfectly. So boot waits for
the vite shell **and** a response from the dev runtime, while watching the child
output for the plugin's `runtime exited unexpectedly` banner. Three restarts is a
crash loop and fails the demo with the log tail.

## What a failure shows you

The wire deliberately flattens a handler throw to `{"message":"internal error"}`,
which tells a reader nothing. Failures therefore attach the runtime's own error,
mined out of the dev-server log:

```
server-side errors (from the dev runtime log):
    TypeError: Cannot read properties of undefined (reading 'find')
      at /home/…/examples/db-todos/src/index.ts:66:8
RPC errors:
    500 http://localhost:5310/__zeroship/v1/users.public
      {"message":"internal error","name":"Error","request_id":"8"}
```

## Adding a demo

1. Add an entry to `DEMOS` in `src/demos.ts` with unused ports.
2. Make that example's `vite.config.ts` read the `apiPortEnv` variable for
   `devServerPort`, defaulting to its current value.
3. Add `suites/<name>.test.ts` using `describeDemo`.

A registry entry with no suite reports `NO RESULT` — it cannot go quiet.

## Relationship to the other tiers

This tier is the browser layer above the build-to-deploy checks: those prove an
artifact deploys, this proves a demo is usable.
