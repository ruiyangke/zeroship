/**
 * Cut 1 — the MANUAL schema source (the new capability).
 *
 * A hand-written `schema.ts` (a map of collection → `@zeroship/db` builder)
 * evaluates → `CollectionDescriptorDto[]` → `genArtifacts({ descriptors })` →
 * `schema.runtime.json` (valid v1) + an AUGMENTATION `env.db.ts`. These tests run
 * the LIBRARY path in-process — no CLI, no subprocess.
 */

import { test, describe } from "node:test";
import assert from "node:assert/strict";
import { promises as fs } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { randomUUID } from "node:crypto";

import {
  genTypesFromSchemaFile,
  ENV_DB_FILE,
  RUNTIME_DESCRIPTOR_FILE,
} from "../../src/gen-types/index.js";
import { fieldDefToDto } from "../../src/gen-types/manual.js";

async function makeFixture(
  files: Record<string, string>,
): Promise<{ root: string; cleanup: () => Promise<void> }> {
  const root = join(tmpdir(), `gt-manual-${randomUUID()}`);
  await fs.mkdir(root, { recursive: true });
  for (const [rel, content] of Object.entries(files)) {
    const abs = resolve(root, rel);
    await fs.mkdir(dirname(abs), { recursive: true });
    await fs.writeFile(abs, content);
  }
  return { root, cleanup: () => fs.rm(root, { recursive: true, force: true }) };
}

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

/**
 * A minimal, self-contained re-statement of the v1 RuntimeSchemaDescriptor
 * contract validated at runtime by `@zeroship/bootstrap`
 * install-schema.ts:155-233. Throws on any violation.
 */
function assertRuntimeDescriptorV1(descriptor: unknown): void {
  assert.ok(descriptor && typeof descriptor === "object", "descriptor is an object");
  const d = descriptor as Record<string, unknown>;
  assert.equal(d.version, 1, "version === 1");
  assert.ok(d.collections && typeof d.collections === "object", "collections object");
  for (const [name, rawColl] of Object.entries(d.collections as Record<string, unknown>)) {
    assert.ok(rawColl && typeof rawColl === "object", `collection ${name} is an object`);
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
    if (opts.strictness !== undefined) {
      assert.ok(
        ["strict", "lenient", "off"].includes(opts.strictness as string),
        `${name}.options.strictness in enum`,
      );
    }
    assert.ok(Array.isArray(coll.indexes), `collection ${name} has array indexes`);
    for (const idx of coll.indexes as unknown[]) {
      const i = idx as Record<string, unknown>;
      assert.equal(typeof i.name, "string", "index has string name");
      assert.ok(
        Array.isArray(i.fields) && (i.fields as unknown[]).every((f) => typeof f === "string"),
        "index has string[] fields",
      );
    }
  }
}

const TWO_COLLECTION_SCHEMA = `
import { schema as defineSchema, t } from "@zeroship/db";

export const schema = {
  users: defineSchema({
    email: t.string().required().mask({ kind: "email", classification: "pii" }),
    secret: t.encrypted(),
    age: t.number(),
  })
    .softDelete()
    .index("users_email_idx", ["email"]),
  posts: defineSchema({
    id: t.id("post"),
    title: t.string().required(),
    authorId: t.ref("users"),
  }),
};
`;

