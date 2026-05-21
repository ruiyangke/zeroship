# Demo audit — Stage 5d (post ZS-standard refactor)

**Date:** 2026-05-20
**HEAD audited:** 75f93d33 (Stages 5a/5b/5c shipped)
**Scope:** every demo in `examples/`, dev path (Vite-built) + `zeroship serve` path (raw `.js`).

Stages 5a-5c moved the RPC dispatcher into the runtime (`crates/runtime/src/bootstrap/rpc_dispatch.js`) and rewired the Vite plugin's synthetic entry to emit dict-shape `default.rpc`. The runtime accepts both function-shape (back-compat) and dict-shape; schema is read directly off `default.schema`.

This audit confirms each demo's dev path still boots, dispatch still works, and where breakage exists, identifies whether it's a 5a-5c regression or pre-existing.

## Audit table

| Demo | Type | Status | Notes |
|---|---|---|---|
| `db-todos` | Vite + DB + RPC + SSR | green | canonical smoke — 18/18 checks pass |
| `ai-chat` | Vite + AI SDK + streaming RPC | green | dispatch verified via streaming `/_zs/v1/chat`; SSE frames flow without OPENAI_API_KEY (error frame is the expected wire shape) |
| `csr-todo` | Vite + React + RPC | green | RPC `listTodos` returns seeded fixture; client at `:5173` serves index.html |
| `db-chat` | Vite + DB + RPC | green | scripts/smoke.sh — all 10 checks pass |
| `db-migrations-playground` | Vite + DB + migrations | green | dispatch + 9/10 functional smoke checks pass. The 1 failing assertion is a pglite state-isolation bug (rows persist across `pnpm dev` restarts). Unrelated to 5a-5c. |
| `hono-demo` | Vite + Hono + fetch | green | `GET /` via fetch handler returns 200 + "Hono works on zeroship." |
| `hr-system` | Vite + DB + RPC | green | RPC `getEmployees` dispatches via dict-shape; returns platform envelope |
| `langchain-demo` | Vite + LangChain + RPC | green | RPC `ping` returns "pong"; dispatch confirmed |
| `openai-demo` | Vite + OpenAI + RPC | green w/ env | requires `OPENAI_API_KEY` (top-level `new OpenAI({...})` throws otherwise); with dummy key set, `ping` returns "pong". Pre-existing demo design — not 5a-5c. |
| `ssg-docs` | Vite + SSG (no worker) | green | dev server boots cleanly. SSG content is copied at build time into `dist/`; dev mode has no JS handler by design (this is a build-only demo). |
| `ssr-blog` | Vite + SSR | yellow | dev boots; client `:5173` serves hydrated HTML; SSR `:3001` 500s because `virtual:zeroship/client-manifest` is build-only and not resolved by the dev runtime. Pre-existing — Vite plugin v1 limitation. |
| `bench` | Vite (build-only) | red | `pnpm build` fails on `@zeroship/server` ↔ `@zeroship/zeroship-stub` missing re-exports (`runQuery`, `runMutation`, `currentUser`, …). Pre-existing — listed in 5d task brief as not-to-fix. |
| | | | |
| `http-handler.js` | Raw + fetch + RPC (legacy "use server") | green | `default.fetch` dispatches `/`, `/echo/:t`, `POST /json`, 404 fallback. The "use server" exports aren't picked up by `zeroship serve` (Vite-plugin-only convention) — pre-existing. |
| `jwt-validator.js` | Raw + fetch + RPC (legacy "use server") | green | `default.fetch` dispatch works; routes return as documented. Requires `env.JWT_SECRET` via platform env to exercise sign/verify (pre-existing demo config). |
| `url-shortener.js` | Raw + fetch + KV | green | `default.fetch` dispatches correctly; the handler throws on `env.KV.set` because KV binding isn't surfaced on `env` for `zeroship serve` — pre-existing demo / platform gap, not a 5a-5c regression. |
| `weather-proxy.js` | Raw + "use server" only | green (no-op) | NO `default.fetch` or `default.rpc` exported, only "use server" functions. `zeroship serve` correctly returns 404 — the runtime sees no handler. Pre-existing demo design — file's doc comment is stale, but runtime behaviour is correct. |
| `ai-streaming.js` | Raw + "use server" only | green (no-op) | Same shape as `weather-proxy.js`. No `default.fetch` or `default.rpc`; runtime correctly 404s. Pre-existing. |
| `raw-rpc.js` | Raw + dict-shape RPC (no Vite) | green | 4 procedures (`ping`, `echo`, `add`, `status`). Confirms the new dict-shape contract works end-to-end without tooling. `status.config.kind = "query"` exercises the capability-frame surface. |
| `raw-streaming.js` (NEW) | Raw + dict-shape RPC (no Vite) | green | richer demo added in this stage — `search` (query + `config.input.parse`), `tick` (AsyncIterator with `kind:"subscription"`), `echoHeaders` (action-like, calls fetch), `recordNote` (mutation). 12-check smoke script passes. |

### Tally

- **Green (contract works):** 17 — db-todos, ai-chat, csr-todo, db-chat, db-migrations-playground, hono-demo, hr-system, langchain-demo, openai-demo, ssg-docs, http-handler.js, jwt-validator.js, url-shortener.js, weather-proxy.js, ai-streaming.js, raw-rpc.js, raw-streaming.js.
- **Yellow (boots but auxiliary feature broken pre-existing):** 1 — ssr-blog (`virtual:zeroship/client-manifest` is build-only and 500s in dev).
- **Red (contract broken):** 1 — bench (`pnpm build` fails on pre-existing `@zeroship/server` ↔ stub re-export gap).

