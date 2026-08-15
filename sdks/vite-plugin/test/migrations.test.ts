/**
 * `.zship` runtime-schema-descriptor packing contract.
 *
 * The `.zship` packer does not carry migration documents; it only stages the
 * generated `schema.runtime.json` descriptor (produced in-process by the
 * `gen-types` library — see `test/gen-types/*.test.ts`) as the manifest's
 * content-addressed `runtime_descriptor` blob. These tests pin that packing
 * contract + the build sub-option forwarding.
 */

import { test, describe } from "node:test";
import assert from "node:assert/strict";
import { promises as fs } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { createHash, randomUUID } from "node:crypto";

import { emitZship } from "../src/zship.js";

/** Local sha256 hex — the SAME convention the `.zship` packer uses. */
function sha256Hex(bytes: Buffer): string {
  return createHash("sha256").update(bytes).digest("hex");
}

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

const RUNTIME_DESCRIPTOR = `{
  "version": 1,
  "collections": {
    "notes": {
      "fields": {
        "title": { "type": "string" }
      },
      "options": { "softDelete": false, "versioning": false, "strictness": "strict" },
      "indexes": []
    }
  }
}
`;

// Descriptor bundling — the runtime schema descriptor (`schema.runtime.json`)
// the packer reads from the gen-types output dir and carries as the manifest's
// content-addressed `runtime_descriptor` blob. Apps with migration source files
// must generate it; schema-less apps remain descriptor-less.
describe("op.* runtime schema descriptor bundling", () => {
  const DESCRIPTOR = RUNTIME_DESCRIPTOR;

  test("emitZship carries runtime_descriptor + stages the descriptor blob", async () => {
    const stem = "20240617123000_notes";
    const fx = await makeFixture({
      "dist/server/index.js":
        "export default { fetch(){ return new Response('ok'); } }\n",
      "dist/index.html": "<!doctype html><html></html>\n",
      [`migrations/${stem}.ts`]: "export function up() {}\n",
      // The committed gen-types output (default dir `generated/zeroship`).
      "generated/zeroship/schema.runtime.json": DESCRIPTOR,
    });
    try {
      const onDisk = await fs.readFile(
        join(fx.root, "generated", "zeroship", "schema.runtime.json")
      );
      const expectHash = sha256Hex(onDisk);

      const res = await emitZship({
        root: fx.root,
        distDir: "dist",
        silent: true,
        builtAt: "2026-06-24T00:00:00Z",
        userHasDefaultFetch: false,
        migrations: { dir: "migrations", genTypesOut: "generated/zeroship" },
      });

      assert.ok(
        res.manifest.runtime_descriptor,
        "manifest.runtime_descriptor must be set when schema.runtime.json exists"
      );
      assert.equal(
        res.manifest.runtime_descriptor!.hash,
        expectHash,
        "the descriptor entry hash must equal the sha256 of the on-disk bytes"
      );
    } finally {
      await fx.cleanup();
    }
  });

  test("emitZship omits manifest.migrations and does not stage migration blobs", async () => {
    const stem = "20240617123000_notes";
    const fx = await makeFixture({
      "dist/server/index.js": "export default { fetch(){ return new Response('ok'); } }\n",
      "dist/index.html": "<!doctype html><html></html>\n",
      [`migrations/${stem}.ts`]: "export function up() {}\n",
      "generated/zeroship/schema.runtime.json": RUNTIME_DESCRIPTOR,
    });
    try {
      const res = await emitZship({
        root: fx.root,
        distDir: "dist",
        silent: true,
        builtAt: "2026-06-24T00:00:00Z",
        userHasDefaultFetch: false,
      });

      assert.ok(
        !("migrations" in (res.manifest as Record<string, unknown>)),
        "manifest.migrations must not be emitted"
      );
    } finally {
      await fx.cleanup();
    }
  });

  test("migration sources without schema.runtime.json fail the build", async () => {
    const stem = "20240617123000_notes";
    const fx = await makeFixture({
      "dist/server/index.js":
        "export default { fetch(){ return new Response('ok'); } }\n",
      "dist/index.html": "<!doctype html><html></html>\n",
      [`migrations/${stem}.ts`]: "export function up() {}\n",
      // No generated/zeroship/schema.runtime.json on disk.
    });
    try {
      await assert.rejects(
        () => emitZship({
          root: fx.root,
          distDir: "dist",
          silent: true,
          builtAt: "2026-06-24T00:00:00Z",
          userHasDefaultFetch: false,
          // Stated, not defaulted: the packer no longer guesses these, so a
          // test that omitted them would exercise the "caller did not ask for
          // a descriptor" arm instead of this one.
          migrations: { dir: "migrations", genTypesOut: "generated/zeroship" },
        }),
        /migration source files.*schema\.runtime\.json|schema\.runtime\.json.*migration service/
      );
    } finally {
      await fx.cleanup();
    }
  });

  test("schema.runtime.json with invalid UTF-8 bytes fails at pack time", async () => {
    const stem = "20240617123000_notes";
    const descriptor = Buffer.concat([
      Buffer.from(
        `{"version":1,"collections":{"notes":{"fields":{"title":{"type":"stri`,
        "utf8",
      ),
      Buffer.from([0xff]),
      Buffer.from(
        `ng"}},"options":{"softDelete":false,"versioning":false},"indexes":[]}}}`,
        "utf8",
      ),
    ]);
    const fx = await makeFixture({
      "dist/server/index.js":
        "export default { fetch(){ return new Response('ok'); } }\n",
      "dist/index.html": "<!doctype html><html></html>\n",
      [`migrations/${stem}.ts`]: "export function up() {}\n",
      "generated/zeroship/schema.runtime.json": descriptor,
    });
    try {
      await assert.rejects(
        () =>
          emitZship({
            root: fx.root,
            distDir: "dist",
            silent: true,
            builtAt: "2026-06-24T00:00:00Z",
            userHasDefaultFetch: false,
            migrations: { dir: "migrations", genTypesOut: "generated/zeroship" },
          }),
        /runtime_descriptor.*UTF-8|schema\.runtime\.json.*UTF-8/i,
      );
    } finally {
      await fx.cleanup();
    }
  });

  test("custom genTypesOut dir is honoured by the packer", async () => {
    const fx = await makeFixture({
      "dist/server/index.js":
        "export default { fetch(){ return new Response('ok'); } }\n",
      "dist/index.html": "<!doctype html><html></html>\n",
      "custom/out/schema.runtime.json": DESCRIPTOR,
    });
    try {
      const onDisk = await fs.readFile(
        join(fx.root, "custom", "out", "schema.runtime.json")
      );
      const res = await emitZship({
        root: fx.root,
        distDir: "dist",
        silent: true,
        builtAt: "2026-06-24T00:00:00Z",
        userHasDefaultFetch: false,
        migrations: { dir: "migrations", genTypesOut: "custom/out" },
      });
      assert.ok(res.manifest.runtime_descriptor);
      assert.equal(res.manifest.runtime_descriptor!.hash, sha256Hex(onDisk));
    } finally {
      await fx.cleanup();
    }
  });
});

// `migrationsForEmit` is gone with the plugin options it forwarded. The build
// now hands `emitZship` the two paths from the resolved project config
// directly, and `ZshipOptions.migrations` REQUIRES both - so there is nothing
// left to forward and no second place for either default to live.
