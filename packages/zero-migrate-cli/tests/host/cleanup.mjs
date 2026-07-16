import pg from 'pg';
const c = new pg.Client({connectionString:'postgres://postgres:zero_migrate@localhost:5440/zero_migrate_test'});
await c.connect();
const r = await c.query(`SELECT nspname FROM pg_namespace WHERE nspname LIKE 'host_oracle_%' OR nspname LIKE 'native_oracle_%' OR nspname LIKE 'status_oracle_%'`);
for (const row of r.rows) { await c.query(`DROP SCHEMA IF EXISTS "${row.nspname}" CASCADE`); }
console.log('dropped', r.rows.length, 'leaked oracle schemas');
await c.end();
