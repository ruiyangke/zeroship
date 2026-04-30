# ssr-blog (SSR demo)

A 3-post React blog where the **server** renders HTML on each request and the **client** hydrates after. Demonstrates the SSR shape — `src/server.ts` exports `default.fetch`, which the V8 worker invokes on every non-RPC GET, returning freshly-rendered HTML.

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

### Gap 2 — SSR build copies `public/` assets

Same as the CSR demo — Vite's SSR build with the current plugin config copies `public/*` into `dist/server/`, so harmless static assets (e.g. `favicon.ico`) end up referenced as worker modules in the manifest. Cosmetic but confusing.

## Files

| Path | Role |
| ---- | ---- |
| `index.html` | Dev-time entry; SSR builds inject the rendered HTML into a fresh shell |
| `src/entry-client.tsx` | Hydrates the SSR'd HTML with `hydrateRoot` |
| `src/entry-server.tsx` | Reference shape — **not used by the build**; `src/server.ts` is the wired-up entry |
| `src/server.ts` | SSR entry: `export default { fetch }`, returns rendered HTML on every GET |
| `src/components/App.tsx` | Top-level component picking PostList or Post by URL |
| `src/components/PostList.tsx` | The 3-post list page |
| `src/components/Post.tsx` | Single-post view with prev/next nav |
| `src/components/posts.ts` | The 3 hardcoded posts |

## Deploy

```bash
zeroship deploy ./dist/app.zsapp --app=<uuid> --control=<url> --key=<master>
```
