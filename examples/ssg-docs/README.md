# ssg-docs (SSG demo)

A 3-page documentation site whose every byte is prerendered HTML.

- **No worker**: the deploy manifest's `worker` field is omitted (= SSG-only).
- **No JavaScript at runtime**: every page is plain HTML + inline CSS.
- **No framework**: the source files in `content/` are hand-written HTML.

The point of this demo is to show what the smallest possible deploy looks like — and to verify that the build pipeline produces a `worker: null` manifest with sensible static rules when no `src/server.ts` is present.

## Build

```bash
npm install
npx vite build
```

Output:

```
dist/
├── about.html
├── docs/
│   └── intro.html
├── index.html
└── app.zship               (the deploy artifact)
```

`mode: "static"` in `vite.config.ts` skips the SSR sub-build entirely and injects a virtual stub Rollup input that gets deleted in `generateBundle`, so no `_empty-<hash>.js` placeholder ships in the artifact.

## Inspect the manifest

```bash
zstd -dc dist/app.zship | tar -xC /tmp/ssg
jq '.rules, .worker' /tmp/ssg/manifest.json
```

What you should see:

```json
[
  { "match": { "kind": "exact", "path": "/about" },
    "action": { "kind": "static", "try": ["/about.html"] } },
  { "match": { "kind": "exact", "path": "/docs/intro" },
    "action": { "kind": "static", "try": ["/docs/intro.html"] } },
  { "match": { "kind": "any" },
    "action": { "kind": "static", "try": ["$path", "/index.html"] } }
]
```

(Plus the `assets/`-prefix rule if Vite emitted any hashed chunks.)

`worker` is **absent** from the JSON entirely (the spec says `null`/missing means SSG-only, and the emitter omits the field rather than writing `null`).

## Notes

### Trailing-slash matching is exact-only

`Match::Exact` requires the path to be exactly equal. So `GET /about/` (with the trailing slash) won't match the rule for `/about`. The catch-all then resolves it via `$path` → `/about/`, which isn't an asset, then falls back to `/index.html`. Result: the home page renders for `/about/`. This is a gateway-side limitation, not a vite-plugin bug — a future enhancement would emit two rules per route, or use `Match::Prefix` with normalization.

## Files

| Path | Role |
| ---- | ---- |
| `content/index.html` | Home — served at `/` |
| `content/about.html` | About — served at `/about` |
| `content/docs/intro.html` | Doc page — served at `/docs/intro` |
| `vite.config.ts` | `ssgContentPlugin` copies `content/**` into `dist/`; `zeroship({ mode: "static" })` packs the result |

## Deploy

```bash
zeroship deploy ./dist/app.zship --app=<uuid> --control=<url> --key=<master>
```

Cold start: instant. Per-request work: one blob fetch from the gateway's content-addressed cache. No V8.
