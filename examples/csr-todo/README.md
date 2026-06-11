# csr-todo (CSR demo)

A minimal SPA that exercises the **client-side rendering** path through the zeroship build pipeline.

- **Frontend**: React 18, vite-bundled, mounted at `<div id="root">` in `/index.html`.
- **Backend**: two server functions (`listTodos`, `searchTodos`) registered automatically via the `"use server"` transform.
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
└── app.zship                (the deploy artifact)
```

## Inspect the manifest

```bash
mkdir -p /tmp/csr && tar -xf dist/app.zship -C /tmp/csr 2>/dev/null \
  || (zstd -dc dist/app.zship | tar -xC /tmp/csr)
jq . /tmp/csr/manifest.json
```

You should see (resources excerpt — v1 flat map shape):

```json
{
  "/assets/*": {
    "static": { "try": ["$path"] },
    "cache": { "max_age": 31536000, "immutable": true }
  },
  "rpc:listTodos": { "kind": "query" },
  "rpc:searchTodos": { "kind": "query" },
  "/[...rest]": {
    "static": { "try": ["$path", "/index.html"] }
  }
}
```

`worker` is non-null with `entry: "index.js"` and a single module — the bundled server file. The catch-all is a static fallback serving the SPA shell (`/index.html`), not a worker SSR entry, because the server bundle has no user-defined `default.fetch` (only RPC handlers via `"use server"`).

## Deploy

```bash
zeroship deploy ./dist/app.zship --app=<uuid> --control=<url> --key=<master>
```

## Notes

- The RPC URL is `POST /_rpc/src/server/listTodos`, not `POST /_rpc/listTodos`. The vite-plugin's transform synthesises method names as `<modulePath>/<exportName>` so two unrelated server modules can have an export with the same name without collision. Client-side calls just use the imported `listTodos()` symbol — the wire path is opaque.
- The trailing **catch-all rule** is `Static{ try: ["$path", "/index.html"] }`, not `Worker(ssr)`, because the build pipeline detects that `src/server.ts` exports no `default.fetch` (only `"use server"` RPC handlers). Unknown URLs serve the SPA shell so the browser router can claim them.

## Notes on the build output

`worker.modules` lists exactly `index.js` — no `favicon.ico` or other `public/` files. The vite-plugin's SSR sub-build runs with `publicDir: false`; the client build keeps `publicDir`, so `public/*` ends up in `dist/<root>/` and gets cataloged in `assets`.