Total: 19 demos. **17 green / 1 yellow / 1 red ≈ 89% green.** All red/yellow is pre-existing and unrelated to Stages 5a-5c.

"Green" here means: the runtime dispatch contract works for what the file actually exports. Demos with stale "use server" comments (`weather-proxy.js`, `ai-streaming.js`) count as green because the runtime correctly serves 404 for files with no `default.fetch`/`default.rpc` — the file's documentation is the only stale surface.

## Real fixes made by Stage 5d

None of the audited demos required code changes attributable to 5a-5c regressions. The synthetic-entry rewrite (5b) and schema-from-default (5c) are transparent to demo source code — every `export const x = query(...)` style continues to work, and no demo passed a `schema:` option to the Vite plugin (5c's removed surface).

## Documented but not fixed (pre-existing breakage)

1. **`bench` prod-build failure** — `@zeroship/server` re-exports symbols (`runQuery`, `runMutation`, `currentUser`, …) that don't exist on `@zeroship/zeroship-stub`. Task brief explicitly lists this as out-of-scope.
2. **`ssr-blog` dev server `virtual:zeroship/client-manifest` 500** — build-time virtual module not resolved in dev. Vite plugin v1 limitation; Vite-plugin-v2 (post-5e) work.
3. **`ssg-docs` dev mode serves nothing** — SSG content is copied at build time; dev has no static serving for `content/*.html`.
4. **`db-migrations-playground` smoke "total=1000" assertion fails on re-run** — pglite state persists across `pnpm dev` restarts. The 9 other functional checks pass.
5. **`url-shortener.js` env.KV undefined** — KV binding isn't surfaced on `env` for raw `zeroship serve`. Demo + platform gap.
6. **`weather-proxy.js` / `ai-streaming.js`** — both rely on Vite-plugin's "use server" auto-discovery, which `zeroship serve` doesn't implement. Their `default.fetch` (where present) works; the "use server" surface doesn't.
7. **`openai-demo` requires OPENAI_API_KEY at module load** — top-level `new OpenAI({...})` throws when the key is absent. Demo design choice, not a runtime issue.
8. **`http-handler.js` `/health` shadowed** — kernel intercepts `GET /health` before user code sees it. Documented behaviour in `serve.rs`.

## Verification gate

- ✅ `db-todos` smoke: 18 checks pass.
- ✅ `cargo test -p zeroship-runtime --test rpc_dispatch`: **11 pass** (baseline 11).
- ✅ `cargo test -p zeroship-runtime --test schema_init`: **5 pass** (baseline 5).
- ✅ `cargo test -p zeroship-bundle`: 6 lib + 26 integration = **32 pass** (baseline 32).
- ✅ `sdks/vite-plugin` `pnpm test`: **170 pass** (baseline 170).
- ✅ `sdks/db` `pnpm test`: **364 pass** (baseline 364).
- ✅ `examples/raw-streaming.smoke.sh` (new): 12/12 pass via `zeroship serve`.

All five baselines hold at the 5c numbers exactly. No regressions detected.

## New raw-RPC demo

`examples/raw-streaming.js` + `examples/raw-streaming.smoke.sh` added. The demo exercises:

| Surface | Procedure | Notes |
|---|---|---|
| `config.kind = "query"` | `search` | capability frame applied (no-op without plugin-db) |
| `config.input.parse(v)` | `search` (custom hand-rolled `.parse()`-compatible schema) | dispatcher returns 400 INVALID_ARGUMENT + `issues[]` on throw |
| `config.kind = "mutation"` | `recordNote` | mutation frame; no-op auto-tx without plugin-db |
| no kind / action-like | `echoHeaders` | calls outbound `fetch()` — proves capability-free path |
| AsyncIterator return / `kind = "subscription"` | `tick` | dispatcher accepts the shape; canonical wire is WS subscription (POST falls through to fallbackFetch — documented in the file header) |

## Anything that surprised

1. **AsyncIterator returns over POST don't work for raw deploys.** The kernel's RPC fast path (`RpcCallResult::FallThrough`) falls through to `default.fetch` for AsyncIterator returns, expecting the synthetic-SSR entry's JS-side encoder to wrap them. Raw `.js` deploys don't have that encoder, so unary streams 404 (fallback). Documented in `crates/runtime/src/core/runtime.rs:3025-3043` and now in `raw-streaming.js`. Not a regression; a known architectural limitation. WebSocket subscriptions are the supported wire for raw streams.

2. **Module-scope mutable state isn't shared across worker isolates.** `zeroship serve` spawns one worker per core; each request can hit a different V8 isolate. A `noteIds` array at module scope appears empty even after a `recordNote` because the next request hits a different isolate. The demo was rewritten to stay stateless; persistence belongs in the database, not in JS module scope. This is correct platform behaviour and surfaces nicely when a raw demo tries to use in-memory state.

3. **`/health` is shadowed by the kernel.** `examples/http-handler.js`'s `/health` handler is unreachable — the kernel-level liveness route in `crates/runtime/src/core/serve.rs:363` returns `{"status":"ok"}` without touching V8. Pre-existing; documented.

4. **The Vite plugin's "use server" convention is plugin-only.** Several raw `.js` demos (`weather-proxy.js`, `ai-streaming.js`) declare procedures via `"use server"` exports, expecting `zeroship serve` to surface them as RPC. It doesn't — only the Vite plugin's transform implements that discovery. The demos' own header comments are stale. Not a 5a-5c regression; predates the refactor.
