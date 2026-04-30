# ssr-blog (SSR demo)

A 3-post React blog where the **server** renders HTML on each request and the **client** hydrates after. Demonstrates the SSR shape — what the deploy artifact should look like for a real per-request rendering setup — but read **Known limitations** below first: the current build pipeline can't actually serve SSR end-to-end yet.

## Build

```bash
npm install
npx vite build
```

Output:

```
dist/
├── index.html
├── _assets/                  (or `assets/` — see Vite output)
│   └── *.js / *.css
├── .vite/manifest.json       (client manifest — see gap 1 below)
├── server/
│   └── index.js              (the SSR bundle + RPC bootstrap)
└── app.zsapp                 (the deploy artifact)
```

## Inspect the manifest

```bash
zstd -dc dist/app.zsapp | tar -xC /tmp/ssr
jq . /tmp/ssr/manifest.json
```

The shape we want:

```json
{
  "rules": [
    { "match": { "kind": "prefix", "path": "/assets/" },
      "action": { "kind": "static", "try": ["$path"], "cache": { "max_age": 31536000, "immutable": true } } },
    { "match": { "kind": "prefix", "method": "POST", "path": "/_rpc/" },
      "action": { "kind": "worker", "mode": "rpc" } },
    { "match": { "kind": "any" },
      "action": { "kind": "worker", "mode": "ssr" } }
  ],
  "worker": { "entry": "index.js", "modules": { "index.js": "<hash>" } }
}
```

## Known limitations (gaps surfaced)

These are the real reasons SSR doesn't work end-to-end today. Each is a fix in the platform, not in this demo.

### Gap 1 — The Vite client manifest never reaches the SSR bundle

In a working SSR pipeline, the SSR bundle imports the Vite client manifest at build time so it knows which hashed `_assets/main-<hash>.js` filename to inject into the rendered HTML. Vite already emits it at `dist/.vite/manifest.json` when `build.manifest: true` is set in vite.config (which this demo does), but:

1. There's no virtual import path for the SSR bundle to ask for it. A real pipeline would either:
   - Copy `dist/.vite/manifest.json` to a stable import path (e.g. `node_modules/.zeroship/manifest.json`) before the SSR build runs, OR
   - Expose a Vite virtual module like `virtual:zeroship/client-manifest` that the SSR bundle can `import`.
2. `sdks/vite-plugin/src/zsapp.ts:484` explicitly skips `.vite/` when collecting files, so even the file-on-disk path can't survive the deploy.

In this demo, `src/entry-server.tsx` hardcodes the entry-client filename as a placeholder; real SSR would read it from the manifest.

### Gap 2 — User-defined `default.fetch` collides with the appended RPC bootstrap

The vite-plugin always appends `sdks/vite-plugin/src/server-bootstrap.js` to the SSR bundle. That bootstrap exports its own `export default { fetch }` (which only handles `/_rpc/*` and 404s on everything else). If user code in `entry-server.tsx` also exports `export default`, the bundle has two `export default` statements — an ESM syntax error — and the build fails.

This is the load-bearing gap. Until the build pipeline:

- Skips appending the bootstrap when the user's bundle already defines a `default.fetch`, OR
- Wraps the user's `default.fetch` so the bootstrap delegates to it for non-RPC requests,

…SSR via `default.fetch` is impossible.

**Workaround in this demo**: `src/server.ts` exports a single `"use server"` named function `ssrRender(url)` that returns the rendered HTML as JSON. Calling it via `POST /_rpc/src/server/ssrRender` works, but every `GET /` and `GET /post/:id` would land at the appended bootstrap's catch-all 404. So the manifest's `Any → Worker(ssr)` rule is correct in shape but dead in practice.

`src/entry-server.tsx` is included in the source tree but **not built into the bundle** — it documents what the file would look like once gap (2) is closed.

### Gap 3 — SSR build copies `public/` assets

Same as the CSR demo — Vite's SSR build with the current plugin config copies `public/*` into `dist/server/`, so harmless static assets (e.g. `favicon.ico`) end up referenced as worker modules in the manifest. Cosmetic but confusing.

## Files

| Path | Role |
| ---- | ---- |
| `index.html` | Shell with `<!--app-html-->` placeholder for the SSR'd body |
| `src/entry-client.tsx` | Hydrates the SSR'd HTML with `hydrateRoot` |
| `src/entry-server.tsx` | The intended SSR entry — **not used by the build today** (gap 2) |
| `src/server.ts` | Workaround entry: exposes `ssrRender(url)` as a "use server" RPC method |
| `src/components/App.tsx` | Top-level component picking PostList or Post by URL |
| `src/components/PostList.tsx` | The 3-post list page |
| `src/components/Post.tsx` | Single-post view with prev/next nav |
| `src/components/posts.ts` | The 3 hardcoded posts |

## Deploy

Once gaps (1) and (2) are closed:

```bash
zeroship deploy ./dist/app.zsapp --app=<uuid> --control=<url> --key=<master>
```

Until then, the build produces a valid `.zsapp` archive with the right manifest shape, but the worker will only respond to `POST /_rpc/src/server/ssrRender`.
