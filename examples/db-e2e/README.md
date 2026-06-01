# db-e2e

Server-only `@zeroship/db` demo for the full SQLite-backed surface area.

## What it covers

- CRUD: `insert`, `insertMany`, `get`, `find`, `first`, `unique`, `update`, `upsert`, soft `delete`, `restore`, `purge`
- Query: filters, sort, `limit`, `skip`, `after`, `count`, `distinct`, `aggregate`
- Relations: `with({ ownerId: true, workspaceId: true })`
- System fields: `created_at`, `updated_at`, `created_by`, `updated_by`, `version`, `deleted_at`
- Security: `t.encrypted`, masking, `MaskedValue.canUnmask`, row unmask, bulk unmask, per-query unmask hints
- Transactions: `db.transaction(...)`, rollback, nested savepoints, `TxCollection` / `TxQuery`
- Search: FTS, vector search, geo `near`
- Realtime: `db.live(...)` streamed over `/__zeroship/v1/liveTasks`

## Build

From the repo root:

```bash
pnpm install --frozen-lockfile
pnpm build
cargo build -p zeroship
pnpm --dir examples/db-e2e build
```

## Run the demo server manually

The encrypted fields use the `db_e2e` key id, so the runtime needs a root key:

```bash
cd examples/db-e2e
export ZEROSHIP_COLUMN_KEY_DB_E2E=$(printf 'a%.0s' {1..64})
DATABASE_URL=sqlite:.zeroship/dev.sqlite \
  ../../target/debug/zeroship serve dist/server/index.js --port 3000
```

Then hit the RPC endpoints under `http://127.0.0.1:3000/__zeroship/v1/*`.

## End-to-end suite

The repo-level runner builds the SDKs, builds this example, boots the real runtime
against `sqlite:.zeroship/dev.sqlite`, and executes the assertion harness:

```bash
./tests/e2e_db_sqlite.sh
```
