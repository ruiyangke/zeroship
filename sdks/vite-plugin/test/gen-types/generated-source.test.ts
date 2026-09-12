// Record migrations and render runtime descriptors through the native library.

import { test, describe } from "node:test";
import assert from "node:assert/strict";
import { assertPolicyColumns } from "./_policy-fixture.js";
import { promises as fs } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { randomUUID } from "node:crypto";
import Module from "node:module";

import {
  genTypesFromMigrations,
  ENV_DB_FILE,
  MIGRATIONS_IR_FILE,
  RUNTIME_DESCRIPTOR_FILE,
} from "../../src/gen-types/index.js";

/** The db-hitcounter example — the golden generated app. */
const HITCOUNTER = resolve(import.meta.dirname, "../../../../examples/db-hitcounter");

/** The scaffold every new creator app is stamped from. */
const SCAFFOLD = resolve(import.meta.dirname, "../../../create-zeroship-app/template");

async function makeFixture(
  files: Record<string, string>,
): Promise<{ root: string; cleanup: () => Promise<void> }> {
  const root = join(tmpdir(), `gt-gen-${randomUUID()}`);
  await fs.mkdir(root, { recursive: true });
  for (const [rel, content] of Object.entries(files)) {
    const abs = resolve(root, rel);
    await fs.mkdir(dirname(abs), { recursive: true });
    await fs.writeFile(abs, content);
  }
  return { root, cleanup: () => fs.rm(root, { recursive: true, force: true }) };
}

/** v2 RuntimeSchemaDescriptor contract (mirrors install-schema.ts:155-233). */
function assertRuntimeDescriptorV2(descriptor: unknown): void {
  assert.ok(descriptor && typeof descriptor === "object", "descriptor is an object");
  const d = descriptor as Record<string, unknown>;
  assert.equal(d.version, 2, "version === 2");
  assert.ok(d.collections && typeof d.collections === "object", "collections object");
  for (const [name, rawColl] of Object.entries(d.collections as Record<string, unknown>)) {
    const coll = rawColl as Record<string, unknown>;
    assert.ok(
      coll.fields && typeof coll.fields === "object" && !Array.isArray(coll.fields),
      `collection ${name} has object fields`,
    );
    for (const [fname, field] of Object.entries(coll.fields as Record<string, unknown>)) {
      assert.ok(
        field && typeof field === "object" && typeof (field as { type?: unknown }).type === "string",
        `field ${name}.${fname} has string type`,
      );
    }
    const opts = coll.options as Record<string, unknown>;
    assert.ok(opts && typeof opts === "object", `collection ${name} has options`);
    assert.equal(typeof opts.softDelete, "boolean", `${name}.options.softDelete boolean`);
    assert.equal(typeof opts.versioning, "boolean", `${name}.options.versioning boolean`);
    assert.ok(Array.isArray(coll.indexes), `collection ${name} has array indexes`);
  }
}

const CREATE_HITS = `
import { table, t } from "@zeroship/migrate";

export default {
  name: "create_hits",
  schema() {
    table("hits").create({
      columns: {
        path: t.text().notNull(),
        counter: t.bigInt().notNull(),
      },
    });
  },
};
`;

test("encrypted migration fields carry their logical type into generated SDK builders", async () => {
  const migration = `
import { table, t } from "@zeroship/migrate";
export default {
  name: "create_secrets",
  schema() {
    table("secrets").create({ columns: {
      message: t.encrypted({ of: t.text() }),
      amount: t.encrypted({ of: t.int() }),
      payload: t.encrypted({ of: t.bytes() }),
    }});
  },
};`;
  const fx = await makeFixture({ "migrations/20260711000000_create_secrets.ts": migration });
  const outDir = join(fx.root, "generated/zeroship");
  try {
    await genTypesFromMigrations(join(fx.root, "migrations"), outDir, {});
    const descriptor = JSON.parse(await fs.readFile(join(outDir, RUNTIME_DESCRIPTOR_FILE), "utf8"));
    for (const [field, type] of [["message", "string"], ["amount", "number"], ["payload", "bytes"]]) {
      const def = descriptor.collections.secrets.fields[field];
      assert.equal(def.type, type);
      assert.equal(def.encrypted, true);
      assert.equal(def.filterable, false);
      assert.equal(def.sortable, false);
    }
    const source = await fs.readFile(join(outDir, ENV_DB_FILE), "utf8");
    assert.match(source, /message: t\.encrypted\(\)/);
    assert.match(source, /amount: t\.encrypted\(\{ of: t\.number\(\) \}\)/);
    assert.match(source, /payload: t\.encrypted\(\{ of: t\.bytes\(\) \}\)/);
  } finally {
    await fx.cleanup();
  }
});

