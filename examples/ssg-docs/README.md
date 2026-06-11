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

### Trailing slashes normalize to the canonical resource

`GET /about/` (with the trailing slash) resolves to the `/about` static
resource — it serves the about page, not the home page. The gateway
canonicalizes every request path *before* resource matching (the SEC-2
path-canonicalization step): a single trailing slash is stripped
(`/about/` → `/about`), so the literal `/about` resource is hit and its
`["/about.html"]` try-chain serves. The catch-all (`/[...rest]` → index)
is reached only for paths that match no declared resource.

The canonical form is also what the gateway forwards to the worker and
matches its auth policy against, so the trailing-slash normalization can
never desync the auth match from the forwarded path. Interior dot-segments
or empty segments (`/a/./b`, `/a//b`, `..`, `%2e`) are *rejected* (400),
not silently rewritten — only a lone trailing slash is normalized.

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
