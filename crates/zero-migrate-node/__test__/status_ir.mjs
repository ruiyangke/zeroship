// Plan-aware status through the real N-API boundary with a canned PostgreSQL
// session. Pins the public detail fields and the lock-bracketed snapshot order.
import { createRequire } from 'module';

const require = createRequire(import.meta.url);
const addon = require('../index.js');
const NO_INJECT_CHARTER_TOML = `policy_version = 1

[[grant]]
key = "schema.cross_schema"
value = true
scope = "all"

[[grant]]
key = "schema.create_table"
value = true
scope = "all"

[[grant]]
key = "schema.rename"
value = true
scope = "all"

[[grant]]
key = "safety.destructive_ops"
value = "allow"
scope = "all"
`;

const COMPLETED = 'mig_0000000000000000000001';
const INFLIGHT = 'mig_0000000000000000000002';
const ROLLED_BACK = 'mig_0000000000000000000003';
const PENDING_CONTRACT = 'mig_0000000000000000000004';
const PLAN_VERSION = 'mig_0000000000000000000005';
const recorded = [];

const text = (value) => ({ kind: 'text', text: value });
const bool = (value) => ({ kind: 'bool', bool: value });
// The journal sequence is an int8 column, so a host driver hands it back as a
// string cell rather than risking a float.
const int = (value) => ({ kind: 'int', intStr: String(value) });
const row = (columns, cells) => ({ columns, cells });

function hostDriver([request, done]) {
  recorded.push(request.sql);
  let rows = [];

  if (request.sql.includes('pg_try_advisory_lock')) {
    rows = [row(['got'], [bool(true)])];
  } else if (request.sql.includes("c.relname = 'schema_backfills'")) {
    rows = [row(['table_exists', 'checksum_exists'], [bool(false), bool(false)])];
  } else if (request.sql.includes('union_all')) {
    rows = [
      row(
        ['version', 'checksum', 'mig_kind', 'event_seq', 'phase', 'down'],
        [text(COMPLETED), text('checksum-completed'), text('apply'), int(1), text('completed'), { kind: 'null' }],
      ),
      row(
        ['version', 'checksum', 'mig_kind', 'event_seq', 'phase', 'down'],
        [text(INFLIGHT), text('checksum-inflight'), { kind: 'null' }, int(2), text('started'), { kind: 'null' }],
      ),
    ];
  } else if (
    request.sql.includes('schema_migrations') &&
    request.sql.includes("event_kind = 'rolled_back'")
  ) {
    rows = [
      row(
        ['version', 'name', 'checksum', 'actor', 'exec_ms', 'at'],
        [
          text(ROLLED_BACK),
          text('rolled back migration'),
          text('checksum-rolled-back'),
          text('operator'),
          { kind: 'null' },
          text('2026-07-15T00:00:00.000000+00:00'),
        ],
      ),
    ];
  } else if (
    request.sql.includes('schema_pending_contracts') &&
    request.sql.includes("WHERE state = 'resolved'")
  ) {
    rows = [];
  } else if (request.sql.includes('schema_pending_contracts')) {
    rows = [
      row(
        [
          'pending_version',
          'plan_version',
          'owner_app',
          'table',
          'from_col',
          'to_col',
          'ty',
          'contract_versions',
        ],
        [
          text(PENDING_CONTRACT),
          text(PLAN_VERSION),
          text('app_status_js'),
          text('widgets'),
          text('old_name'),
          text('new_name'),
          text('text'),
          text('[]'),
        ],
      ),
    ];
  }

  setTimeout(() => done(null, { rows, rowCount: rows.length }), 0);
}

const status = await addon.statusIr(hostDriver, {
  ownerApp: 'app_status_js',
  projectSchema: 'proj_status_js',
  dialect: 'postgres',
  registry: {},
  envelopes: [],
  charterLayers: [NO_INJECT_CHARTER_TOML],
});

function assert(condition, message) {
  if (!condition) throw new Error(message);
}

assert(status.rolledBack.length === 1 && status.rolledBack[0] === ROLLED_BACK,
  `rolledBack was not preserved: ${JSON.stringify(status.rolledBack)}`);
assert(status.pendingContracts.length === 1,
  `pendingContracts missing: ${JSON.stringify(status.pendingContracts)}`);
assert(status.pendingContracts[0].pendingVersion === PENDING_CONTRACT,
  'pending-contract identity was not projected');
assert(status.pendingContracts[0].orphaned === true,
  'contract whose plan is absent should be orphaned');
assert(Array.isArray(status.blocked) && status.blocked.length === 0,
  `blocked detail missing: ${JSON.stringify(status.blocked)}`);
assert(status.unexpectedJournal.length === 2,
  `unexpected journal entries missing: ${JSON.stringify(status.unexpectedJournal)}`);
