// Behavioral JS gate for the sync, DB-free `genArtifacts` verb through the REAL
// napi boundary. Proves:
//   1. the GENERATED source (a RAW author-only create-table IR envelope — the exact
//      shape the pure-JS recorder emits, with NO system columns) and the MANUAL
//      source (the equivalent declared descriptor set) emit BYTE-IDENTICAL
//      `runtimeJson` + `envDbTs`;
//   2. the emitted `runtimeJson` parses + satisfies the v1 shape (incl. the 7 system
//      fields the RESOLVE injects);
//   3. the emitted `envDbTs` is a real `.ts` module of `t.*()` builder calls;
//   4. the error arms fail soft (never throw) — both-arms + no-arm + malformed.
//
// NB on (1): the pure-JS recorder emits RAW author-only ops. `genArtifacts` resolves
// the confined system shape (the 7 system columns + `id` PK + the deleted_at/
// updated_at/created_by system indexes) via `resolve_create_table_policy` BEFORE the
// ops reach the fold; the manual producer (`descriptors_to_create_ops`) does the same
// resolution. So the RAW generated envelope below and the descriptor set both resolve
// to the SAME shape — which is what makes them byte-identical. This gate feeds RAW
// (unresolved) ops on purpose: the true guarantee that resolution is wired.
import { createRequire } from 'module';
const require = createRequire(import.meta.url);
const addon = require('../index.js');

function assert(cond, msg) {
  if (!cond) {
    console.error(`FAIL: ${msg}`);
    process.exit(1);
  }
}

// --- The confined schema-emit charter (a `RootCharter` TOML) both sources pass as
//     `charterLayers`. The engine bakes in NO confined preset: the caller supplies
//     the injection shape. Both sides pass the SAME charter ⇒ byte-identical output.
//     This mirrors the monorepo's bundled confined schema-emit charter (the 7 system
//     columns + [id] PK + 3 system indexes + the grants emission needs). ---
const CONFINED_CHARTER_TOML = `policy_version = 1

[[grant]]
key = "schema.cross_schema"
value = true
scope = { include = ["app"] }

[[grant]]
key = "schema.create_table"
value = true
scope = { include = ["app"] }

[[grant]]
key = "schema.rename"
value = true
scope = { include = ["app"] }

[[grant]]
key = "safety.destructive_ops"
value = "allow"
scope = "all"

[[inject]]
scope = "all"
mandatory = true
primary_key = ["id"]
author_primary_key = "forbid"
columns = [
  { name = "id",         type = "text",        nullable = false },
  { name = "created_at", type = "timestamptz", nullable = false },
  { name = "updated_at", type = "timestamptz", nullable = false },
  { name = "created_by", type = "text",        nullable = true  },
  { name = "updated_by", type = "text",        nullable = true  },
  { name = "version",    type = "integer",     nullable = false },
  { name = "deleted_at", type = "timestamptz", nullable = true  },
]
indexes = [
  { name = "ix_deleted_at", columns = ["deleted_at"] },
  { name = "ix_updated_at", columns = ["updated_at"] },
  { name = "ix_created_by", columns = ["created_by"] },
]
`;

// --- The GENERATED source: the RAW author-only create-table envelope (recorder shape:
//     author columns ONLY, no system fields, no author PK — genArtifacts resolves the
//     confined system shape before folding). ---
const envelope = {
  ir_version: addon.irVersion(),
  name: 'create_widgets',
  ops: [
    {
      op: 'createTable',
      name: 'widgets',
      columns: [
        { name: 'label', type: 'text' },
        { name: 'count', type: 'int', nullable: false },
      ],
      primaryKey: null,
      constraints: [],
      indexes: [],
      runtimeOptions: { softDelete: false, versioning: false, strictness: 'strict' },
    },
  ],
};

// --- The MANUAL source: the EQUIVALENT declared descriptor set (producer resolves). ---
const descriptor = {
  name: 'widgets',
  ownerApp: 'app_js',
  fields: [
    { name: 'label', type: 'string' },
    { name: 'count', type: 'int', required: true },
  ],
};

