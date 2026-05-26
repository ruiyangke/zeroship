# examples/

Each subdirectory here is a small app that exercises the zeroship build pipeline (`@zeroship/vite-plugin` → `.zship` → control plane → BlobStore → gateway) end-to-end. They double as integration tests: if all three build cleanly and produce the right manifest shape, the pipeline is healthy.

## Three rendering modes, three demos

| Demo | Mode | What it shows | Worker? |
| ---- | ---- | ------------- | ------- |
| [`csr-todo/`](./csr-todo/) | CSR (SPA) | One React component, browser-side state, one RPC endpoint | yes (RPC only) |
| [`ssr-blog/`](./ssr-blog/) | SSR | HTML rendered per-request in V8, hydrated client-side | yes (RPC + SSR) |
| [`ssg-docs/`](./ssg-docs/) | SSG | Three prerendered HTML pages, no JS at runtime | no |

Each demo's README explains:

- **What it shows** — the rendering mode and the file shape it produces.
- **How to build** — `npm install && npx vite build`.
- **The expected manifest shape** — the rules + worker fields the build emits.
- **How to deploy** — `zeroship deploy ./dist/app.zship ...`.
- **Known limitations** — gaps in the platform that this demo surfaces.

## When to use which mode

- **CSR** — your app is a tool/dashboard/console. Fast iteration, no SEO needs. The browser does the rendering work.
- **SSR** — your app needs first-paint performance OR SEO. The worker renders per-request. Good for blogs, marketplaces, social feeds.
- **SSG** — your content rarely changes. Documentation, marketing sites, blogs with infrequent updates. Cheapest to run; static bytes through the gateway with no V8 cold start.

## Build all three

```bash
for d in csr-todo ssr-blog ssg-docs; do
  (cd examples/$d && npm install && npx vite build)
done
```

After each, `dist/app.zship` is the deploy artifact. Inspect with:

```bash
zstd -dc examples/<demo>/dist/app.zship | tar -tf - | head           # manifest first
zstd -dc examples/<demo>/dist/app.zship | tar -xf - -O manifest.json | jq .
```

## Other examples in this directory

The `*.js` files are older framework-less examples that predate the
`.zship` format and exist for legacy CLI testing (`zeroship serve <file>.js`).

Platform SDK examples:

| Demo | What it shows |
| ---- | ------------- |
| [`kv-dashboard/`](./kv-dashboard/) | `@zeroship/kv` JSON values, TTL, counters, leases, namespaced list, and cleanup |
| [`db-todos/`](./db-todos/) | `@zeroship/db` schema discovery, relations, RPC procedures, and live snapshots |
| [`db-chat/`](./db-chat/) | reactive DB queries and broker-driven updates |
| [`db-migrations-playground/`](./db-migrations-playground/) | online data backfills with `@zeroship/migrations` |
