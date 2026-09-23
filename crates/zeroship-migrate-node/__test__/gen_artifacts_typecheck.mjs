// Regression gate for the schema-artifact code generator through the REAL N-API
// boundary. The generated env.db.ts is compiled against the real zero-migrate
// authoring package; a stale/non-existent helper therefore makes this test fail.
//
// TWO calls, because the corpus of spellings splits along the charter. The
// AUTHORED half declares its own keys and so can only render with nothing
// injected; the CONFINED half turns on the lifecycle options, whose generators
// exist only in the production injection shape. Both sources are typechecked.
import { execFileSync } from 'node:child_process';
import { mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { createRequire } from 'node:module';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const HERE = dirname(fileURLToPath(import.meta.url));
const REPO_ROOT = resolve(HERE, '../../..');
const AUTHORING_ROOT = join(REPO_ROOT, 'packages/zero-migrate');
const require = createRequire(import.meta.url);
const addon = require('../index.js');

// Two charters, because one cannot carry both halves of this gate.
//
// The AUTHORED spellings below declare their own keys - a UUID primary key, an
// auto-increment one, a TypeID one, two composite ones. An injecting charter
// forbids every one of them (`author_primary_key = "forbid"`) and owns the
// column name `id`, so the spelling corpus can only be rendered under a charter
// that injects nothing.
const NO_INJECT_CHARTER_TOML = 'policy_version = 1\n';

// The PRODUCTION schema-emit ceiling, composed exactly as
// `packages/vite-plugin/src/gen-types/confined-ceiling.ts` composes it: the
// document header plus the authored `[[inject]]` fragment. Read from the
// fragment itself rather than copied, so this gate cannot describe a platform
// shape the platform no longer has - the same bytes
// `crates/zeroship-migrate-node/tests/gen_artifacts_reserved_identifiers.rs`
// takes through `include_str!`.
const CONFINED_EMIT_CEILING_TOML =
  'policy_version = 1\n\n' +
  readFileSync(join(REPO_ROOT, 'policies/confined-system-shape.inject.toml'), 'utf8');

function assert(cond, msg) {
  if (!cond) {
    throw new Error(`FAIL: ${msg}`);
  }
}

// One final schema that exercises every spelling which previously regressed:
// UUID v4/v7 defaults, integer auto-increment IDs, typed-ID prefixes, exact
// bigint literals, typed single-column references, and composite PK/FK arrays.
const envelope = {
  ir_version: addon.irVersion(),
  name: 'generated_schema_typecheck',
  ops: [
    {
      op: 'createTable',
      name: 'accounts',
      columns: [
        {
          name: 'id',
          type: 'uuid',
          nullable: false,
          default: { expr: { node: 'uuidV4' } },
        },
        {
          name: 'next_public_id',
          type: 'uuid',
          nullable: false,
          default: { expr: { node: 'uuidV7' } },
        },
        {
          name: 'exact_counter',
          type: 'bigInt',
          nullable: false,
          default: { literal: { value: { int64: '9007199254740993' } } },
        },
        {
          name: 'multiline_label',
          type: 'text',
          default: { literal: { value: 'line one\nline two' } },
        },
      ],
      primaryKey: ['id'],
      constraints: [],
      indexes: [],
    },
    {
      op: 'createTable',
      name: 'sequences',
      columns: [
        {
          name: 'id',
          type: 'bigInt',
          nullable: false,
          identity: { always: false },
        },
      ],
      primaryKey: ['id'],
      constraints: [],
      indexes: [],
    },
    {
      op: 'createTable',
      name: 'users',
      columns: [
        {
          name: 'id',
          type: { string: { length: 36 } },
          nullable: false,
          idPrefix: 'member',
        },
        {
          name: 'account_id',
          type: 'uuid',
          nullable: false,
          references: {
            table: 'accounts',
            column: 'id',
            onDelete: 'cascade',
            onUpdate: 'restrict',
          },
        },
      ],
      primaryKey: ['id'],
      constraints: [],
      indexes: [],
    },
    {
      op: 'createTable',
      name: 'locales',
      columns: [
        { name: 'tenant_id', type: 'text', nullable: false },
        { name: 'locale', type: 'text', nullable: false },
      ],
      primaryKey: ['tenant_id', 'locale'],
      constraints: [],
      indexes: [],
    },
    {
      op: 'createTable',
      name: 'pages',
      columns: [
        { name: 'tenant_id', type: 'text', nullable: false },
        { name: 'locale', type: 'text', nullable: false },
        { name: 'slug', type: 'text', nullable: false },
      ],
      primaryKey: ['tenant_id', 'slug'],
      constraints: [
        {
          name: 'pages_locale_fkey',
          kind: {
            kind: 'fk',
            columns: ['tenant_id', 'locale'],
            referencesTable: 'locales',
            referencesColumns: ['tenant_id', 'locale'],
            onDelete: 'cascade',
            onUpdate: 'noAction',
          },
        },
      ],
      indexes: [
        {
          name: 'pages_slug_idx',
          columns: [{ kind: 'column', name: 'slug' }],
        },
      ],
      // `strictness` is the only runtime option a no-inject charter can satisfy.
      // `softDelete` and `versioning` each REQUIRE exactly one assignment
      // generator, and generators are charter data - an injected column's
      // `assign = { by = "now", on = "delete" }`, never a column this envelope
      // could declare. Declaring them here would declare an option nothing in
      // this call can satisfy; they are exercised under the production ceiling
      // in the confined half below.
      runtimeOptions: { softDelete: false, versioning: false, strictness: 'lenient' },
    },
  ],
};

const reply = addon.genArtifacts({
  envelopes: [envelope],
  dialect: 'postgres',
  charterLayers: [NO_INJECT_CHARTER_TOML],
});
assert(reply.ok, `genArtifacts succeeds: ${reply.error ?? 'unknown error'}`);
const source = reply.envDbTs;
assert(typeof source === 'string', 'genArtifacts returns envDbTs source');

// Keep explicit sentinels alongside tsc so this gate identifies the removed
// spellings directly even if a future ambient declaration accidentally widens t.
assert(!/\bt\s*\.\s*id\s*\(/.test(source), 'generated source never calls removed t.id()');
assert(!/\bt\s*\[\s*["']id["']\s*\]\s*\(/.test(source), 'generated source never calls removed t["id"]()');
assert(!/\bt\s*\.\s*ref\s*\(/.test(source), 'generated source never calls removed t.ref()');

assert(
  /satisfies\s+Record\s*<\s*string\s*,\s*CreateTableArgs\s*>/.test(source),
  'generated source is checked as a passive CreateTableArgs schema map',
);
assert(/t\.uuid\(\)\s*\.primaryKey\(\)\s*\.default\(uuidV4\(\)\)/.test(source), 'renders the UUID-v4 primary-key composition');
assert(/\.default\(uuidV7\(\)\)/.test(source), 'renders UUID-v7 defaults with uuidV7()');
assert(/t\.bigInt\(\)\s*\.primaryKey\(\)\s*\.autoIncrement\(\)/.test(source), 'renders the integer-ID composition');
assert(/t\.typedId\("member"\)\s*\.primaryKey\(\)/.test(source), 'renders the TypeID primary-key composition');
assert(source.includes('int64("9007199254740993")'), 'renders exact bigint literals with int64()');
assert(source.includes('.default("line one\\nline two")'), 'escapes control characters in string defaults');
assert(
  /\.references\(\s*"accounts"\s*,\s*"id"\s*,\s*\{\s*onDelete:\s*"cascade"\s*,\s*onUpdate:\s*"restrict"\s*\}\s*\)/.test(source),
  'renders the typed single-column reference chain',
);
assert(/primaryKey:\s*\[\s*"tenant_id"\s*,\s*"slug"\s*\]/.test(source), 'renders a composite primary-key array');
assert(source.includes('foreignKeys: ['), 'renders a composite foreign-key array');
assert(/name:\s*"pages_locale_fkey"/.test(source), 'preserves the composite foreign-key name');
assert(
  /references:\s*\{\s*table:\s*"locales"\s*,\s*columns:\s*\[\s*"tenant_id"\s*,\s*"locale"\s*\]\s*\}/.test(source),
  'preserves the composite foreign-key target columns',
);
assert(!/\btable\s*\(/.test(source), 'generated artifact does not execute a table lifecycle operation');

// The lifecycle half: the SAME renderer under the production schema-emit
// ceiling. A collection that turns `softDelete`/`versioning` on is refused
// unless exactly one injected assignment generates each - a delete-event `now`
// and a write-event `increment`. Those live in the charter, so this is the only
// charter under which the option spellings can be rendered at all, and the
// collection below therefore declares columns and nothing else.
const confinedEnvelope = {
  ir_version: addon.irVersion(),
  name: 'confined_lifecycle_typecheck',
  ops: [
    {
      op: 'createTable',
      name: 'articles',
      columns: [{ name: 'title', type: 'text', nullable: false }],
      primaryKey: null,
      constraints: [],
      indexes: [],
      runtimeOptions: { softDelete: true, versioning: true, strictness: 'strict' },
    },
  ],
};

const confinedReply = addon.genArtifacts({
  envelopes: [confinedEnvelope],
  dialect: 'postgres',
  charterLayers: [CONFINED_EMIT_CEILING_TOML],
});
assert(confinedReply.ok, `genArtifacts under the confined ceiling succeeds: ${confinedReply.error ?? 'unknown error'}`);
const confinedSource = confinedReply.envDbTs;
assert(typeof confinedSource === 'string', 'the confined call returns envDbTs source');

assert(
  /options:\s*\{\s*softDelete:\s*true\s*,\s*versioning:\s*true\s*\}/.test(confinedSource),
  'renders both lifecycle options once their generators are injected',
);
assert(
  /deleted_at:\s*t\.timestamp\(\)/.test(confinedSource),
  'renders the injected delete-event column the softDelete generator assigns',
);
assert(
  /version:\s*t\.int\(\)\s*\.required\(\)\s*\.default\(1\)/.test(confinedSource),
  'renders the injected write-event column the versioning generator increments',
);
// The generators reach the OTHER artifact of the same call as typed per-field
// facts, which is what the runtime reads; `env.db.ts` carries only the options.
const confinedRuntime = JSON.parse(confinedReply.runtimeJson);
const articleFields = confinedRuntime.collections.articles.fields;
assert(
  articleFields.deleted_at.assign?.by === 'now' && articleFields.deleted_at.assign?.on === 'delete',
  'preserves the delete-event generator on the descriptor field',
);
assert(articleFields.deleted_at.softDelete === true, 'marks the soft-delete field on the descriptor');
assert(
  articleFields.version.assign?.by === 'increment(1)' && articleFields.version.assign?.on === 'write',
  'preserves the write-event increment generator on the descriptor field',
);
assert(articleFields.version.concurrency === true, 'marks the concurrency field on the descriptor');

// Root the harness inside the real package tree, matching the package doc gates,
// so `import ... from "@zeroship/migrate"` resolves its built public declarations.
const dir = mkdtempSync(join(AUTHORING_ROOT, 'node_modules', '.codegen-gate-'));
try {
  writeFileSync(join(dir, 'env.db.ts'), source, 'utf8');
  writeFileSync(join(dir, 'env.db.confined.ts'), confinedSource, 'utf8');
  const tsconfig = {
    extends: resolve(AUTHORING_ROOT, 'tsconfig.json'),
    compilerOptions: {
      noEmit: true,
      rootDir: dir,
      types: [],
    },
    include: ['env.db.ts', 'env.db.confined.ts'],
  };
  const configPath = join(dir, 'tsconfig.json');
  writeFileSync(configPath, JSON.stringify(tsconfig), 'utf8');

  try {
    const tsc = resolve(
      AUTHORING_ROOT,
      process.platform === 'win32' ? 'node_modules/.bin/tsc.cmd' : 'node_modules/.bin/tsc',
    );
    execFileSync(tsc, ['--noEmit', '-p', configPath], {
      cwd: AUTHORING_ROOT,
      encoding: 'utf8',
      stdio: 'pipe',
    });
  } catch (error) {
    const diagnostics = `${error.stdout ?? ''}${error.stderr ?? ''}`;
    assert(
      false,
      `generated env.db.ts must typecheck against zero-migrate\n${diagnostics}` +
        `\n--- generated source ---\n${source}` +
        `\n--- generated source (confined ceiling) ---\n${confinedSource}`,
    );
  }
} finally {
  rmSync(dir, { recursive: true, force: true });
}

console.log('PASS: generated env.db.ts typechecks against the real @zeroship/migrate authoring package');
