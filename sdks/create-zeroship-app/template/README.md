# zeroship app

A zeroship app with database (SQLite by default in dev), file storage, and key-value state
wired up out of the box.

## Get started

```bash
pnpm install
pnpm dev
```

- **http://localhost:5173** — your app
- **.zeroship/** — local dev state (SQLite data, uploaded files). Git-ignored.
- **src/index.ts** — server functions (marked `"use server"`). React UI calls
  these like regular functions; the plugin turns them into RPC.
- **migrations/** — committed op.* schema migrations.
- **generated/zeroship/env.db.ts** — generated `env.db` typing from those migrations.
- **src/App.tsx** — React client.

## What's wired

| SDK | Where it comes from | What it does |
|---|---|---|
| `@zeroship/db` | `env.db` | Typed CRUD over the app database |
| `@zeroship/storage` | `env.storage` | File uploads / object storage |
| `@zeroship/kv` | `env.kv` | Ephemeral key-value state, sessions, counters |

The `"use server"` directive at the top of `src/index.ts` opts the file into
RPC discovery. Wrapped exports (`query`, `mutation`, `action`, `stream`) become
server functions. You can call those directly from client code; the Vite plugin
converts the imports into RPC calls.

## Deploy (coming soon)

```bash
zeroship deploy
```

Will bundle the app, deploy to the platform, and return a `*.zeroship.app`
URL. (In progress — see the zeroship roadmap.)
