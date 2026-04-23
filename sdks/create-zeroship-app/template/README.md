# zeroship app

A zeroship app with database (PGlite in dev), file storage, and key-value cache
wired up out of the box.

## Get started

```bash
npm install
npm run dev
```

- **http://localhost:5173** — your app
- **.zeroship/** — local dev state (Postgres data, uploaded files). Git-ignored.
- **src/index.ts** — server functions (marked `"use server"`). React UI calls
  these like regular functions; the plugin turns them into RPC.
- **src/App.tsx** — React client.

## What's wired

| SDK | Where it comes from | What it does |
|---|---|---|
| `@zeroship/db` | `env.db` | Typed CRUD over Postgres |
| `@zeroship/storage` | `env.storage` | File uploads / object storage |
| `@zeroship/kv` | `env.kv` | In-memory cache, sessions, counters |

The `"use server"` directive at the top of `src/index.ts` marks every export
as a server function. You can call these directly from client code — the
vite-plugin converts client calls into RPC.

## Deploy (coming soon)

```bash
zeroship deploy
```

Will bundle the app, deploy to the platform, and return a `*.zeroship.app`
URL. (In progress — see the zeroship roadmap.)
