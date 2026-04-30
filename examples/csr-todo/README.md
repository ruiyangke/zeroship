# csr-todo (CSR demo)

A minimal SPA that exercises the **client-side rendering** path through the zeroship build pipeline.

- **Frontend**: React 18, vite-bundled, mounted at `<div id="root">` in `/index.html`.
- **Backend**: one server function (`listTodos`) registered automatically via the `"use server"` transform.
- **Routing**: a hand-rolled tiny router in `src/main.tsx` — every URL the client doesn't recognise falls through to `/about` or back to `/`. The gateway's `Match::Any → Static{ try: ["$path", "/index.html"] }` rule serves the SPA shell so the browser can take over.

## Build

```bash
npm install
npx vite build
```

This produces:

```
dist/
├── index.html
├── favicon.svg
├── assets/
│   └── *.js / *.css         (hashed Vite chunks)
├── server/
│   └── index.js             (the bundled "use server" exports + RPC bootstrap)
└── app.zsapp                (the deploy artifact)
```

## Inspect the manifest

```bash
mkdir -p /tmp/csr && tar -xf dist/app.zsapp -C /tmp/csr 2>/dev/null \
  || (zstd -dc dist/app.zsapp | tar -xC /tmp/csr)
jq . /tmp/csr/manifest.json
```

You should see (rules excerpt):

```json
[
  { "match": { "kind": "prefix", "path": "/assets/" },
    "action": { "kind": "static", "try": ["$path"], "cache": { "max_age": 31536000, "immutable": true } } },
  { "match": { "kind": "prefix", "method": "POST", "path": "/_rpc/" },
    "action": { "kind": "worker", "mode": "rpc" } },
  { "match": { "kind": "any" },
    "action": { "kind": "worker", "mode": "ssr" } }
]
```

`worker` is non-null with `entry: "index.js"` and a single module — the bundled server file.

## Deploy

```bash
zeroship deploy ./dist/app.zsapp --app=<uuid> --control=<url> --key=<master>
```

## Notes

- The RPC URL is `POST /_rpc/src/server/listTodos`, not `POST /_rpc/listTodos`. The vite-plugin's transform synthesises method names as `<modulePath>/<exportName>` so two unrelated server modules can have an export with the same name without collision. Client-side calls just use the imported `listTodos()` symbol — the wire path is opaque.
- The trailing **catch-all rule** is `worker(ssr)`, not `static`, because the build pipeline knows you have a worker. The worker's bootstrap (provided by `@zeroship/vite-plugin`'s `server-bootstrap.js`) returns 404 for non-RPC paths, so the gateway's behaviour for unknown URLs is "ask the worker, which 404s." For a truly static SPA-fallback, you'd want `Any → Static{ try: ["$path", "/index.html"] }` — see "Known limitations" below.

## Known limitations (gaps surfaced)

### Gap 1 — `Any → Worker(ssr)` catch-all on a CSR-only app

The current `@zeroship/vite-plugin` (v0.3.0) emits `Any → Worker(ssr)` whenever a server bundle exists. For pure CSR apps that's wrong: the catch-all should be `Any → Static{ try: ["$path", "/index.html"] }` so unknown URLs serve the SPA shell instead of hitting the worker. The worker's bootstrap then 404s on every non-RPC path — the SPA never loads on a real GET to `/about`.

The fix is in `sdks/vite-plugin/src/zsapp.ts`'s `buildRules()` — if the worker has no `default.fetch` (i.e. it only handles RPC), the catch-all should be the static-fallback rule, with a `Any → Worker(rpc)` rule before it for `POST /_rpc/`.

### Gap 2 — `public/` assets get duplicated into `dist/server/`

When the SSR build runs (kicked off automatically by the plugin), Vite's default `publicDir` behaviour copies `public/*` into the SSR build's output directory (`dist/server/`). Those files then end up cataloged as `worker.modules` entries in the deploy manifest — see the example output above where `favicon.ico` lives in `worker.modules` next to `index.js`.

The fix is to set `publicDir: false` on the SSR sub-build inside `sdks/vite-plugin/src/build.ts:viteBuild({...})`. The deduplication in `zsapp.ts` keeps both references pointing at the same blob hash so it's harmless on the wire, but the worker's module map should not list static-only files.
