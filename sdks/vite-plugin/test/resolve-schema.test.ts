/**
 * Unit tests for the schema-path resolver.
 *
 * Convention chain (see `src/resolve-schema.ts`):
 *   1. `opt` set       → that path (asserted to exist)
 *   2. `src/schema.*`  → single-file convention
 *   3. `src/schema/index.*` → directory-form convention
 *   4. else            → null (entry-fallback)
 *
 * Each branch is covered by a fixture tmpdir.
 */

import { test, describe } from "node:test";
import assert from "node:assert/strict";
import { promises as fs } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { randomUUID } from "node:crypto";

import { resolveSchemaPath } from "../src/resolve-schema.js";

interface Fixture {
  root: string;
  cleanup: () => Promise<void>;
}

async function makeFixture(files: Record<string, string>): Promise<Fixture> {
  const root = join(tmpdir(), `resolve-schema-${randomUUID()}`);
  await fs.mkdir(root, { recursive: true });
  for (const [rel, content] of Object.entries(files)) {
    const abs = resolve(root, rel);
    await fs.mkdir(resolve(abs, ".."), { recursive: true });
    await fs.writeFile(abs, content);
  }
  return { root, cleanup: () => fs.rm(root, { recursive: true, force: true }) };
}

describe("resolveSchemaPath", () => {
  test("returns the explicit option when set", async () => {
    const fix = await makeFixture({
      "custom/schema.ts": "export default { schema: {} };",
    });
    try {
      const got = resolveSchemaPath(fix.root, "./custom/schema.ts");
      assert.equal(got.source, "option");
      assert.equal(got.path, resolve(fix.root, "custom/schema.ts"));
    } finally {
      await fix.cleanup();
    }
  });

  test("absolute option paths are returned as-is", async () => {
    const fix = await makeFixture({
      "custom/schema.ts": "export default { schema: {} };",
    });
    try {
      const abs = resolve(fix.root, "custom/schema.ts");
      const got = resolveSchemaPath(fix.root, abs);
      assert.equal(got.source, "option");
      assert.equal(got.path, abs);
    } finally {
      await fix.cleanup();
    }
  });

  test("throws when the explicit option file does not exist", async () => {
    const fix = await makeFixture({});
    try {
      assert.throws(
        () => resolveSchemaPath(fix.root, "./does-not-exist.ts"),
        /does-not-exist\.ts/,
      );
    } finally {
      await fix.cleanup();
    }
  });

  test("picks src/schema.ts when present", async () => {
    const fix = await makeFixture({
      "src/schema.ts": "export default { schema: {} };",
    });
    try {
      const got = resolveSchemaPath(fix.root);
      assert.equal(got.source, "src/schema.ts");
      assert.equal(got.path, resolve(fix.root, "src/schema.ts"));
    } finally {
      await fix.cleanup();
    }
  });

  test("picks src/schema.js when no .ts exists", async () => {
    const fix = await makeFixture({
      "src/schema.js": "export default { schema: {} };",
    });
    try {
      const got = resolveSchemaPath(fix.root);
      // Source string is the conventional name, not the actual extension.
      assert.equal(got.source, "src/schema.ts");
      assert.equal(got.path, resolve(fix.root, "src/schema.js"));
    } finally {
      await fix.cleanup();
    }
  });

  test("picks src/schema/index.ts when no single-file form", async () => {
    const fix = await makeFixture({
      "src/schema/index.ts": "export default { schema: {} };",
    });
    try {
      const got = resolveSchemaPath(fix.root);
      assert.equal(got.source, "src/schema/index.ts");
      assert.equal(got.path, resolve(fix.root, "src/schema/index.ts"));
    } finally {
      await fix.cleanup();
    }
  });

  test("falls back to entry-fallback when nothing matches", async () => {
    const fix = await makeFixture({
      "src/index.ts": "export default { schema: {} };",
    });
    try {
      const got = resolveSchemaPath(fix.root);
      assert.equal(got.source, "entry-fallback");
      assert.equal(got.path, null);
    } finally {
      await fix.cleanup();
    }
  });

  test("single-file form wins over directory form", async () => {
    const fix = await makeFixture({
      "src/schema.ts": "export default { schema: {} };",
      "src/schema/index.ts": "export default { schema: {} };",
    });
    try {
      const got = resolveSchemaPath(fix.root);
      assert.equal(got.source, "src/schema.ts");
      assert.equal(got.path, resolve(fix.root, "src/schema.ts"));
    } finally {
      await fix.cleanup();
    }
  });

  test("option wins over convention", async () => {
    const fix = await makeFixture({
      "src/schema.ts": "// convention",
      "other.ts": "// option",
    });
    try {
      const got = resolveSchemaPath(fix.root, "./other.ts");
      assert.equal(got.source, "option");
      assert.equal(got.path, resolve(fix.root, "other.ts"));
    } finally {
      await fix.cleanup();
    }
  });

  test("empty-string option is treated as unset", async () => {
    const fix = await makeFixture({
      "src/schema.ts": "// convention",
    });
    try {
      const got = resolveSchemaPath(fix.root, "");
      assert.equal(got.source, "src/schema.ts");
    } finally {
      await fix.cleanup();
    }
  });
});
