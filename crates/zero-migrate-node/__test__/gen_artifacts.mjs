// Behavioral JS gate for the sync, DB-free `genArtifacts` verb through the REAL
// napi boundary. Proves:
//   1. the GENERATED source (a RESOLVED create-table IR envelope — the shape the
//      recorder emits, with the 7 platform system columns + system indexes injected)
//      and the MANUAL source (the equivalent declared descriptor set, which the
//      producer resolves internally) emit BYTE-IDENTICAL `runtimeJson` + `envDbTs`;
//   2. the emitted `runtimeJson` parses + satisfies the v1 shape;
//   3. the emitted `envDbTs` is a real `.ts` module of `t.*()` builder calls;
//   4. the error arms fail soft (never throw) — both-arms + no-arm + malformed.
//
// NB on (1): `fold_to_field_defs` folds ops AS-AUTHORED — it does not itself inject
// system columns. The generated pipeline's recorder resolves the create-table
// (system columns + `id` PK + the deleted_at/updated_at/created_by system indexes)
// BEFORE the ops reach the fold; the manual producer (`descriptors_to_create_ops`)
// does the same resolution. So the generated envelope below carries the RESOLVED
// ops — exactly what makes the two sources "equivalent" and therefore byte-identical.
import { createRequire } from 'module';
const require = createRequire(import.meta.url);
const addon = require('../index.js');

function assert(cond, msg) {
  if (!cond) {
    console.error(`FAIL: ${msg}`);
    process.exit(1);
  }
}

// --- The GENERATED source: the RESOLVED create-table envelope (recorder shape). ---
const envelope = {
  ir_version: addon.irVersion(),
  name: 'create_widgets',
  ops: [
    {
      op: 'createTable',
      name: 'widgets',
      columns: [
        { name: 'id', type: 'text', nullable: false },
        { name: 'created_at', type: 'timestamp', nullable: false },
        { name: 'updated_at', type: 'timestamp', nullable: false },
        { name: 'created_by', type: 'text', nullable: true },
        { name: 'updated_by', type: 'text', nullable: true },
        { name: 'version', type: 'int', nullable: false },
        { name: 'deleted_at', type: 'timestamp', nullable: true },
        { name: 'label', type: 'text' },
        { name: 'count', type: 'int', nullable: false },
      ],
      primaryKey: ['id'],
      constraints: [],
      indexes: [
        { columns: [{ kind: 'column', name: 'deleted_at' }] },
        { columns: [{ kind: 'column', name: 'updated_at' }] },
        { columns: [{ kind: 'column', name: 'created_by' }] },
      ],
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

const gen = addon.genArtifacts({ envelopes: [envelope] });
assert(gen.ok, `generated source ok: ${gen.error}`);

const man = addon.genArtifacts({ descriptors: [descriptor] });
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
const SYS = ['id', 'created_at', 'updated_at', 'created_by', 'updated_by', 'version', 'deleted_at'];
for (const s of SYS) {
  assert(widgets.fields[s] && typeof widgets.fields[s].type === 'string', `system field ${s} present with string type`);
}
assert(widgets.fields.label.type === 'string', 'label field type string');
assert(widgets.fields.count.required === true, 'count field required');
assert(widgets.options.softDelete === false && widgets.options.strictness === 'strict', 'options block v1');
assert(Array.isArray(widgets.indexes), 'indexes is an array');

// (3) env.db.ts is a real .ts module of builder calls.
assert(gen.envDbTs.includes('import { t, schema as defineSchema, type Db } from "@zeroship/db";'), 'imports @zeroship/db');
assert(gen.envDbTs.includes('const schema = {'), 'has the schema const');
assert(gen.envDbTs.includes('label: t.string(),'), 'renders label builder chain');
assert(gen.envDbTs.includes('count: t.number().required(),'), 'renders count builder chain');
assert(gen.envDbTs.includes('declare module "zeroship" {'), 'carries the module augmentation');
assert(gen.envDbTs.includes('db: Db<typeof schema>;'), 'augments Env.db');
for (const s of SYS) {
  assert(!gen.envDbTs.includes(`${s}: t.`), `env.db.ts omits system field ${s}`);
}

// (4) error arms fail soft (never throw).
const both = addon.genArtifacts({ envelopes: [envelope], descriptors: [descriptor] });
assert(!both.ok && typeof both.error === 'string', 'both-arms is a soft error');

const neither = addon.genArtifacts({});
assert(!neither.ok && typeof neither.error === 'string', 'no-arm is a soft error');

const malformed = addon.genArtifacts({ envelopes: [{ ir_version: addon.irVersion(), ops: 'nope' }] });
assert(!malformed.ok && typeof malformed.error === 'string', 'malformed envelope is a soft error');

console.log('PASS: genArtifacts byte-identical + v1-shape + .ts-module + soft-error arms (through the real .node)');
