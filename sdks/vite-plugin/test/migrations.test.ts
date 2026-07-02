/**
 * Migration discovery/recording tests plus the `.zship` descriptor packing
 * contract.
 *
 * A migrations dir with `.ts` sources records transient IR through the CLI and
 * returns in-memory bytes. The `.zship` packer does not carry those migration
 * documents; it only stages the generated `schema.runtime.json` descriptor.
 */

import { test, describe } from "node:test";
import assert from "node:assert/strict";
import { promises as fs } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { createHash, randomUUID } from "node:crypto";

import { emitZship } from "../src/zship.js";
import { discoverMigrations, sha256Hex } from "../src/migrations.js";
import { migrationsForEmit } from "../src/build.js";

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

// A minimal, valid transient `.ir.json` byte payload (the bare MigrationIr doc,
// pretty + trailing newline — the canonical byte convention).
const TRANSIENT_IR = `{
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

function recorderStubBody(irText: string): string {
  return [
    "#!/usr/bin/env node",
    "const fs = require('node:fs');",
    "const args = process.argv.slice(2);",
    "if (process.env.ARGS_LOG) fs.writeFileSync(process.env.ARGS_LOG, JSON.stringify(args));",
    "if (process.env.ZSTUB_FAIL === '1') { process.stderr.write('stub: synthetic record failure\\n'); process.exit(3); }",
    "if (args[0] !== 'record') { process.stderr.write('stub: unknown cmd ' + args[0] + '\\n'); process.exit(2); }",
    `process.stdout.write(${JSON.stringify(irText)});`,
    "",
  ].join("\n");
}

describe("op.* migration discovery", () => {
  test("discoverMigrations: records .ts through the CLI and hashes transient stdout", async () => {
    const stem = "20240617123000_notes";
    const fx = await makeFixture({
      [`migrations/${stem}.ts`]: "export function up() {}\n",
      "stub-cli.js": recorderStubBody(TRANSIENT_IR),
    });
    const cliPath = join(fx.root, "stub-cli.js");
    try {
      await fs.chmod(cliPath, 0o755);
      const entries = await discoverMigrations({ root: fx.root, cliPath });
      assert.equal(entries.length, 1);
      assert.equal(entries[0].name, `${stem}.ir.json`);
      assert.equal(entries[0].hash, sha256Hex(Buffer.from(TRANSIENT_IR)));
      assert.equal(entries[0].bytes.toString("utf8"), TRANSIENT_IR);
      await assert.rejects(
        () => fs.readFile(join(fx.root, "migrations", `${stem}.ir.json`)),
        /ENOENT/,
        "discoverMigrations must not write a sibling .ir.json"
      );
    } finally {
      await fx.cleanup();
    }
  });

  test("fresh recording hash tracks the CLI stdout bytes", async () => {
    const stem = "20240617123000_notes";
    const fx = await makeFixture({
      [`migrations/${stem}.ts`]: "export function up() {}\n",
      "stub-cli.js": recorderStubBody(TRANSIENT_IR),
    });
    const cliPath = join(fx.root, "stub-cli.js");
    try {
      await fs.chmod(cliPath, 0o755);
      const before = (await discoverMigrations({ root: fx.root, cliPath }))[0].hash;
      const tampered = TRANSIENT_IR.replace('"notes"', '"NOTES"');
      assert.notEqual(tampered, TRANSIENT_IR);
      await fs.writeFile(join(fx.root, "stub-cli.js"), recorderStubBody(tampered));
      await fs.chmod(cliPath, 0o755);
      const after = (await discoverMigrations({ root: fx.root, cliPath }))[0].hash;
      assert.notEqual(before, after, "the entry hash must track freshly-recorded bytes");
      assert.equal(after, sha256Hex(Buffer.from(tampered)));
    } finally {
      await fx.cleanup();
    }
  });

  test("emitZship omits manifest.migrations and does not stage migration blobs", async () => {
    const stem = "20240617123000_notes";
    const fx = await makeFixture({
      // A minimal dist: one worker module + one asset so emitZship has files.
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
        // Avoid SSR-entry resource catch-all complexity; this is a content test.
        userHasDefaultFetch: false,
      });

      assert.ok(
        !("migrations" in (res.manifest as Record<string, unknown>)),
        "manifest.migrations must not be emitted"
      );
      assert.notEqual(
        res.manifest.runtime_descriptor?.hash,
        sha256Hex(Buffer.from(TRANSIENT_IR)),
        "migration IR bytes must not be staged as a manifest migration entry"
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

  test("no migrations dir → empty discovery result", async () => {
    const fx = await makeFixture({ "dist/index.html": "<html></html>" });
    try {
      const entries = await discoverMigrations({ root: fx.root });
      assert.equal(entries.length, 0);
    } finally {
      await fx.cleanup();
    }
  });
});

// Descriptor bundling — the runtime schema descriptor
// (`schema.runtime.json`) the packer reads from the gen-types output dir and
// carries as the manifest's content-addressed `runtime_descriptor` blob.
// Apps with migration source files must generate it; schema-less apps remain
// descriptor-less.
describe("op.* runtime schema descriptor bundling (P4a)", () => {
  // The exact JSON gen-types' schema.runtime.json holds:
  // RuntimeSchemaDescriptor v1.
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
        migrations: { genTypesOut: "custom/out" },
      });
      assert.ok(res.manifest.runtime_descriptor);
      assert.equal(res.manifest.runtime_descriptor!.hash, sha256Hex(onDisk));
    } finally {
      await fx.cleanup();
    }
  });
});

// LOW #2 — exercise the recordViaCli CLI-shelling path: a `.ts` source must shell
// the (stub) CLI, which prints transient canonical IR. Also asserts a non-zero CLI
// exit surfaces as a thrown Error carrying the stderr. The stub stands in for the
// real Rust `zeroship-migrate-js` (kept hermetic + fast; no V8/no kernel sandbox
// here).
describe("recordViaCli CLI-shelling (A4 record path)", () => {
  const STUB_STEM = "20240617123000_notes";

  const STUB_IR = {
    ir_version: 1,
    name: "notes",
    ops: [{ op: "createTable", name: "notes", columns: [{ name: "title", type: "text" }] }],
  };

  async function withStub(
    extraEnv: Record<string, string>,
    fn: (ctx: {
      root: string;
      cliPath: string;
      argsLog: string;
    }) => Promise<void>
  ): Promise<void> {
    const fx = await makeFixture({
      [`migrations/${STUB_STEM}.ts`]: "export function up() {}\n",
      "stub-cli.js": recorderStubBody(JSON.stringify(STUB_IR, null, 2) + "\n"),
    });
    const cliPath = join(fx.root, "stub-cli.js");
    await fs.chmod(cliPath, 0o755);
    const argsLog = join(fx.root, "args.json");
    const saved: Record<string, string | undefined> = {};
    const env = { ARGS_LOG: argsLog, ...extraEnv };
    for (const [k, v] of Object.entries(env)) {
      saved[k] = process.env[k];
      process.env[k] = v;
    }
    try {
      await fn({ root: fx.root, cliPath, argsLog });
    } finally {
      for (const [k, v] of Object.entries(saved)) {
        if (v === undefined) delete process.env[k];
        else process.env[k] = v;
      }
      await fx.cleanup();
    }
  }

  test("records via the CLI from a .ts-only dir (LOCAL `record` fork)", async () => {
    await withStub({}, async ({ root, cliPath, argsLog }) => {
      const entries = await discoverMigrations({ root, cliPath });
      assert.equal(entries.length, 1, "the recorded artifact is discovered + bundled");
      const expected = Buffer.from(JSON.stringify(STUB_IR, null, 2) + "\n");
      assert.equal(entries[0].hash, sha256Hex(expected));
      assert.deepEqual(entries[0].bytes, expected);
      await assert.rejects(
        () => fs.readFile(join(root, "migrations", `${STUB_STEM}.ir.json`)),
        /ENOENT/,
        "recording must stay in memory"
      );
      // LOCAL fork: `record <file.ts> --owner-app …` (no --recorder-url).
      const args = JSON.parse(await fs.readFile(argsLog, "utf8")) as string[];
      assert.equal(args[0], "record", "LOCAL fork shells `record`");
      assert.ok(args.includes("--owner-app"));
      assert.ok(!args.includes("--recorder-url"), "LOCAL fork has no --recorder-url");
    });
  });

  test("recorderUrl is rejected by the legacy discover helper", async () => {
    await withStub({}, async ({ root, cliPath, argsLog }) => {
      await assert.rejects(
        () =>
          discoverMigrations({
            root,
            cliPath,
            recorderUrl: "https://recorder.example/v1",
          }),
        /gen-types remains the supported build integration/
      );
      await assert.rejects(
        () => fs.readFile(argsLog, "utf8"),
        /ENOENT/,
        "hosted recorderUrl should fail before shelling the stub"
      );
    });
  });

  test("a non-zero CLI exit surfaces as a thrown Error with the stderr", async () => {
    await withStub({ ZSTUB_FAIL: "1" }, async ({ root, cliPath }) => {
      await assert.rejects(
        () => discoverMigrations({ root, cliPath }),
        (err: Error) => {
          assert.match(err.message, /exited 3/);
          assert.match(err.message, /synthetic record failure/);
          return true;
        }
      );
    });
  });

  test("a missing CLI binary surfaces as a thrown Error (res.error)", async () => {
    await withStub({}, async ({ root }) => {
      await assert.rejects(
        () =>
          discoverMigrations({
            root,
            cliPath: join(root, "does-not-exist-cli"),
          }),
        /failed to invoke the recorder CLI/
      );
    });
  });
});

// MED-3 (faithful-e2e) — the stub tests above cover unit behaviour, but per the
// faithful-e2e mandate at least ONE test must shell the ACTUALLY-BUILT Rust
// `zeroship-migrate-js` binary (which drives the REAL kernel-sandboxed recorder
// child) over a real `.ts`, asserting transient IR is returned without writing a
// sibling artifact.
//
// Gated on `ZEROSHIP_MIGRATE_JS_BIN` (or the cargo default-target path). When the env
// var is set the test HARD-FAILS rather than silent-skipping (faithful-e2e rule). CI
// sets `ZEROSHIP_MIGRATE_JS_BIN=<repo>/target/debug/zeroship-migrate-js` after
// `cargo build -p zeroship-migrate --bins`; the sibling
// `zeroship-migrate-recorder-child` must live next to it (standard cargo layout).
describe("recordViaCli against the REAL zeroship-migrate-js binary (faithful e2e)", () => {
  const REAL_STEM = "20240617123000_real_notes";
  // A real op.* migration `.ts` (the recorder resolves `@zeroship/migrate`).
  const REAL_TS = [
    'import { table, t } from "@zeroship/migrate";',
    "export function up() {",
    '  table("real_notes").create({',
    "    title: t.text().notNull(),",
    "  });",
    "}",
    "",
  ].join("\n");

  // Resolve the built binary: explicit env override, else the cargo default-target
  // path relative to this test file (sdks/vite-plugin/test → repo root → target).
  function realCliPath(): string | null {
    const fromEnv = process.env.ZEROSHIP_MIGRATE_JS_BIN;
    if (fromEnv) return fromEnv;
    // sdks/vite-plugin/test/migrations.test.ts → up 3 = repo root.
    const repoRoot = resolve(import.meta.dirname, "..", "..", "..");
    return join(repoRoot, "target", "debug", "zeroship-migrate-js");
  }

  test("shells the built CLI over a real .ts → transient IR returned, no sibling written", async () => {
    const cliPath = realCliPath();
    const explicit = !!process.env.ZEROSHIP_MIGRATE_JS_BIN;
    let cliExists = false;
    if (cliPath) {
      try {
        await fs.access(cliPath);
        cliExists = true;
      } catch {
        cliExists = false;
      }
    }
    if (!cliExists) {
      // Faithful-e2e: HARD-FAIL when the env var is set but the binary is missing
      // (a misconfigured CI must not silently pass). Otherwise (local dev without the
      // built binary) note + skip.
      if (explicit) {
        throw new Error(
          `ZEROSHIP_MIGRATE_JS_BIN is set (${cliPath}) but the binary does not exist — ` +
            "build it: cargo build -p zeroship-migrate --bins"
        );
      }
      console.warn(
        `[skip] real-binary migration e2e: ${cliPath} not built ` +
          "(build it: cargo build -p zeroship-migrate --bins, or set ZEROSHIP_MIGRATE_JS_BIN)"
      );
      return;
    }

    const fx = await makeFixture({
      [`migrations/${REAL_STEM}.ts`]: REAL_TS,
    });
    try {
      const entries = await discoverMigrations({
        root: fx.root,
        cliPath: cliPath!,
        ownerApp: "app_e2e",
      });
      assert.equal(entries.length, 1, "the recorded artifact is discovered + bundled");
      assert.equal(entries[0].name, `${REAL_STEM}.ir.json`);

      // The canonical IR is returned in memory and no committed sibling lands on disk.
      const irPath = join(fx.root, "migrations", `${REAL_STEM}.ir.json`);
      await assert.rejects(() => fs.readFile(irPath), /ENOENT/);
      assert.equal(entries[0].hash, sha256Hex(entries[0].bytes));
      // It is a real recorded IR carrying the createTable op (not an empty stub).
      const doc = JSON.parse(entries[0].bytes.toString("utf8")) as {
        ops?: Array<{ op?: string; name?: string }>;
      };
      assert.ok(
        (doc.ops ?? []).some((o) => o.op === "createTable" && o.name === "real_notes"),
        `the recorded IR must carry the createTable op; got: ${entries[0].bytes.toString("utf8")}`
      );
    } finally {
      await fx.cleanup();
    }
  });
});

describe("build.ts migration sub-option forwarding", () => {
  test("migrationsForEmit forwards descriptor-relevant fields", () => {
    const out = migrationsForEmit({
      dir: "migrations",
      genTypesOut: "generated/zeroship",
      cliPath: "/bin/zeroship-migrate-js",
    });
    assert.equal(out.dir, "migrations");
    assert.equal(out.genTypesOut, "generated/zeroship");
    assert.ok(!("cliPath" in out));
  });

  test("migrationsForEmit on undefined yields an all-undefined object (defaults preserved)", () => {
    const out = migrationsForEmit(undefined);
    assert.deepEqual(out, {
      dir: undefined,
      genTypesOut: undefined,
    });
  });
});
