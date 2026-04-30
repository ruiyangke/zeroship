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
    "action": { "kind": "static", "try": ["$path", "/index.html"] } }
]
```

`worker` is non-null with `entry: "index.js"` and a single module — the bundled server file. The catch-all is `Static`, not `Worker(ssr)`, because the server bundle has no user-defined `default.fetch` (only RPC handlers via `"use server"`).

## Deploy

```bash
zeroship deploy ./dist/app.zsapp --app=<uuid> --control=<url> --key=<master>
```

## Notes

- The RPC URL is `POST /_rpc/src/server/listTodos`, not `POST /_rpc/listTodos`. The vite-plugin's transform synthesises method names as `<modulePath>/<exportName>` so two unrelated server modules can have an export with the same name without collision. Client-side calls just use the imported `listTodos()` symbol — the wire path is opaque.
- The trailing **catch-all rule** is `Static{ try: ["$path", "/index.html"] }`, not `Worker(ssr)`, because the build pipeline detects that `src/server.ts` exports no `default.fetch` (only `"use server"` RPC handlers). Unknown URLs serve the SPA shell so the browser router can claim them.

## Known limitations (gaps surfaced)

### Gap — `public/` assets get duplicated into `dist/server/`

When the SSR build runs (kicked off automatically by the plugin), Vite's default `publicDir` behaviour copies `public/*` into the SSR build's output directory (`dist/server/`). Those files then end up cataloged as `worker.modules` entries in the deploy manifest — see the example output above where `favicon.ico` lives in `worker.modules` next to `index.js`.

The fix is to set `publicDir: false` on the SSR sub-build inside `sdks/vite-plugin/src/build.ts:viteBuild({...})`. The deduplication in `zsapp.ts` keeps both references pointing at the same blob hash so it's harmless on the wire, but the worker's module map should not list static-only files.
