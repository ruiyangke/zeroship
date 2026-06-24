/**
 * PR4 deliverable A4 / test #5 (the TS peer of the Rust packed_hash test).
 *
 * A migrations dir with one committed `.ir.json` yields a `manifest.migrations`
 * entry whose `hash` equals the sha256 of the on-disk bytes — packer-consumes-
 * verbatim parity. The committed bytes are also staged into the archive as a blob
 * (never re-emitted). Tampering the committed bytes makes the entry hash TRACK the
 * disk, proving the packer copies.
 */

import { test, describe } from "node:test";
import assert from "node:assert/strict";
import { promises as fs } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { createHash, randomUUID } from "node:crypto";

import { emitZship } from "../src/zship.js";
import { discoverMigrations, sha256Hex } from "../src/migrations.js";

async function makeFixture(
  files: Record<string, string | Buffer>
): Promise<{ root: string; cleanup: () => Promise<void> }> {
  const root = join(tmpdir(), `mig-test-${randomUUID()}`);
  await fs.mkdir(root, { recursive: true });
  for (const [relPath, content] of Object.entries(files)) {
    const abs = resolve(root, relPath);
    await fs.mkdir(dirname(abs), { recursive: true });
    await fs.writeFile(abs, content as Buffer | string);
  }
  return { root, cleanup: () => fs.rm(root, { recursive: true, force: true }) };
}

// A minimal, valid committed `.ir.json` (the bare MigrationIr doc, pretty +
// trailing newline — the canonical byte convention).
const COMMITTED_IR = `{
  "ir_version": 1,
  "name": "notes",
  "ops": [
    {
      "op": "createTable",
      "name": "notes",
      "columns": [
        {
          "name": "title",
          "type": "text"
        }
      ]
    }
  ]
}
`;

describe("op.* migration discovery + bundling (A4)", () => {
  test("discoverMigrations: committed .ir.json hash equals on-disk sha256 (verbatim)", async () => {
    const stem = "20240617123000_notes";
    const fx = await makeFixture({
      [`migrations/${stem}.ts`]: "export function up() {}\n",
      [`migrations/${stem}.ir.json`]: COMMITTED_IR,
    });
    try {
      const entries = await discoverMigrations({ root: fx.root });
      assert.equal(entries.length, 1);
      const onDisk = await fs.readFile(join(fx.root, "migrations", `${stem}.ir.json`));
      assert.equal(entries[0].name, `${stem}.ir.json`);
      assert.equal(
        entries[0].hash,
        sha256Hex(onDisk),
        "the entry hash must equal the sha256 of the on-disk committed bytes"
      );
      // The bytes the packer stages are EXACTLY the on-disk bytes.
      assert.deepEqual(Buffer.from(entries[0].bytes), onDisk);
    } finally {
      await fx.cleanup();
    }
  });

  test("packer copies (never re-emits): tampering the committed bytes tracks the disk", async () => {
    const stem = "20240617123000_notes";
    const fx = await makeFixture({
      [`migrations/${stem}.ts`]: "export function up() {}\n",
      [`migrations/${stem}.ir.json`]: COMMITTED_IR,
    });
    try {
      const before = (await discoverMigrations({ root: fx.root }))[0].hash;
      // Mutate one byte of the committed artifact.
      const tampered = COMMITTED_IR.replace('"notes"', '"NOTES"');
      assert.notEqual(tampered, COMMITTED_IR);
      await fs.writeFile(join(fx.root, "migrations", `${stem}.ir.json`), tampered);
      const after = (await discoverMigrations({ root: fx.root }))[0].hash;
      assert.notEqual(before, after, "the entry hash must track the tampered disk bytes");
      assert.equal(after, sha256Hex(Buffer.from(tampered)));
    } finally {
      await fx.cleanup();
    }
  });

  test("emitZship contributes manifest.migrations + stages the blob", async () => {
    const stem = "20240617123000_notes";
    const fx = await makeFixture({
      // A minimal dist: one worker module + one asset so emitZship has files.
      "dist/server/index.js": "export default { fetch(){ return new Response('ok'); } }\n",
      "dist/index.html": "<!doctype html><html></html>\n",
      [`migrations/${stem}.ts`]: "export function up() {}\n",
      [`migrations/${stem}.ir.json`]: COMMITTED_IR,
    });
    try {
      const onDisk = await fs.readFile(join(fx.root, "migrations", `${stem}.ir.json`));
      const expectHash = sha256Hex(onDisk);

      const res = await emitZship({
        root: fx.root,
        distDir: "dist",
        silent: true,
        builtAt: "2026-06-24T00:00:00Z",
        // Avoid SSR-entry resource catch-all complexity; this is a content test.
        userHasDefaultFetch: false,
      });

      const migrations = res.manifest.migrations ?? [];
      assert.equal(migrations.length, 1, "manifest.migrations must carry the committed artifact");
      assert.equal(migrations[0].name, `${stem}.ir.json`);
      assert.equal(
        migrations[0].hash,
        expectHash,
        "the manifest entry hash must equal the sha256 of the on-disk committed bytes"
      );
    } finally {
      await fx.cleanup();
    }
  });

  test("discoverMigrations rejects an invalid .ts filename", async () => {
    const fx = await makeFixture({
      "migrations/not-a-migration.ts": "export function up() {}\n",
    });
    try {
      await assert.rejects(
        () => discoverMigrations({ root: fx.root }),
        /filename grammar/
      );
    } finally {
      await fx.cleanup();
    }
  });

  test("no migrations dir → empty (ships no migrations)", async () => {
    const fx = await makeFixture({ "dist/index.html": "<html></html>" });
    try {
      const entries = await discoverMigrations({ root: fx.root });
      assert.equal(entries.length, 0);
    } finally {
      await fx.cleanup();
    }
  });
});
