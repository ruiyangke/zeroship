// Drops the schemas `oracle.ts` leaks. Plain ESM so it runs under bare `node`, which
// is why it repeats the DSN `live-db.ts` (a `.ts` module) resolves.
import pg from 'pg';
const PG_URL =
  process.env.ZERO_MIGRATE_TEST_PG_URL ||
  'postgres://postgres:postgres@127.0.0.1:5434/zero_migrate_test';
const c = new pg.Client({connectionString: PG_URL});
await c.connect();
const r = await c.query(`SELECT nspname FROM pg_namespace WHERE nspname LIKE 'host_oracle_%' OR nspname LIKE 'native_oracle_%' OR nspname LIKE 'status_oracle_%'`);
for (const row of r.rows) { await c.query(`DROP SCHEMA IF EXISTS "${row.nspname}" CASCADE`); }
console.log('dropped', r.rows.length, 'leaked oracle schemas');
await c.end();
