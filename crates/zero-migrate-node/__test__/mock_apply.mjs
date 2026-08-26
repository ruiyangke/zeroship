// Behavioral JS gate: drive `status` (the journal-read async path) through the
// REAL napi TSFN fire-and-resolve bridge with a canned JS host driver. Proves the
// cross-thread oneshot wakeup reaches the reactor-less block_on THROUGH the real
// N-API boundary under Node (the §B.3 feasibility hinge, empirically), and that the
// Promise resolves with the correct journal outcome.
import { createRequire } from 'module';
const require = createRequire(import.meta.url);
const addon = require('../index.js');
function noInjectCharterToml(schema) {
  const scope = `{ include = [${JSON.stringify(schema)}] }`;
  return `policy_version = 1

[[grant]]
key = "schema.cross_schema"
value = true
scope = ${scope}

[[grant]]
key = "schema.create_table"
value = true
scope = ${scope}

[[grant]]
key = "schema.rename"
value = true
scope = ${scope}

[[grant]]
key = "safety.destructive_ops"
value = "allow"
scope = "all"
`;
}

const recorded = [];
// napi delivers the (request, done) tuple as a SINGLE array arg.
function hostDriver([request, done]) {
  recorded.push(`${request.kind}: ${request.sql}`);
  // The non-waiting project-lock acquisition asks for a row back; everything else
  // this canned driver answers is satisfied by an empty result.
  const rows = request.sql.includes('pg_try_advisory_lock')
    ? [{ columns: ['got'], cells: [{ kind: 'bool', bool: true }] }]
    : [];
  // A real event-loop turn so the wakeup is genuinely cross-thread + deferred
  // (mirrors the B.5 spike's setTimeout(…,0) ordering variant).
  setTimeout(() => done(null, { rows, rowCount: 1 }), 0);
}

const watchdog = setTimeout(() => {
  console.error('FAIL: async bridge hung (Promise never settled)');
  process.exit(1);
}, 10000);

// Typed verb boundary: `status` takes a `StatusRequest` object
// and RESOLVES a typed `StatusReply` — no JSON string, `currentVersion` camelCase.
const status = await addon.status(hostDriver, {
  projectId: 'prj_js',
  projectSchema: 'proj_js',
  dialect: 'postgres',
  migrations: [],
  charterLayers: [noInjectCharterToml('proj_js')],
});
clearTimeout(watchdog);

let ok = true;
function check(cond, msg) { if (!cond) { console.error('FAIL:', msg); ok = false; } }

check(recorded.length > 0, 'host driver was never called (TSFN bridge did not fire)');
check(
  recorded.some(s => s.includes('schema_migrations') || s.includes('union_all')),
  'journal read never reached the host driver'
);
check(status.currentVersion === null || status.currentVersion === undefined, `expected null currentVersion on empty journal, got ${status.currentVersion}`);
check(Array.isArray(status.applied) && status.applied.length === 0, 'applied should be empty on empty journal');

const mysqlRecorded = [];
const int = (value) => ({ kind: 'int', int: value });
const text = (value) => ({ kind: 'text', text: value });
const row = (columns, cells) => ({ columns, cells });

function mysqlHostDriver([request, done]) {
  mysqlRecorded.push(`${request.kind}: ${request.sql}`);
  let rows = [];
  if (request.sql.includes('performance_schema.events_transactions_current')) {
    rows = [row(
      ['transaction_tracking_enabled', 'in_transaction'],
      [int(1), int(0)],
    )];
  } else if (request.sql.includes('GET_LOCK')) {
    // Both MySQL named-lock acquisitions: the journal bootstrap's bounded
    // `GET_LOCK(?, ?)` and the read-only path's non-waiting `GET_LOCK(?, 0)`.
    rows = [row(['got'], [int(1)])];
  } else if (
    request.sql.includes('COLLATION_NAME AS collation_name') &&
    request.sql.includes('schema_migrations_inflight')
  ) {
    // Answer with exactly the columns the probe asked for, read back out of the probe.
    // The engine refuses to finish the journal bootstrap when any identity column is
    // absent from this result, so a hand-written list here stops covering a table the
    // moment one is added on the Rust side. That is not hypothetical: this arm kept
    // answering for eight columns after the rollback marker took the count to ten, and
    // the only reason it went unnoticed is that the compiled addon under test is a
    // build artifact the repository does not carry.
    const requested = [...request.sql.matchAll(
      /TABLE_NAME = '([^']+)'\s+AND COLUMN_NAME IN \(([^)]+)\)/g,
    )].flatMap(([, table, columns]) =>
      [...columns.matchAll(/'([^']+)'/g)].map(([, column]) => [table, column]),
    );
    check(
      requested.length > 0,
      'the identity-column probe was recognised but no (table, column) pair parsed out of it',
    );
    rows = requested.map(([table, column]) => row(
      ['table_name', 'column_name', 'character_set_name', 'collation_name'],
      [text(table), text(column), text('utf8mb4'), text('utf8mb4_bin')],
    ));
  }
  setTimeout(() => done(null, { rows, rowCount: rows.length }), 0);
}

const mysqlStatus = await addon.status(mysqlHostDriver, {
  projectId: 'prj_mysql_js',
  projectSchema: 'proj_mysql_js',
  dialect: 'mysql',
  migrations: [],
  charterLayers: [noInjectCharterToml('proj_mysql_js')],
});
const mysqlSql = mysqlRecorded.join('\n');
check(mysqlRecorded.length > 0, 'MySQL status never reached the host driver');
check(
  mysqlSql.includes('performance_schema.events_transactions_current'),
  'MySQL status did not use the MySQL journal bootstrap',
);
check(mysqlSql.includes('GET_LOCK(?, ?)'), 'MySQL status did not use MySQL lock SQL');
// The project lock a read takes is the NON-WAITING one. The bounded form above is
// the journal bootstrap's, on a different lock name, so the needle has to name the
// timeout rather than the function.
check(
  mysqlSql.includes('GET_LOCK(?, 0)'),
  'MySQL status waited for the project lock instead of trying it',
);
check(mysqlSql.includes('COLLATE utf8mb4_bin'), 'MySQL status did not use MySQL journal SQL');
check(
  !/BEGIN ISOLATION LEVEL REPEATABLE READ READ ONLY|pg_advisory|pg_catalog|CREATE SCHEMA IF NOT EXISTS|\$1/.test(mysqlSql),
  'MySQL status emitted PostgreSQL SQL',
);
check(
  Array.isArray(mysqlStatus.applied) && mysqlStatus.applied.length === 0,
  'MySQL empty-journal status should have no applied migrations',
);

if (!ok) {
  console.error('PostgreSQL recorded:', recorded);
  console.error('MySQL recorded:', mysqlRecorded);
  process.exit(1);
}
console.log(`PASS: PostgreSQL and MySQL status drove the real napi fire-and-resolve bridge with dialect-native journal SQL`);
console.log('  sample verbs:', recorded.slice(0, 3).join(' | '));
