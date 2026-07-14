/**
 * Cut 2 — the GENERATED schema source (record `op.*` migrations → genArtifacts).
 *
 * The generated front-end records each `.ts` migration into an IR envelope
 * (pure-JS recorder, no CLI) and folds the envelopes through the Rust
 * `genArtifacts` verb → the inline `const schema = { … } as const` `env.db.ts`
 * literal + a valid v1 `schema.runtime.json` carrying the 7 injected system
 * fields + system indexes. These tests run the LIBRARY path in-process — no
 * subprocess is ever spawned.
 *
 * The db-hitcounter example is the golden proof: regenerating its committed
 * artifacts from its migrations reproduces them byte-identically (`--check`
 * clean).
 */

import { test, describe } from "node:test";
import assert from "node:assert/strict";
import { promises as fs } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { randomUUID } from "node:crypto";
import Module from "node:module";

import {
  genTypesFromMigrations,
  ENV_DB_FILE,
  RUNTIME_DESCRIPTOR_FILE,
} from "../../src/gen-types/index.js";

/** The db-hitcounter example — the golden generated app. */
const HITCOUNTER = resolve(import.meta.dirname, "../../../../examples/db-hitcounter");

/** The 7 platform system fields the producer injects into every collection. */
const SYSTEM_FIELDS = [
  "id",
  "created_at",
  "updated_at",
  "created_by",
  "updated_by",
  "version",
  "deleted_at",
] as const;

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

/** v1 RuntimeSchemaDescriptor contract (mirrors install-schema.ts:155-233). */
function assertRuntimeDescriptorV1(descriptor: unknown): void {
  assert.ok(descriptor && typeof descriptor === "object", "descriptor is an object");
  const d = descriptor as Record<string, unknown>;
  assert.equal(d.version, 1, "version === 1");
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
  up() {
    table("hits").create({
      columns: {
        path: t.text().notNull(),
      },
    });
  },
};
`;

describe("Cut 2 — generated schema source (record → genArtifacts)", () => {
  test("records a migration → valid v1 descriptor + all 7 system fields, no subprocess", async () => {
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
      assert.deepEqual([...res.files], [ENV_DB_FILE, RUNTIME_DESCRIPTOR_FILE]);

      const json = JSON.parse(await fs.readFile(join(outDir, RUNTIME_DESCRIPTOR_FILE), "utf8"));
      assertRuntimeDescriptorV1(json);
      assert.ok(json.collections.hits, "hits collection present");
      for (const sys of SYSTEM_FIELDS) {
        assert.ok(json.collections.hits.fields[sys], `hits.${sys} injected`);
      }
      assert.equal(json.collections.hits.fields.path.type, "string", "author path field");

      // System indexes injected (updated_at / created_by / deleted_at).
      const idxNames: string[] = json.collections.hits.indexes.map((i: { name: string }) => i.name);
      assert.ok(idxNames.includes("hits_updated_at_idx"), "system index injected");

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

  test("db-hitcounter regenerates BYTE-IDENTICAL to its committed artifacts (--check clean)", async () => {
    // The proof: the committed generated/zeroship artifacts are exactly what the
    // in-process emitter produces from the migrations. `--check` regenerates in
    // memory and diffs against the committed files; a byte drift throws.
    const outDir = join(HITCOUNTER, "generated/zeroship");
    const migDir = join(HITCOUNTER, "migrations");
    const res = await genTypesFromMigrations(migDir, outDir, { check: true });
    assert.equal(res.status, "checked", "committed db-hitcounter artifacts reproduce from migrations");
  });
});
