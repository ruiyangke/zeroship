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
      "generated/zeroship/schema.runtime.json": RUNTIME_DESCRIPTOR,
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

// Migration-first descriptor bundling — the runtime schema descriptor
// (`schema.runtime.json`) the packer reads from the gen-types output dir and
// carries as the manifest's content-addressed `runtime_descriptor` blob.
// Migration-bearing apps must carry it; no-migration apps remain schema-less.
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
      [`migrations/${stem}.ir.json`]: COMMITTED_IR,
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

  test("migrations without schema.runtime.json fail the build", async () => {
    const stem = "20240617123000_notes";
    const fx = await makeFixture({
      "dist/server/index.js":
        "export default { fetch(){ return new Response('ok'); } }\n",
      "dist/index.html": "<!doctype html><html></html>\n",
      [`migrations/${stem}.ts`]: "export function up() {}\n",
      [`migrations/${stem}.ir.json`]: COMMITTED_IR,
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
        /migrations?.*runtime schema descriptor|schema\.runtime\.json/
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

// LOW #2 — exercise the recordViaCli CLI-shelling path: a `.ts` with NO committed
// `.ir.json` must shell the (stub) CLI, which produces the committed artifact. Also
// asserts (a) the LOCAL-vs-hosted arg fork and (b) a non-zero CLI exit surfaces as a
// thrown Error carrying the stderr. The stub stands in for the real Rust
// `zeroship-migrate-js` (kept hermetic + fast; no V8/no kernel sandbox here).
describe("recordViaCli CLI-shelling (A4 record path)", () => {
  const STUB_STEM = "20240617123000_notes";

  // A node stub CLI. `record <file.ts> --owner-app X` writes a sibling `.ir.json`
  // and logs the args to `$ARGS_LOG` so the test can assert the arg fork. `build
  // ... --recorder-url URL` writes the `.ir.json` for every `.ts` in `--dir` (the
  // hosted-fork shape). `ZSTUB_FAIL=1` makes it exit non-zero with a stderr message.
  const STUB_BODY = [
    "#!/usr/bin/env node",
    "const fs = require('node:fs');",
    "const path = require('node:path');",
    "const args = process.argv.slice(2);",
    "if (process.env.ARGS_LOG) fs.writeFileSync(process.env.ARGS_LOG, JSON.stringify(args));",
    "if (process.env.ZSTUB_FAIL === '1') { process.stderr.write('stub: synthetic record failure\\n'); process.exit(3); }",
    "const ir = JSON.parse(process.env.ZSTUB_IR);",
    "const irText = JSON.stringify(ir, null, 2) + '\\n';",
    "function writeFor(tsPath) {",
    "  const dir = path.dirname(tsPath);",
    "  const base = path.basename(tsPath).replace(/\\.ts$/, '');",
    "  fs.writeFileSync(path.join(dir, base + '.ir.json'), irText);",
    "}",
    "if (args[0] === 'record') { writeFor(args[1]); }",
    "else if (args[0] === 'build') {",
    "  const di = args.indexOf('--dir'); const dir = args[di + 1];",
    "  for (const n of fs.readdirSync(dir)) if (n.endsWith('.ts')) writeFor(path.join(dir, n));",
    "} else { process.stderr.write('stub: unknown cmd ' + args[0] + '\\n'); process.exit(2); }",
    "",
  ].join("\n");

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
      "stub-cli.js": STUB_BODY,
    });
    const cliPath = join(fx.root, "stub-cli.js");
    await fs.chmod(cliPath, 0o755);
    const argsLog = join(fx.root, "args.json");
    const saved: Record<string, string | undefined> = {};
    const env = { ZSTUB_IR: JSON.stringify(STUB_IR), ARGS_LOG: argsLog, ...extraEnv };
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

  test("records via the CLI when no committed .ir.json exists (LOCAL `record` fork)", async () => {
    await withStub({}, async ({ root, cliPath, argsLog }) => {
      const entries = await discoverMigrations({ root, cliPath });
      assert.equal(entries.length, 1, "the recorded artifact is discovered + bundled");
      const onDisk = await fs.readFile(
        join(root, "migrations", `${STUB_STEM}.ir.json`)
      );
      assert.equal(entries[0].hash, sha256Hex(onDisk));
      // LOCAL fork: `record <file.ts> --owner-app …` (no --recorder-url).
      const args = JSON.parse(await fs.readFile(argsLog, "utf8")) as string[];
      assert.equal(args[0], "record", "LOCAL fork shells `record`");
      assert.ok(args.includes("--owner-app"));
      assert.ok(!args.includes("--recorder-url"), "LOCAL fork has no --recorder-url");
    });
  });

  test("hosted fork: a recorderUrl shells `build --recorder-url`", async () => {
    await withStub({}, async ({ root, cliPath, argsLog }) => {
      await discoverMigrations({
        root,
        cliPath,
        recorderUrl: "https://recorder.example/v1",
      });
      const args = JSON.parse(await fs.readFile(argsLog, "utf8")) as string[];
      assert.equal(args[0], "build", "hosted fork shells `build`");
      const ri = args.indexOf("--recorder-url");
      assert.ok(ri >= 0, "hosted fork passes --recorder-url");
      assert.equal(args[ri + 1], "https://recorder.example/v1");
      assert.ok(args.includes("--dir"), "hosted fork passes --dir");
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

// MED-3 (faithful-e2e) — the stub tests above cover the arg-fork unit behaviour, but
// per the faithful-e2e mandate at least ONE test must shell the ACTUALLY-BUILT Rust
// `zeroship-migrate-js` binary (which drives the REAL kernel-sandboxed recorder
// child) over a real `.ts`, asserting a committed `.ir.json` is produced + bundled.
//
// Gated on `ZEROSHIP_MIGRATE_JS_BIN` (or the cargo default-target path). When the env
// var is set the test HARD-FAILS rather than silent-skipping (faithful-e2e rule). CI
// sets `ZEROSHIP_MIGRATE_JS_BIN=<repo>/target/debug/zeroship-migrate-js` after
// `cargo build -p zeroship-migrate-js --bins`; the sibling
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

  test("shells the built CLI over a real .ts → committed .ir.json produced + bundled", async () => {
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
            "build it: cargo build -p zeroship-migrate-js --bins"
        );
      }
      console.warn(
        `[skip] real-binary migration e2e: ${cliPath} not built ` +
          "(build it: cargo build -p zeroship-migrate-js --bins, or set ZEROSHIP_MIGRATE_JS_BIN)"
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

      // The committed `.ir.json` really landed on disk and the entry hash tracks it.
      const irPath = join(fx.root, "migrations", `${REAL_STEM}.ir.json`);
      const onDisk = await fs.readFile(irPath);
      assert.equal(entries[0].hash, sha256Hex(onDisk));
      // It is a real recorded IR carrying the createTable op (not an empty stub).
      const doc = JSON.parse(onDisk.toString("utf8")) as {
        ops?: Array<{ op?: string; name?: string }>;
      };
      assert.ok(
        (doc.ops ?? []).some((o) => o.op === "createTable" && o.name === "real_notes"),
        `the recorded IR must carry the createTable op; got: ${onDisk.toString("utf8")}`
      );
    } finally {
      await fx.cleanup();
    }
  });
});

// LOW-1 (P4a review) — build.ts's `emitZship({ migrations })` call site must
// forward the FULL migration sub-options. The earlier partial spread threaded
// only dir/genTypesOut/cliPath and DROPPED ownerApp/recorderUrl, which the
// packer's migration discovery (step 8b) consumes — silently breaking the
// recorder arg-fork in production builds. These tests pin both the structural
// forwarder and the end-to-end flow of those two fields to the recorder CLI.
describe("build.ts migration sub-option forwarding (P4a LOW-1)", () => {
  test("migrationsForEmit forwards ALL five fields (no partial spread)", () => {
    const out = migrationsForEmit({
      dir: "migrations",
      genTypesOut: "generated/zeroship",
      cliPath: "/bin/zeroship-migrate-js",
      ownerApp: "app_owner123",
      recorderUrl: "https://recorder.example/v1",
    });
    // The bug was that ownerApp/recorderUrl were silently absent — assert
    // every field survives, not just the three the old spread copied.
    assert.equal(out.dir, "migrations");
    assert.equal(out.genTypesOut, "generated/zeroship");
    assert.equal(out.cliPath, "/bin/zeroship-migrate-js");
    assert.equal(out.ownerApp, "app_owner123", "ownerApp must be forwarded");
    assert.equal(
      out.recorderUrl,
      "https://recorder.example/v1",
      "recorderUrl must be forwarded"
    );
  });

  test("migrationsForEmit on undefined yields an all-undefined object (defaults preserved)", () => {
    const out = migrationsForEmit(undefined);
    assert.deepEqual(out, {
      dir: undefined,
      genTypesOut: undefined,
      cliPath: undefined,
      ownerApp: undefined,
      recorderUrl: undefined,
    });
  });

  // Faithful e2e: drive the REAL emitZship → REAL discoverMigrations → REAL
  // spawnSync of a stub recorder CLI, asserting the ownerApp/recorderUrl that
  // build.ts now forwards actually reach the recorder arg-fork. A `.ts` with no
  // committed `.ir.json` forces the record path; the stub logs its argv.
  const STUB_STEM = "20240617123000_notes";
  const STUB_BODY = [
    "#!/usr/bin/env node",
    "const fs = require('node:fs');",
    "const path = require('node:path');",
    "const args = process.argv.slice(2);",
    "if (process.env.ARGS_LOG) fs.writeFileSync(process.env.ARGS_LOG, JSON.stringify(args));",
    "const ir = JSON.parse(process.env.ZSTUB_IR);",
    "const irText = JSON.stringify(ir, null, 2) + '\\n';",
    "function writeFor(tsPath) {",
    "  const dir = path.dirname(tsPath);",
    "  const base = path.basename(tsPath).replace(/\\.ts$/, '');",
    "  fs.writeFileSync(path.join(dir, base + '.ir.json'), irText);",
    "}",
    "if (args[0] === 'record') { writeFor(args[1]); }",
    "else if (args[0] === 'build') {",
    "  const di = args.indexOf('--dir'); const dir = args[di + 1];",
    "  for (const n of fs.readdirSync(dir)) if (n.endsWith('.ts')) writeFor(path.join(dir, n));",
    "} else { process.exit(2); }",
    "",
  ].join("\n");
  const STUB_IR = {
    ir_version: 1,
    name: "notes",
    ops: [{ op: "createTable", name: "notes", columns: [{ name: "title", type: "text" }] }],
  };

  test("emitZship forwards ownerApp to the recorder (LOCAL `record --owner-app`)", async () => {
    const fx = await makeFixture({
      "dist/server/index.js":
        "export default { fetch(){ return new Response('ok'); } }\n",
      "dist/index.html": "<!doctype html><html></html>\n",
      [`migrations/${STUB_STEM}.ts`]: "export function up() {}\n",
      "generated/zeroship/schema.runtime.json": RUNTIME_DESCRIPTOR,
      "stub-cli.js": STUB_BODY,
    });
    const cliPath = join(fx.root, "stub-cli.js");
    await fs.chmod(cliPath, 0o755);
    const argsLog = join(fx.root, "args.json");
    const saved = { ZSTUB_IR: process.env.ZSTUB_IR, ARGS_LOG: process.env.ARGS_LOG };
    process.env.ZSTUB_IR = JSON.stringify(STUB_IR);
    process.env.ARGS_LOG = argsLog;
    try {
      await emitZship({
        root: fx.root,
        distDir: "dist",
        silent: true,
        builtAt: "2026-06-24T00:00:00Z",
        userHasDefaultFetch: false,
        // Exactly the shape build.ts now forwards via migrationsForEmit.
        migrations: { cliPath, ownerApp: "app_owner123" },
      });
      const args = JSON.parse(await fs.readFile(argsLog, "utf8")) as string[];
      assert.equal(args[0], "record", "LOCAL fork shells `record`");
      const oi = args.indexOf("--owner-app");
      assert.ok(oi >= 0, "the forwarded ownerApp must reach the recorder");
      assert.equal(
        args[oi + 1],
        "app_owner123",
        "ownerApp value must NOT be dropped (the LOW-1 partial-spread bug)"
      );
    } finally {
      if (saved.ZSTUB_IR === undefined) delete process.env.ZSTUB_IR;
      else process.env.ZSTUB_IR = saved.ZSTUB_IR;
      if (saved.ARGS_LOG === undefined) delete process.env.ARGS_LOG;
      else process.env.ARGS_LOG = saved.ARGS_LOG;
      await fx.cleanup();
    }
  });

  test("emitZship forwards recorderUrl to the recorder (hosted `build --recorder-url`)", async () => {
    const fx = await makeFixture({
      "dist/server/index.js":
        "export default { fetch(){ return new Response('ok'); } }\n",
      "dist/index.html": "<!doctype html><html></html>\n",
      [`migrations/${STUB_STEM}.ts`]: "export function up() {}\n",
      "generated/zeroship/schema.runtime.json": RUNTIME_DESCRIPTOR,
      "stub-cli.js": STUB_BODY,
    });
    const cliPath = join(fx.root, "stub-cli.js");
    await fs.chmod(cliPath, 0o755);
    const argsLog = join(fx.root, "args.json");
    const saved = { ZSTUB_IR: process.env.ZSTUB_IR, ARGS_LOG: process.env.ARGS_LOG };
    process.env.ZSTUB_IR = JSON.stringify(STUB_IR);
    process.env.ARGS_LOG = argsLog;
    try {
      await emitZship({
        root: fx.root,
        distDir: "dist",
        silent: true,
        builtAt: "2026-06-24T00:00:00Z",
        userHasDefaultFetch: false,
        migrations: {
          cliPath,
          ownerApp: "app_owner123",
          recorderUrl: "https://recorder.example/v1",
        },
      });
      const args = JSON.parse(await fs.readFile(argsLog, "utf8")) as string[];
      assert.equal(args[0], "build", "hosted fork shells `build`");
      const ri = args.indexOf("--recorder-url");
      assert.ok(ri >= 0, "the forwarded recorderUrl must reach the recorder");
      assert.equal(
        args[ri + 1],
        "https://recorder.example/v1",
        "recorderUrl value must NOT be dropped (the LOW-1 partial-spread bug)"
      );
    } finally {
      if (saved.ZSTUB_IR === undefined) delete process.env.ZSTUB_IR;
      else process.env.ZSTUB_IR = saved.ZSTUB_IR;
      if (saved.ARGS_LOG === undefined) delete process.env.ARGS_LOG;
      else process.env.ARGS_LOG = saved.ARGS_LOG;
      await fx.cleanup();
    }
  });
});
