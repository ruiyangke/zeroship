# zeroship app

A zeroship app with database (SQLite by default in dev), file storage, and key-value state
wired up out of the box.

## Get started

```bash
pnpm install
pnpm dev
```

Production builds that ship migrations run the gen-types generated-artifact check. Keep
`zeroship-migrate-js` on PATH, or set `ZEROSHIP_MIGRATE_JS_BIN=/path/to/zeroship-migrate-js`.

- **http://localhost:5173** — your app
- **.zeroship/** — local dev state (SQLite data, uploaded files). Git-ignored.
- **src/index.ts** — server functions (marked `"use server"`). React UI calls
  these like regular functions; the plugin turns them into RPC.
- **migrations/** — committed op.* `.ts` schema migrations; this is the schema source.
- **generated/zeroship/env.db.ts** — generated `env.db` typing from the migration fold.
- **generated/zeroship/schema.runtime.json** — generated runtime descriptor shipped in the bundle.
- **src/App.tsx** — React client.

App code does not export a schema object. Add or change tables by editing
`migrations/*.ts`; gen-types records those files in the sandbox and folds the
transient IR in memory. There is no committed `.ir.json` sibling. Regenerate/check
the generated artifacts through the Vite plugin build or `zeroship-migrate-js
gen-types`.

## What's wired

| SDK | Where it comes from | What it does |
|---|---|---|
| `@zeroship/db` | `env.db` | Typed CRUD over the app database |
| `@zeroship/server` | build-time/server bundle | Server wrapper helpers used by the Vite transform |
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
