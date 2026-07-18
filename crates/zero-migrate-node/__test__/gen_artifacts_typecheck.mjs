// Regression gate for the schema-artifact code generator through the REAL N-API
// boundary. The generated env.db.ts is compiled against the real zero-migrate
// authoring package; a stale/non-existent helper therefore makes this test fail.
import { execFileSync } from 'node:child_process';
import { mkdtempSync, rmSync, writeFileSync } from 'node:fs';
import { createRequire } from 'node:module';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const HERE = dirname(fileURLToPath(import.meta.url));
const REPO_ROOT = resolve(HERE, '../../..');
const AUTHORING_ROOT = join(REPO_ROOT, 'packages/zero-migrate');
const require = createRequire(import.meta.url);
const addon = require('../index.js');
const NO_INJECT_CEILING_TOML = 'policy_version = 1\n';

function assert(cond, msg) {
  if (!cond) {
    throw new Error(`FAIL: ${msg}`);
  }
}

// One final schema that exercises every spelling which previously regressed:
// UUID v4/v7 defaults, integer auto-increment IDs, TypeID/ULID formats, exact
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
          type: 'text',
          nullable: false,
          valueFormat: { typeId: { prefix: 'usr' } },
        },
        {
          name: 'session_id',
          type: 'text',
          nullable: false,
          valueFormat: 'ulid',
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
      runtimeOptions: {
        softDelete: true,
        versioning: true,
        strictness: 'lenient',
      },
    },
  ],
};

const reply = addon.genArtifacts({
  envelopes: [envelope],
  policyCeilingToml: NO_INJECT_CEILING_TOML,
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
assert(/ids\.typeId\(\{\s*prefix:\s*"usr"\s*\}\)\s*\.primaryKey\(\)/.test(source), 'renders the TypeID primary-key composition');
assert(source.includes('ids.ulid()'), 'renders ULID columns with ids.ulid()');
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

// Root the harness inside the real package tree, matching the package doc gates,
// so `import ... from "zero-migrate"` resolves its built public declarations.
const dir = mkdtempSync(join(AUTHORING_ROOT, 'node_modules', '.codegen-gate-'));
try {
  writeFileSync(join(dir, 'env.db.ts'), source, 'utf8');
  const tsconfig = {
    extends: resolve(AUTHORING_ROOT, 'tsconfig.json'),
    compilerOptions: {
      noEmit: true,
      rootDir: dir,
      types: [],
    },
    include: ['env.db.ts'],
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
    assert(false, `generated env.db.ts must typecheck against zero-migrate\n${diagnostics}\n--- generated source ---\n${source}`);
  }
} finally {
  rmSync(dir, { recursive: true, force: true });
}

console.log('PASS: generated env.db.ts typechecks against the real zero-migrate authoring package');