describe("generated schema source (record -> genArtifacts)", () => {
  test("records a migration with policy assignments without a migration subprocess", async () => {
    const fx = await makeFixture({ "migrations/20260711000000_create_hits.ts": CREATE_HITS });
    const outDir = join(fx.root, "generated/zeroship");

    // Trip-wire: the generated path must NOT shell any `zeroship-migrate`-family
    // CLI (the deleted subprocess seam). The recorder legitimately drives esbuild,
    // which runs its own bundler service subprocess — that is an in-process build
    // dependency, not the migrate CLI — so the guard fires only on a migrate binary.
    const cp = Module.createRequire(import.meta.url)("node:child_process") as Record<string, unknown>;
    const spies: Array<[string, unknown]> = [];
    for (const fn of ["spawn", "spawnSync", "exec", "execFile", "execFileSync", "execSync"]) {
      const original = cp[fn] as (...a: unknown[]) => unknown;
      spies.push([fn, original]);
      cp[fn] = (...args: unknown[]) => {
        const cmd = String(args[0] ?? "");
        if (/zeroship-migrate/.test(cmd)) {
          throw new Error(`gen-types generated path shelled the deleted migrate CLI via child_process.${fn}(${cmd})`);
        }
        return original.apply(cp, args);
      };
    }
    try {
      const res = await genTypesFromMigrations(join(fx.root, "migrations"), outDir, {});
      assert.equal(res.status, "written");
      assert.deepEqual([...res.files], [
        ENV_DB_FILE,
        RUNTIME_DESCRIPTOR_FILE,
        MIGRATIONS_IR_FILE,
      ]);

      const json = JSON.parse(await fs.readFile(join(outDir, RUNTIME_DESCRIPTOR_FILE), "utf8"));
      assertRuntimeDescriptorV2(json);
      assert.ok(json.collections.hits, "hits collection present");
      assertPolicyColumns(json.collections.hits.fields);
      assert.equal(json.collections.hits.fields.path.type, "string", "author path field");

      // Policy indexes survive generation.
      const idxNames: string[] = json.collections.hits.indexes.map((i: { name: string }) => i.name);
      assert.ok(idxNames.includes("hits_updated_at_idx"), "policy index injected");

      // env.db.ts is the inline generated literal (NOT the manual augmentation).
      const envDb = await fs.readFile(join(outDir, ENV_DB_FILE), "utf8");
      assert.match(envDb, /const schema = \{/, "generated env.db.ts has the inline schema literal");
      assert.match(envDb, /DO NOT EDIT/, "carries the DO NOT EDIT banner");
      assert.match(envDb, /Db<typeof schema>/, "augments Env.db off the inline schema");
    } finally {
      for (const [fn, original] of spies) cp[fn] = original;
      await fx.cleanup();
    }
  });

  test("the generated env.db.ts imports ONLY runtime deps a scaffolded app has", async () => {
    // The generated artifact is committed into the creator's app and typechecked
    // by their tsc. Every specifier it names must therefore resolve from the
    // scaffold's own dependencies. A schema-toolchain-internal package (the
    // migration engine, say) would typecheck here and break there, so pin the
    // whole import set rather than spot-checking one specifier.
    const fx = await makeFixture({ "migrations/20260711000000_create_hits.ts": CREATE_HITS });
    const outDir = join(fx.root, "generated/zeroship");
    try {
      await genTypesFromMigrations(join(fx.root, "migrations"), outDir, {});
      const envDb = await fs.readFile(join(outDir, ENV_DB_FILE), "utf8");

      // Every `from "..."` / bare `import "..."` specifier in the emitted module.
      assert.match(envDb, /counter: t\.bigInt\(\)\.required\(\)/);

      const specifiers = new Set<string>();
      for (const m of envDb.matchAll(/\bfrom\s+"([^"]+)"/g)) specifiers.add(m[1]!);
      for (const m of envDb.matchAll(/\bimport\s+"([^"]+)"/g)) specifiers.add(m[1]!);

      assert.deepEqual(
        [...specifiers].sort(),
        ["@zeroship/db"],
        "generated env.db.ts imports exactly @zeroship/db",
      );
    } finally {
      await fx.cleanup();
    }
  });

  test("db-hitcounter regenerates BYTE-IDENTICAL to its committed artifacts (--check clean)", async () => {
    // The proof: the committed generated/zeroship artifacts are exactly what the
    // in-process emitter produces from the migrations. `--check` regenerates in
    // memory and diffs against the committed files; a byte drift throws.
    const outDir = join(HITCOUNTER, "generated/zeroship");
    const migDir = join(HITCOUNTER, "migrations");
    const res = await genTypesFromMigrations(migDir, outDir, { check: true });
    assert.equal(res.status, "checked", "committed db-hitcounter artifacts reproduce from migrations");
  });

  test("the scaffold template's committed artifacts regenerate BYTE-IDENTICAL too", async () => {
    // The template ships `generated/zeroship/*` so a freshly scaffolded app
    // typechecks before its first `pnpm dev`. Nothing regenerates them for us -
    // they are committed by hand whenever the template's migrations change - so
    // without this they drift silently and every new app starts from a schema
    // surface that does not match its own migrations. db-hitcounter's golden
    // does not cover it: different migrations, and an example nobody stamps.
    const res = await genTypesFromMigrations(
      join(SCAFFOLD, "migrations"),
      join(SCAFFOLD, "generated/zeroship"),
      { check: true },
    );
    assert.equal(res.status, "checked", "committed scaffold artifacts reproduce from migrations");
  });
});