assert(status.unexpectedJournal[0].version === COMPLETED && status.unexpectedJournal[0].state === 'applied',
  'unexpected completed entry was not projected');
assert(status.unexpectedJournal[1].version === INFLIGHT && status.unexpectedJournal[1].state === 'inflight',
  'unexpected inflight entry was not projected');

const orderedStatus = await addon.statusIr(hostDriver, {
  ownerApp: 'app_status_ordered',
  projectSchema: 'app_status_ordered',
  dialect: 'postgres',
  registry: {},
  charterLayers: [NO_INJECT_CHARTER_TOML],
  envelopes: [
    {
      ir_version: addon.irVersion(),
      name: 'create_status_widgets',
      ops: [{
        op: 'createTable',
        name: 'status_widgets',
        columns: [{ name: 'payload', type: 'json' }],
        primaryKey: null,
        constraints: [],
        indexes: [],
      }],
    },
    {
      ir_version: addon.irVersion(),
      name: 'default_status_widgets_payload',
      ops: [{
        op: 'setColumnDefault',
        table: 'status_widgets',
        column: 'payload',
        value: { container: 'object' },
      }],
    },
  ],
});
assert(orderedStatus.plans.length === 2,
  `ordered envelope plans missing: ${JSON.stringify(orderedStatus.plans)}`);
assert(orderedStatus.plans.every((plan) => plan.state === 'pending'),
  `fresh ordered envelope plans should be pending: ${JSON.stringify(orderedStatus.plans)}`);

// The acquisition is the NON-WAITING pg_try_advisory_lock: a status read must not
// sit behind a deploy that holds the lock for its whole run. The three orderings
// are unchanged; only the spelling of the acquisition moved, and the blocking
// spelling is asserted absent so a regression cannot pass on a shared substring.
const lock = recorded.findIndex((sql) => sql.includes('pg_try_advisory_lock'));
const catalogRead = recorded.findIndex((sql) => sql.includes('FROM pg_class child'));
const snapshotRead = recorded.findIndex((sql) => sql.includes('union_all'));
const unlock = recorded.findIndex((sql) => sql.includes('pg_advisory_unlock'));
assert(catalogRead >= 0, 'live catalog snapshot was not read');
assert(lock >= 0 && lock < catalogRead, 'project lock must precede live catalog reads');
assert(lock >= 0 && lock < snapshotRead, 'project lock must precede snapshot reads');
assert(unlock > snapshotRead, 'project unlock must follow snapshot reads');
assert(
  !recorded.some((sql) => sql.includes('SELECT pg_advisory_lock')),
  'a status read must never take the unbounded acquisition a deploy takes',
);

// A contended acquisition reads NOTHING and comes back as a first-class busy
// reply, not an error: the reads are composite and unbracketed, so a reader that
// went ahead without the lock would see a live deploy's halfway state as drift.
const contended = [];
function contendedDriver([request, done]) {
  contended.push(request.sql);
  let rows = [];
  if (request.sql.includes('pg_try_advisory_lock')) {
    rows = [row(['got'], [bool(false)])];
  } else if (request.sql.includes('pg_stat_activity')) {
    rows = [
      row(
        ['pid', 'application_name', 'state', 'query'],
        [int(4242), text('zero-migrate'), text('active'), text('CREATE INDEX CONCURRENTLY ix')],
      ),
    ];
  }
  setTimeout(() => done(null, { rows, rowCount: rows.length }), 0);
}

const busy = await addon.statusIr(contendedDriver, {
  ownerApp: 'app_status_busy',
  projectSchema: 'app_status_busy',
  dialect: 'postgres',
  registry: {},
  envelopes: [],
  charterLayers: [NO_INJECT_CHARTER_TOML],
  readOnly: true,
});
assert(busy.busy === true, `a contended status must report busy: ${JSON.stringify(busy)}`);
assert(busy.lockHolders.length === 1 && busy.lockHolders[0].pid === 4242,
  `the busy reply must name the holder: ${JSON.stringify(busy.lockHolders)}`);
assert(busy.pending.length === 0 && busy.applied.length === 0,
  'a busy reply reconciles nothing');
for (const forbidden of ['FROM pg_class child', 'union_all', 'pg_advisory_unlock']) {
  assert(!contended.some((sql) => sql.includes(forbidden)),
    `a contended status must not run ${forbidden}: ${JSON.stringify(contended)}`);
}
assert(contended.filter((sql) => sql.includes('pg_try_advisory_lock')).length === 3,
  `the retry is bounded at three attempts, never a loop: ${JSON.stringify(contended)}`);

console.log('PASS: statusIr preserves plan-aware details and brackets one coherent snapshot');