describe("Cut 1 — manual schema source", () => {
  test("writes a valid v1 schema.runtime.json + an augmentation env.db.ts", async () => {
    const fx = await makeFixture({ "schema.ts": TWO_COLLECTION_SCHEMA });
    const outDir = join(fx.root, "generated/zeroship");
    try {
      const res = await genTypesFromSchemaFile(join(fx.root, "schema.ts"), outDir, {});
      assert.equal(res.status, "written");
      assert.deepEqual([...res.files], [ENV_DB_FILE, RUNTIME_DESCRIPTOR_FILE]);

      const json = JSON.parse(await fs.readFile(join(outDir, RUNTIME_DESCRIPTOR_FILE), "utf8"));
      assertRuntimeDescriptorV1(json);

      // Both collections present.
      assert.ok(json.collections.users, "users collection present");
      assert.ok(json.collections.posts, "posts collection present");

      // System fields injected into every collection.
      for (const coll of ["users", "posts"]) {
        for (const sys of SYSTEM_FIELDS) {
          assert.ok(json.collections[coll].fields[sys], `${coll}.${sys} injected`);
        }
      }

      // Author facets survived: mask, encrypted, id-prefix, ref.
      assert.ok(json.collections.users.fields.email.mask, "email carries mask facet");
      assert.ok(json.collections.users.fields.secret.encrypted, "secret carries encrypted facet");
      assert.equal(json.collections.posts.fields.id.idPrefix, "post", "post id carries prefix");
      assert.equal(json.collections.posts.fields.authorId.refTarget, "users", "ref target");

      // Options reflected.
      assert.equal(json.collections.users.options.softDelete, true, "softDelete honoured");

      // W2 (standalone `descriptors_to_create_ops`, commit 24038ed): author-declared
      // named indexes now SURVIVE the manual producer path — they are carried through
      // `descriptors_to_create_ops` into the CreateTable op, alongside the injected
      // system indexes. (The earlier Step-1 gap that dropped them is fixed.)
      const idxNames: string[] = json.collections.users.indexes.map((i: { name: string }) => i.name);
      assert.ok(idxNames.includes("users_updated_at_idx"), "system index injected");
      assert.ok(
        idxNames.includes("users_email_idx"),
        "author named index survives descriptors_to_create_ops (W2 fix)",
      );
    } finally {
      await fx.cleanup();
    }
  });

  test("env.db.ts is the module augmentation over the author's schema.ts (§11.3)", async () => {
    const fx = await makeFixture({ "schema.ts": TWO_COLLECTION_SCHEMA });
    const outDir = join(fx.root, "generated/zeroship");
    try {
      await genTypesFromSchemaFile(join(fx.root, "schema.ts"), outDir, {});
      const envDb = await fs.readFile(join(outDir, ENV_DB_FILE), "utf8");
      assert.match(envDb, /DO NOT EDIT/, "carries the DO NOT EDIT banner");
      assert.match(envDb, /import \{ type Db \} from "@zeroship\/db";/, "imports Db");
      // Relative import to the author's schema.ts (out dir is generated/zeroship,
      // schema.ts is two levels up).
      assert.match(envDb, /import \{ schema \} from "\.\.\/\.\.\/schema";/, "imports author schema");
      assert.match(envDb, /db: Db<typeof schema>;/, "augments Env.db");
      assert.match(envDb, /declare module "zeroship"/, "module augmentation");
      // It is NOT the inline generated literal (that is the GENERATED path).
      assert.doesNotMatch(envDb, /const schema = \{/, "manual env.db.ts has no inline schema literal");
    } finally {
      await fx.cleanup();
    }
  });

  test("the emitted env.db.ts type-checks and Db<typeof schema> resolves", async () => {
    const fx = await makeFixture({ "schema.ts": TWO_COLLECTION_SCHEMA });
    const outDir = join(fx.root, "generated/zeroship");
    try {
      await genTypesFromSchemaFile(join(fx.root, "schema.ts"), outDir, {});
      // A probe module that reaches into the augmented `Env.db` type. It resolves
      // the same @zeroship/db this monorepo builds (via a path alias), asserting
      // `Db<typeof schema>` is a real, resolvable type — not `any`/`never`.
      const dbDist = resolve(import.meta.dirname, "../../../db/dist");
      const probe = [
        `import "../generated/zeroship/env.db";`,
        `import type { Env } from "zeroship";`,
        `type UsersColl = Env["db"]["users"];`,
        `// If Db<typeof schema> failed to resolve, UsersColl would be \`never\`/\`any\`.`,
        `type _assertResolved = UsersColl extends never ? never : true;`,
        `const _ok: _assertResolved = true;`,
        `void _ok;`,
      ].join("\n");
      await fs.writeFile(join(outDir, "probe.ts"), probe);

      // A minimal `zeroship` module shim so the augmentation target exists.
      const zeroshipShim = `export interface Env {}\n`;
      await fs.mkdir(join(fx.root, "node_modules/zeroship"), { recursive: true });
      await fs.writeFile(join(fx.root, "node_modules/zeroship/index.d.ts"), zeroshipShim);
      await fs.writeFile(
        join(fx.root, "node_modules/zeroship/package.json"),
        JSON.stringify({ name: "zeroship", types: "index.d.ts", version: "0.0.0" }),
      );

      const tsconfig = {
        compilerOptions: {
          module: "NodeNext",
          moduleResolution: "NodeNext",
          strict: true,
          noEmit: true,
          skipLibCheck: true,
          paths: { "@zeroship/db": [dbDist.replace(/\\/g, "/") + "/index.d.ts"] },
          baseUrl: ".",
        },
        include: ["generated/zeroship/env.db.ts", "generated/zeroship/probe.ts", "schema.ts"],
      };
      await fs.writeFile(join(fx.root, "tsconfig.json"), JSON.stringify(tsconfig, null, 2));

      const { execFileSync } = await import("node:child_process");
      const tscBin = resolve(
        import.meta.dirname,
        "../../node_modules/.bin/tsc",
      );
      // tsc exits 0 iff env.db.ts + the probe type-check cleanly.
      execFileSync(tscBin, ["-p", join(fx.root, "tsconfig.json")], { stdio: "pipe" });
    } finally {
      await fx.cleanup();
    }
  });

  test("--check is clean on a freshly-written pair, then reports drift", async () => {
    const fx = await makeFixture({ "schema.ts": TWO_COLLECTION_SCHEMA });
    const outDir = join(fx.root, "generated/zeroship");
    try {
      await genTypesFromSchemaFile(join(fx.root, "schema.ts"), outDir, {});

      // Clean re-derive.
      const checked = await genTypesFromSchemaFile(join(fx.root, "schema.ts"), outDir, {
        check: true,
      });
      assert.equal(checked.status, "checked");

      // Inject drift into the committed runtime descriptor → --check hard-fails.
      await fs.writeFile(
        join(outDir, RUNTIME_DESCRIPTOR_FILE),
        `{ "version": 1, "collections": {} }\n`,
      );
      await assert.rejects(
        () => genTypesFromSchemaFile(join(fx.root, "schema.ts"), outDir, { check: true }),
        /STALE|drift/i,
      );
    } finally {
      await fx.cleanup();
    }
  });

  test("--check hard-fails when a committed artifact is absent", async () => {
    const fx = await makeFixture({ "schema.ts": TWO_COLLECTION_SCHEMA });
    const outDir = join(fx.root, "generated/zeroship");
    try {
      await assert.rejects(
        () => genTypesFromSchemaFile(join(fx.root, "schema.ts"), outDir, { check: true }),
        /missing/i,
      );
    } finally {
      await fx.cleanup();
    }
  });
});

describe("Cut 1 — NormalizedSchema → CollectionDescriptorDto mapping", () => {
  test("common facets map onto the DTO field shape", async () => {
    const { schema: defineSchema, t } = await import("@zeroship/db");
    const builder = defineSchema({
      name: t.string().required().unique(),
      count: t.number(),
      handle: t.id("handle"),
      owner: t.ref("users"),
    });
    const fields = builder.fields as Record<string, { toFieldDef(): import("@zeroship/db").FieldDef }>;

    const name = fieldDefToDto("c", "name", fields.name.toFieldDef());
    assert.equal(name.type, "string");
    assert.equal(name.required, true);
    assert.equal(name.unique, true);

    const handle = fieldDefToDto("c", "handle", fields.handle.toFieldDef());
    assert.equal(handle.type, "id");
    assert.equal(handle.idPrefix, "handle");

    const owner = fieldDefToDto("c", "owner", fields.owner.toFieldDef());
    assert.equal(owner.type, "ref");
    assert.equal(owner.references, "users");
  });

  test("the JS mapper carries author named indexes into the DTO (drop is downstream)", async () => {
    const { schema: defineSchema, t } = await import("@zeroship/db");
    const { schemaModuleToDescriptors } = await import("../../src/gen-types/manual.js");
    const declared = {
      users: defineSchema({ email: t.string().required() })
        .index("users_email_idx", ["email"])
        .uniqueIndex("users_email_uq", ["email"]),
    };
    const [descriptor] = schemaModuleToDescriptors(declared as Record<string, unknown>);
    const names = (descriptor.indexes ?? []).map((i) => i.name);
    assert.ok(names.includes("users_email_idx"), "named index present in DTO");
    const uq = (descriptor.indexes ?? []).find((i) => i.name === "users_email_uq");
    assert.equal(uq?.unique, true, "unique flag carried on the DTO index");
  });

  test("an unmappable FieldDef facet THROWS (no silent drop)", async () => {
    const { t } = await import("@zeroship/db");
    // `t.calendarDate()` has no descriptor type token — must throw, not drop.
    const cal = t.calendarDate().toFieldDef();
    assert.throws(
      () => fieldDefToDto("c", "birthday", cal),
      /calendarDate|cannot be mapped|no CollectionDescriptorDto home/,
    );
  });
});
