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

## Client manifest

The SSR bundle reads hashed asset filenames from the Vite client manifest. Two pieces wire this up:

1. `vite.config.ts` enables `build.manifest: true`, so Vite writes `dist/.vite/manifest.json`.
2. `src/server.ts` does `import clientManifest from "virtual:zeroship/client-manifest";` — a virtual module the `@zeroship/vite-plugin` exposes during the SSR build. It inlines `dist/.vite/manifest.json` (read off disk after the client build's `writeBundle` finishes) into the SSR bundle as a `Record<string, ManifestChunk>`.

Then `clientScriptTag(ENTRY_SRC)` looks up `entry.file` and emits `<script type="module" src="/<hash>.js">`. CSS imports come through `entry.css`.

Type declarations: a triple-slash `<reference types="@zeroship/vite-plugin/types" />` at the top of `vite.config.ts` brings the `virtual:zeroship/client-manifest` module declaration into TypeScript's lookup.

## Known limitations (gaps surfaced)

### Gap — SSR build copies `public/` assets

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
