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
├── _empty.js               (1 KB inert chunk — see "Why a placeholder JS file" below)
├── about.html
├── docs/
│   └── intro.html
├── index.html
├── assets/                 (likely missing — no real chunks to emit)
└── app.zsapp               (the deploy artifact)
```

## Inspect the manifest

```bash
zstd -dc dist/app.zsapp | tar -xC /tmp/ssg
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

## Known limitations (gaps surfaced)

### Gap — `/_assets/` rule isn't emitted when no JS chunks exist

The vite-plugin's `buildRules()` in `zsapp.ts` only emits the `/_assets/` Static rule when at least one asset path starts with the configured prefix. For a pure-HTML SSG with zero JS chunks, the prefix rule is correctly skipped — but the demo has a `_empty.js` file living at the root of `dist/`, which gets mounted as `/index-<hash>.js` (or wherever Rollup put it) and isn't covered by any explicit rule. The catch-all `Any → Static{ try: ["$path", "/index.html"] }` happens to serve it (since `$path` resolves), but a stricter setup might want a per-asset rule.

### Why a placeholder JS file

Vite refuses to build with zero inputs — see `rollupOptions.input` in `vite.config.ts`. We feed it a one-line `vite.empty.js` so it has SOMETHING to emit. The chunk is never linked to and is dead weight (~30 bytes after compression), but it keeps the build from erroring out.

A cleaner approach would be a dedicated "static-only" mode in the vite-plugin that disables Rollup entirely and just walks `content/` directly. That's a future addition.

### Trailing-slash matching is exact-only

`Match::Exact` requires the path to be exactly equal. So `GET /about/` (with the trailing slash) won't match the rule for `/about`. The catch-all then resolves it via `$path` → `/about/`, which isn't an asset, then falls back to `/index.html`. Result: the home page renders for `/about/`. A future enhancement would emit two rules per route, or use `Match::Prefix` with normalization.

## Files

| Path | Role |
| ---- | ---- |
| `content/index.html` | Home — served at `/` |
| `content/about.html` | About — served at `/about` |
| `content/docs/intro.html` | Doc page — served at `/docs/intro` |
| `vite.config.ts` | Custom `ssgContentPlugin` copies `content/**` into `dist/` |
| `vite.empty.js` | One-line placeholder that satisfies Vite's `input` requirement |

## Deploy

```bash
zeroship deploy ./dist/app.zsapp --app=<uuid> --control=<url> --key=<master>
```

Cold start: instant. Per-request work: one blob fetch from the gateway's content-addressed cache. No V8.
