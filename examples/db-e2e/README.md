# db-e2e

Server-only `@zeroship/db` demo for the full SQLite-backed surface area.

## What it covers

- CRUD: `insert`, `insertMany`, `get`, `find`, `first`, `unique`, `update`, `upsert`, soft `delete`, `restore`, `purge`
- Query: filters, sort, `limit`, `skip`, `after`, `count`, `distinct`, `aggregate`
- Relations: `with({ owner: true, workspace: true })`
- Generated lifecycle fields: `created_at`, `updated_at`, `created_by`, `updated_by`, `version`, `deleted_at`
- Security: `t.encrypted`, masking, `MaskedValue.canUnmask`, row unmask, bulk unmask, per-query unmask hints
- Transactions: `db.transaction(...)`, rollback, nested savepoints, `TxCollection` / `TxQuery`
- Search: vector search, geo `near`
- Realtime: `db.live(...)` streamed over `/__zeroship/v1/liveTasks`

## Build

From the repo root:

```bash
pnpm install --frozen-lockfile
pnpm build
cargo build -p zeroship-cli
pnpm --dir examples/db-e2e build
```

## Run the demo server manually

Start the server with a file-backed database:

```bash
cd examples/db-e2e
DATABASE_URL=sqlite:.zeroship/dev.sqlite \
  ../../target/debug/zeroship serve dist/server/index.js --port 3000
```

Then hit the RPC endpoints under `http://127.0.0.1:3000/__zeroship/v1/*`.

## End-to-end suite

Vitest owns the fixture and assertions in `tests/`. It builds the SDKs and Rust
runtime, copies the app into a disposable workspace directory, applies its
migrations to a SQLite file, and starts Vite and the runtime on allocated ports.
Chromium exercises the page and RPC proxy; TypeScript tests exercise the database
contract. The fixture removes its own app copy and processes when the run ends.

```bash
pnpm --dir examples/db-e2e test
```

Install workspace dependencies first. Use the repository's Rust and Node
toolchains and Chromium from the development environment, or install it with
`pnpm --dir examples/db-e2e exec playwright install chromium`.
`PLAYWRIGHT_CHROMIUM_EXECUTABLE_PATH` selects an explicit browser executable.
Service logs and failure screenshots remain under `tests/.artifacts/`.
An unavailable runtime, browser, or failed migration fails the suite.

The local host keeps its project encryption key in
`.zeroship/private/project-data-key.json`, alongside the local runtime state.
Keep that private file with database backups. The fixture owns a disposable
project directory and key. Deployed workers receive their project key from
control; column keys are never supplied through environment variables.
