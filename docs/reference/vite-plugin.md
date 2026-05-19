# `@zeroship/vite-plugin` — server-module discovery

**Status:** Reference

Plugin tracker for the build/transform side of zeroship apps. This page covers the rules for what becomes an RPC endpoint vs. what stays private to the server bundle. Other build-system pieces (Vite Environment API, the synthetic SSR entry, asset emission) live in `docs/reference/vite-environment-api.md` and `docs/proposals/rpc.md`.

## What gets published as an RPC

A file's exports become network-reachable RPC procedures (mounted at `POST /_zs/v1/<wireId>`) iff **both** of the following hold:

1. The file opens with the file-level `"use server"` ECMAScript directive (i.e. the first non-comment statement is the string-literal expression `"use server"`).
2. The export's initializer is a call to one of the wrapper markers — `procedure`, `query`, `mutation`, `stream`, or `subscription` — imported from `@zeroship/server` (or `@zeroship/rpc`).

Everything else stays in the server bundle as a private helper. Plain `export function`, plain `export const x = ...`, types, constants — none reach the wire. Re-exports (`export * from "./_helpers"`) don't auto-publish either, because the helpers' names don't appear as wrapper calls in the re-exporting module.

```ts
"use server";

import { procedure, query, mutation, stream, z } from "@zeroship/server";

// ── RPC procedures (network-reachable) ─────────────────────────────
export const list = query(async () => db.todos.find({}));

export const create = mutation(
  async (input: { text: string }) => db.todos.insert(input),
  {
    id: "todo.create",                 // explicit wireId (required for prod)
    input: z.object({ text: z.string() }),
  },
);

export const watch = stream(async function* () {
  for await (const ev of db.todos.changes()) yield ev;
});

// ── Private helpers (server-only, never reach the wire) ────────────
function shapeBody(t: string) { return t.trim(); }
export async function buildSummary(rows: unknown[]) { /* ... */ }
export const PI = 3.14;
```

## Why this shape (and the breaking change)

Before ISS-02, server modules were discovered purely by path: `src/server.{ts,tsx,js,jsx}` or anything under `src/server/**`. Inside such a file, **every** export became an RPC. This made `export * from "./_helpers"` silently publish every helper as a public endpoint with no warning. See ISS-02 in `ISSUES.md` for the full footgun analysis.

The new shape has two opt-in points (file directive + wrapper marker), so the wire surface is decidable from the AST alone: the build refuses to register anything the source didn't explicitly mark.

## Wrapper variants

| Wrapper | Implies `kind` | Notes |
| --- | --- | --- |
| `procedure(handler, config?)` | (none — see below) | Generic marker. Kind comes from the name-based heuristic (`get*` / `list*` / `find*` / `search*` / `count*` / `read*` / `fetch*` → `query`, async generator → `stream`, default → `mutation`). |
| `query(handler, config?)` | `query` | Read-only procedures. |
| `mutation(handler, config?)` | `mutation` | Side-effecting procedures. |
| `stream(handler, config?)` | `stream` | Async-generator procedures. The handler must be a generator (`async function*` or any async iterable factory). |
| `subscription(handler, config?)` | `subscription` | Long-lived event stream. Same wire shape as `stream` until the subscription wire fully ships. |

The optional `config` arg accepts the same fields as the legacy `<fn>.config = { ... }` assignment (`id`, `input`, `output`, `auth`, `rateLimit`, `idempotent`, etc.). The legacy assignment shape still works alongside wrappers.

## Migrating an existing app

For apps that used the old `src/server.{ts,...}` / `src/server/**` path convention:

1. Add `"use server";` as the first line of every server file (the directive is what opts the file in now).
2. Wrap each procedure in `procedure()` / `query()` / `mutation()` / `stream()` / `subscription()`. The simplest mechanical migration is `export async function name(args) { ... }` → `export const name = procedure(async (args) => { ... });` — name-based kind inference still works for queries.
3. Anything you DON'T wrap stays in the server bundle but never reaches the wire — this is the win.

Files at the legacy server-module path that lack the directive emit a one-time migration warning per file (so HMR doesn't spam) pointing at this page.

## Wrapper resolution rules

The transform resolves wrapper identity via a per-file static symbol table:

- Only **named imports** from `@zeroship/server` or `@zeroship/rpc` count: `import { procedure, query as q } from "@zeroship/server"`. Aliased imports work (`q(...)`).
- **Namespace imports** (`import * as zs`) and **default imports** are not recognized — wrappers must be bare-Identifier callees so the symbol table is decidable from the AST alone.
- Imports from any other package (even a re-export of `@zeroship/server`) are NOT recognized. This keeps the rule simple and tooling-friendly.

If you need a custom wrapper helper (e.g., one that records an audit log before delegating to `procedure`), make it return a `procedure(...)` call: the inner `procedure(...)` is what the transform sees.

## See also

- `docs/proposals/rpc.md` — the complete RPC design (wire shape, wireId derivation, manifest emission)
- `docs/reference/vite-environment-api.md` — Vite's Environment API and the V8 dev runtime
- `ISSUES.md` — historical ISS-02 entry (closed) for the path-convention footgun