const gen = addon.genArtifacts({ envelopes: [envelope], charterLayers: [CONFINED_CHARTER_TOML] });
assert(gen.ok, `generated source ok: ${gen.error}`);

const man = addon.genArtifacts({ descriptors: [descriptor], charterLayers: [CONFINED_CHARTER_TOML] });
assert(man.ok, `manual source ok: ${man.error}`);

// (1) byte-identical runtimeJson + envDbTs.
assert(
  gen.runtimeJson === man.runtimeJson,
  `generated and manual runtimeJson must be BYTE-IDENTICAL\n--- gen ---\n${gen.runtimeJson}\n--- man ---\n${man.runtimeJson}`,
);
assert(gen.envDbTs === man.envDbTs, 'generated and manual envDbTs must be byte-identical');

// (2) v1 shape.
const desc = JSON.parse(gen.runtimeJson);
assert(desc.version === 1, 'runtime descriptor is v1');
const widgets = desc.collections.widgets;
assert(widgets && typeof widgets === 'object', 'widgets collection present');
const injectedColumnBlock = CONFINED_CHARTER_TOML.match(/columns = \[([\s\S]*?)\]\nindexes = \[/);
assert(injectedColumnBlock, 'test charter exposes an inject columns block');
const injectedFields = [...injectedColumnBlock[1].matchAll(/\bname\s*=\s*"([^"]+)"/g)].map(
  (match) => match[1],
);
assert(injectedFields.length > 0, 'test charter injects at least one field');
for (const s of injectedFields) {
  assert(widgets.fields[s] && typeof widgets.fields[s].type === 'string', `system field ${s} present with string type`);
}
assert(widgets.fields.label.type === 'string', 'label field type string');
assert(widgets.fields.count.required === true, 'count field required');
assert(widgets.options.softDelete === false && widgets.options.strictness === 'strict', 'options block v1');
assert(Array.isArray(widgets.indexes), 'indexes is an array');

// (3) env.db.ts is a passive schema map using the current authoring API.
assert(gen.envDbTs.includes('from "zero-migrate";'), 'imports zero-migrate');
assert(gen.envDbTs.includes('const schema = {'), 'has the schema const');
assert(gen.envDbTs.includes('label: t.text(),'), 'renders label builder chain');
assert(gen.envDbTs.includes('count: t.int().notNull(),'), 'renders count builder chain');
assert(gen.envDbTs.includes('satisfies Record<string, CreateTableArgs>'), 'checks table payloads against CreateTableArgs');
assert(gen.envDbTs.includes('export { schema };'), 'exports the passive schema map');
for (const s of injectedFields) {
  assert(gen.envDbTs.includes(`${s}:`), `env.db.ts renders resolved system field ${s}`);
}
assert(!gen.envDbTs.includes('t.id('), 'never emits removed t.id');
assert(!gen.envDbTs.includes('t["id"]'), 'never emits removed t[id]');
assert(!gen.envDbTs.includes('t.ref('), 'never emits removed t.ref');
assert(!gen.envDbTs.includes('.create('), 'never executes a lifecycle operation');

// (4) error arms fail soft (never throw).
const both = addon.genArtifacts({
  envelopes: [envelope],
  descriptors: [descriptor],
  charterLayers: [CONFINED_CHARTER_TOML],
});
assert(!both.ok && typeof both.error === 'string', 'both-arms is a soft error');

const neither = addon.genArtifacts({ charterLayers: [CONFINED_CHARTER_TOML] });
assert(!neither.ok && typeof neither.error === 'string', 'no-arm is a soft error');

const malformed = addon.genArtifacts({
  envelopes: [{ ir_version: addon.irVersion(), ops: 'nope' }],
  charterLayers: [CONFINED_CHARTER_TOML],
});
assert(!malformed.ok && typeof malformed.error === 'string', 'malformed envelope is a soft error');

const noCharters = addon.genArtifacts({ envelopes: [envelope], charterLayers: [] });
assert(
  !noCharters.ok && noCharters.error?.includes('at least one policy charter is required'),
  'an empty charterLayers list preserves the layered loader error',
);

console.log('PASS: genArtifacts byte-identical + v1-shape + current authoring schema + soft-error arms (through the real .node)');
