/**
 * Migration-first P3 — `gen-types` wiring into @zeroship/vite-plugin.
 *
 * The plugin shells the EXISTING `zeroship-migrate-js gen-types --dir <migrations>
 * --out <outDir> [--check]` subcommand (it does NOT re-implement type generation).
 * These tests cover the four behaviours the wiring adds:
 *
 *  (a) a migration change invokes `gen-types` with the right args + emits into the
 *      configured `genTypesOut` dir (stub CLI — hermetic, asserts the arg shape);
 *  (b) a REAL-binary e2e: the actual `zeroship-migrate-js` folds a real recorded
 *      migration and regenerates `env.db.ts` + `schema.runtime.json` (gated on
 *      ZEROSHIP_MIGRATE_JS_BIN / target/debug; HARD-FAILS if the env var is set but
 *      the binary is missing);
 *  (c) `--check` drift: when the committed artifacts diverge from the migrations the
 *      CLI exits non-zero and `genTypesViaCli` THROWS (the CI drift gate);
 *  (d) a MISSING binary in DEV warns-and-noops (`{ status: "skipped" }`, no throw);
 *      with `requireBinary` (the CI gate) the same absence THROWS.
 *
 * The P3 deferral (artifacts emitted OUTSIDE the tsc program; alias untouched) is a
 * design constraint, not a runtime behaviour, so it is asserted by the `genTypesOut`
 * default living at `generated/zeroship` (NOT under `src/`, NOT `.zeroship/`).
 */

import { test, describe } from "node:test";
import assert from "node:assert/strict";
import { promises as fs } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { randomUUID } from "node:crypto";

import {
  genTypesViaCli,
  resolveGenTypesCli,
  GEN_TYPES_OUT_DEFAULT,
} from "../src/migrations.js";
import { devServerPlugin } from "../src/dev-server.js";
import type { TransformState } from "../src/transform.js";

async function makeFixture(
  files: Record<string, string | Buffer>
): Promise<{ root: string; cleanup: () => Promise<void> }> {
  const root = join(tmpdir(), `gentypes-test-${randomUUID()}`);
  await fs.mkdir(root, { recursive: true });
  for (const [relPath, content] of Object.entries(files)) {
    const abs = resolve(root, relPath);
    await fs.mkdir(dirname(abs), { recursive: true });
    await fs.writeFile(abs, content as Buffer | string);
  }
  return { root, cleanup: () => fs.rm(root, { recursive: true, force: true }) };
}

// A node stub CLI standing in for `zeroship-migrate-js gen-types`. It logs its
// args to `$ARGS_LOG` (so the test asserts the arg fork) and, unless `--check`,
// writes the two artifacts into `--out`. `ZSTUB_DRIFT=1` makes `--check` exit
// non-zero with a stderr drift message (the drift-gate path).
const STUB_GENTYPES = [
  "#!/usr/bin/env node",
  "const fs = require('node:fs');",
  "const path = require('node:path');",
  "const args = process.argv.slice(2);",
  "if (process.env.ARGS_LOG) fs.writeFileSync(process.env.ARGS_LOG, JSON.stringify(args));",
  "if (args[0] !== 'gen-types') { process.stderr.write('stub: unknown cmd ' + args[0] + '\\n'); process.exit(2); }",
  "const oi = args.indexOf('--out'); const out = args[oi + 1];",
  "const check = args.includes('--check');",
  "if (check) {",
  "  if (process.env.ZSTUB_DRIFT === '1') {",
  "    process.stderr.write('gen-types --check: env.db.ts is STALE\\n'); process.exit(1);",
  "  }",
  "  process.stdout.write('gen-types --check: OK\\n'); process.exit(0);",
  "}",
  "fs.mkdirSync(out, { recursive: true });",
  "fs.writeFileSync(path.join(out, 'env.db.ts'), '// stub env.db.ts\\n');",
  "fs.writeFileSync(path.join(out, 'schema.runtime.json'), '{}\\n');",
  "",
].join("\n");

async function withStub(
  extraEnv: Record<string, string>,
  fn: (ctx: { root: string; cliPath: string; argsLog: string }) => Promise<void>
): Promise<void> {
  const fx = await makeFixture({
    "migrations/20240617123000_notes.ts": "export function up() {}\n",
    "stub-gentypes.js": STUB_GENTYPES,
  });
  const cliPath = join(fx.root, "stub-gentypes.js");
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

describe("gen-types wiring (P3)", () => {
  // (a) stub-CLI unit: a migration regeneration invokes gen-types with the right
  //     args and emits into the configured genTypesOut.
  test("genTypesViaCli shells `gen-types --dir … --out …` and emits into genTypesOut", async () => {
    await withStub({}, async ({ root, cliPath, argsLog }) => {
      const result = genTypesViaCli({
        root,
        cliPath,
        genTypesOut: "generated/zeroship",
      });
      assert.deepEqual(result, { status: "ok", cli: cliPath });

      const args = JSON.parse(await fs.readFile(argsLog, "utf8")) as string[];
      assert.equal(args[0], "gen-types");
      const di = args.indexOf("--dir");
      assert.ok(di >= 0, "passes --dir");
      assert.equal(args[di + 1], join(root, "migrations"), "--dir is the resolved migrations dir");
      const oi = args.indexOf("--out");
      assert.ok(oi >= 0, "passes --out");
      assert.equal(
        args[oi + 1],
        join(root, "generated/zeroship"),
        "--out is the resolved genTypesOut dir"
      );
      assert.ok(!args.includes("--check"), "a write (non-check) run has no --check");

      // The artifacts really landed in the configured dir.
      const dts = await fs.readFile(join(root, "generated/zeroship/env.db.ts"), "utf8");
      assert.match(dts, /stub env\.db\.ts/);
      await fs.access(join(root, "generated/zeroship/schema.runtime.json"));
    });
  });

  test("genTypesOut defaults to generated/zeroship (committed, OUTSIDE the tsc program)", async () => {
    // The default is NOT under src/ (would enter `include`) and NOT .zeroship/
    // (gitignored). This encodes the P3 deferral: emit-and-commit, do NOT
    // activate the types until P5.
    assert.equal(GEN_TYPES_OUT_DEFAULT, "generated/zeroship");
    await withStub({}, async ({ root, cliPath, argsLog }) => {
      genTypesViaCli({ root, cliPath }); // no genTypesOut → default
      const args = JSON.parse(await fs.readFile(argsLog, "utf8")) as string[];
      const oi = args.indexOf("--out");
      assert.equal(args[oi + 1], join(root, "generated/zeroship"));
    });
  });

  // (c) --check drift: a divergence exits non-zero → genTypesViaCli THROWS.
  test("--check drift: a stale committed artifact makes genTypesViaCli throw (CI gate)", async () => {
    await withStub({ ZSTUB_DRIFT: "1" }, async ({ root, cliPath }) => {
      assert.throws(
        () => genTypesViaCli({ root, cliPath, check: true, requireBinary: true }),
        (err: Error) => {
          assert.match(err.message, /exited 1/);
          assert.match(err.message, /STALE/);
          assert.match(err.message, /--check/);
          return true;
        }
      );
    });
  });

  test("--check OK: in-sync artifacts return { status: ok } (no throw)", async () => {
    await withStub({}, async ({ root, cliPath }) => {
      const result = genTypesViaCli({ root, cliPath, check: true, requireBinary: true });
      assert.equal(result.status, "ok");
    });
  });

  // (d) missing binary in DEV warns-and-noops; in CI (requireBinary) it throws.
  test("missing binary in dev → { status: skipped } (no throw)", async () => {
    const fx = await makeFixture({
      "migrations/20240617123000_notes.ts": "export function up() {}\n",
    });
    // Ensure no env override + no node_modules/.bin in the fixture.
    const savedEnv = process.env.ZEROSHIP_MIGRATE_JS_BIN;
    delete process.env.ZEROSHIP_MIGRATE_JS_BIN;
    try {
      const result = genTypesViaCli({ root: fx.root }); // dev: requireBinary defaults false
      assert.equal(result.status, "skipped");
      if (result.status === "skipped") {
        assert.match(result.reason, /not found/);
      }
    } finally {
      if (savedEnv === undefined) delete process.env.ZEROSHIP_MIGRATE_JS_BIN;
      else process.env.ZEROSHIP_MIGRATE_JS_BIN = savedEnv;
      await fx.cleanup();
    }
  });

  // MED-1 (review fix): with `requireBinary` and a GENUINELY-absent binary (not in
  // env, not in node_modules/.bin, not on $PATH) the prod gate must still HARD-FAIL
  // — now via the bare-name spawn ENOENT (not a silent pass). We scrub $PATH so the
  // bare name truly cannot resolve.
  test("missing binary with requireBinary (CI gate) → throws (bare-name spawn ENOENT)", async () => {
    const fx = await makeFixture({
      "migrations/20240617123000_notes.ts": "export function up() {}\n",
    });
    const savedEnv = process.env.ZEROSHIP_MIGRATE_JS_BIN;
    const savedPath = process.env.PATH;
    delete process.env.ZEROSHIP_MIGRATE_JS_BIN;
    process.env.PATH = ""; // no $PATH dir contains `zeroship-migrate-js`
    try {
      assert.throws(
        () => genTypesViaCli({ root: fx.root, check: true, requireBinary: true }),
        (err: Error) => {
          // The bare PATH name was spawned and failed to resolve.
          assert.match(err.message, /failed to invoke the gen-types CLI/);
          assert.match(err.message, /zeroship-migrate-js/);
          return true;
        }
      );
    } finally {
      if (savedEnv === undefined) delete process.env.ZEROSHIP_MIGRATE_JS_BIN;
      else process.env.ZEROSHIP_MIGRATE_JS_BIN = savedEnv;
      process.env.PATH = savedPath;
      await fx.cleanup();
    }
  });

  // MED-1 (review fix): the prod/CI drift gate must MIRROR `recordViaCli` and the
  // dev-server's binary resolution, both of which fall through to a BARE PATH name.
  // Without this, a host with the binary on `$PATH` (the natural `cargo install`
  // location) but NOT in node_modules/.bin and no env var would let `record`/`build`
  // succeed while the prod `--check` gate spuriously hard-fails "not found".
  test("resolveGenTypesCli: requireBinary falls through to the bare PATH name (mirror of recordViaCli)", async () => {
    // A fixture with NO node_modules/.bin and (below) NO env override.
    const fx = await makeFixture({ "x.txt": "x" });
    const savedEnv = process.env.ZEROSHIP_MIGRATE_JS_BIN;
    delete process.env.ZEROSHIP_MIGRATE_JS_BIN;
    try {
      // Dev (requireBinary=false): null → graceful no-op.
      assert.equal(resolveGenTypesCli(fx.root), null);
      assert.equal(resolveGenTypesCli(fx.root, undefined, false), null);
      // Prod/CI (requireBinary=true): the bare PATH name, so spawnSync resolves
      // it via $PATH exactly as record/build do — NOT null (which would spuriously
      // hard-fail on a $PATH-installed binary).
      assert.equal(
        resolveGenTypesCli(fx.root, undefined, true),
        "zeroship-migrate-js"
      );
    } finally {
      if (savedEnv === undefined) delete process.env.ZEROSHIP_MIGRATE_JS_BIN;
      else process.env.ZEROSHIP_MIGRATE_JS_BIN = savedEnv;
      await fx.cleanup();
    }
  });

  // MED-1 end-to-end: with the binary ONLY on $PATH (not node_modules/.bin, no env
  // var), the prod drift gate (`requireBinary:true`) must SUCCEED — proving the
  // bare-name fall-through resolves a $PATH-installed binary. Pre-fix this threw
  // "zeroship-migrate-js not found" (resolution returned null) — a build-breaking
  // false negative on a legitimately-configured machine.
  test("genTypesViaCli(requireBinary): resolves a $PATH-installed binary (no node_modules/.bin, no env)", async () => {
    const fx = await makeFixture({
      "migrations/20240617123000_notes.ts": "export function up() {}\n",
      // Stub binary placed under a dir we prepend to $PATH, named EXACTLY the
      // bare CLI name so $PATH resolution finds it.
      "pathbin/zeroship-migrate-js": STUB_GENTYPES,
    });
    const pathBin = join(fx.root, "pathbin");
    await fs.chmod(join(pathBin, "zeroship-migrate-js"), 0o755);
    const savedPath = process.env.PATH;
    const savedEnv = process.env.ZEROSHIP_MIGRATE_JS_BIN;
    const savedArgsLog = process.env.ARGS_LOG;
    delete process.env.ZEROSHIP_MIGRATE_JS_BIN; // force the bare-name fall-through
    process.env.PATH = `${pathBin}:${savedPath ?? ""}`;
    process.env.ARGS_LOG = join(fx.root, "args.json");
    try {
      const result = genTypesViaCli({
        root: fx.root,
        genTypesOut: "generated/zeroship",
        check: true,
        requireBinary: true,
      });
      assert.equal(result.status, "ok");
      assert.equal((result as { cli: string }).cli, "zeroship-migrate-js");
      // It really ran the $PATH binary with the gen-types --check args.
      const args = JSON.parse(
        await fs.readFile(join(fx.root, "args.json"), "utf8")
      ) as string[];
      assert.equal(args[0], "gen-types");
      assert.ok(args.includes("--check"));
    } finally {
      process.env.PATH = savedPath;
      if (savedEnv === undefined) delete process.env.ZEROSHIP_MIGRATE_JS_BIN;
      else process.env.ZEROSHIP_MIGRATE_JS_BIN = savedEnv;
      if (savedArgsLog === undefined) delete process.env.ARGS_LOG;
      else process.env.ARGS_LOG = savedArgsLog;
      await fx.cleanup();
    }
  });

  test("resolveGenTypesCli: explicit cliPath > env > node_modules/.bin > null", async () => {
    const fx = await makeFixture({
      "node_modules/.bin/zeroship-migrate-js": "#!/bin/sh\n",
    });
    await fs.chmod(join(fx.root, "node_modules/.bin/zeroship-migrate-js"), 0o755);
    const savedEnv = process.env.ZEROSHIP_MIGRATE_JS_BIN;
    try {
      // explicit wins
      delete process.env.ZEROSHIP_MIGRATE_JS_BIN;
      assert.equal(resolveGenTypesCli(fx.root, "/explicit/cli"), "/explicit/cli");
      // env next
      process.env.ZEROSHIP_MIGRATE_JS_BIN = "/from/env";
      assert.equal(resolveGenTypesCli(fx.root), "/from/env");
      // node_modules/.bin next
      delete process.env.ZEROSHIP_MIGRATE_JS_BIN;
      assert.equal(
        resolveGenTypesCli(fx.root),
        join(fx.root, "node_modules/.bin/zeroship-migrate-js")
      );
      // null when absent everywhere
      const empty = await makeFixture({ "x.txt": "x" });
      try {
        assert.equal(resolveGenTypesCli(empty.root), null);
      } finally {
        await empty.cleanup();
      }
    } finally {
      if (savedEnv === undefined) delete process.env.ZEROSHIP_MIGRATE_JS_BIN;
      else process.env.ZEROSHIP_MIGRATE_JS_BIN = savedEnv;
      await fx.cleanup();
    }
  });
});

// (a, dev wiring) — drive the dev-server plugin hooks DIRECTLY (no full Vite boot):
// a change under the migrations dir must invoke `gen-types` via the stub CLI and
// emit the artifacts; a change OUTSIDE the migrations dir must NOT.
describe("dev-server hotUpdate → gen-types (P3)", () => {
  type AnyFn = (...a: any[]) => any;
  // Resolve a plugin hook that Vite allows to be either a bare fn or { handler }.
  function hook(plugin: any, name: string): AnyFn {
    const h = plugin?.[name];
    return typeof h === "function" ? h.bind(plugin) : h?.handler?.bind(plugin);
  }

  async function drive(
    root: string,
    cliPath: string,
    changedFile: string,
    // MED-2 made configureServer regenerate on boot. To isolate the hotUpdate
    // branch, clear the boot-regen artifacts (args log + generated dir) before
    // firing hotUpdate.
    clearBootRegen = true
  ): Promise<void> {
    const state: TransformState = {
      serverFunctionMap: new Map(),
      discoveredProcedures: [],
    };
    const plugins = devServerPlugin(
      { migrations: { cliPath, dir: "migrations", genTypesOut: "generated/zeroship" } } as any,
      state
    );
    const [envPlugin, devPlugin] = plugins;
    // configResolved (on the environment plugin) sets root + isDev (serve).
    hook(envPlugin, "configResolved")({ root, command: "serve" });
    // configureServer wires the watcher + resolves migrationsAbs; feed a minimal
    // server stub (watcher.add + middlewares.use are the only surfaces touched
    // before our early returns / branches).
    const server = {
      watcher: { add() {} },
      middlewares: { use() {} },
      environments: {},
      config: { root },
      httpServer: { once() {} },
    };
    hook(devPlugin, "configureServer")(server);
    // configureServer ran the boot regen (MED-2); wait for its spawnSync FS, then
    // wipe its outputs so the subsequent hotUpdate assertion measures hotUpdate alone.
    await new Promise((r) => setTimeout(r, 10));
    if (clearBootRegen) {
      await fs.rm(join(root, "args.json"), { force: true });
      await fs.rm(join(root, "generated"), { recursive: true, force: true });
    }
    // Now fire the hotUpdate for the changed file.
    hook(devPlugin, "hotUpdate")({ file: changedFile });
    // gen-types runs synchronously (spawnSync) inside hotUpdate; give the FS a tick.
    await new Promise((r) => setTimeout(r, 10));
  }

  test("a change under the migrations dir regenerates env.db.ts via the stub CLI", async () => {
    await withStub({}, async ({ root, cliPath, argsLog }) => {
      await drive(root, cliPath, join(root, "migrations", "20240617123000_notes.ts"));
      // The stub logged a gen-types invocation.
      const args = JSON.parse(await fs.readFile(argsLog, "utf8")) as string[];
      assert.equal(args[0], "gen-types");
      // The artifacts were emitted into generated/zeroship.
      await fs.access(join(root, "generated/zeroship/env.db.ts"));
      await fs.access(join(root, "generated/zeroship/schema.runtime.json"));
    });
  });

  // MED-2 (review fix): an INITIAL regen must run on dev-server boot (in
  // configureServer), not only on a subsequent change. Without it, a migration
  // changed while the dev server was down leaves env.db.ts stale until the next
  // save. Assert the artifacts exist after configureServer alone — NO hotUpdate fired.
  test("configureServer regenerates env.db.ts on boot (no hotUpdate needed)", async () => {
    await withStub({}, async ({ root, cliPath, argsLog }) => {
      const state: TransformState = {
        serverFunctionMap: new Map(),
        discoveredProcedures: [],
      };
      const plugins = devServerPlugin(
        { migrations: { cliPath, dir: "migrations", genTypesOut: "generated/zeroship" } } as any,
        state
      );
      const [envPlugin, devPlugin] = plugins as any[];
      const hookFn = (plugin: any, name: string): AnyFn => {
        const h = plugin?.[name];
        return typeof h === "function" ? h.bind(plugin) : h?.handler?.bind(plugin);
      };
      hookFn(envPlugin, "configResolved")({ root, command: "serve" });
      const server = {
        watcher: { add() {} },
        middlewares: { use() {} },
        environments: {},
        config: { root },
        httpServer: { once() {} },
      };
      // configureServer ONLY — deliberately NO hotUpdate.
      hookFn(devPlugin, "configureServer")(server);
      await new Promise((r) => setTimeout(r, 10));
      // gen-types ran on boot.
      const args = JSON.parse(await fs.readFile(argsLog, "utf8")) as string[];
      assert.equal(args[0], "gen-types");
      await fs.access(join(root, "generated/zeroship/env.db.ts"));
      await fs.access(join(root, "generated/zeroship/schema.runtime.json"));
    });
  });

  test("a change OUTSIDE the migrations dir does NOT invoke gen-types", async () => {
    await withStub({}, async ({ root, cliPath, argsLog }) => {
      await drive(root, cliPath, join(root, "src", "app.ts"));
      // No gen-types ran → no args log, no artifacts.
      await assert.rejects(() => fs.access(argsLog));
      await assert.rejects(() => fs.access(join(root, "generated/zeroship/env.db.ts")));
    });
  });
});

// (b) REAL-binary e2e — shell the ACTUALLY-BUILT `zeroship-migrate-js` over a real
// recorded migration and assert `env.db.ts` + `schema.runtime.json` regenerate from
// the fold. Gated on ZEROSHIP_MIGRATE_JS_BIN (or the cargo default-target path); when
// the env var is set the test HARD-FAILS if the binary is missing (faithful-e2e).
describe("gen-types against the REAL zeroship-migrate-js binary (faithful e2e)", () => {
  const STEM = "20240617123000_real_notes";
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

  function realCliPath(): string {
    const fromEnv = process.env.ZEROSHIP_MIGRATE_JS_BIN;
    if (fromEnv) return fromEnv;
    // sdks/vite-plugin/test/gen-types.test.ts → up 3 = repo root.
    const repoRoot = resolve(import.meta.dirname, "..", "..", "..");
    return join(repoRoot, "target", "debug", "zeroship-migrate-js");
  }

  test("real gen-types folds a recorded migration → env.db.ts + schema.runtime.json", async () => {
    const cliPath = realCliPath();
    const explicit = !!process.env.ZEROSHIP_MIGRATE_JS_BIN;
    let cliExists = false;
    try {
      await fs.access(cliPath);
      cliExists = true;
    } catch {
      cliExists = false;
    }
    if (!cliExists) {
      if (explicit) {
        throw new Error(
          `ZEROSHIP_MIGRATE_JS_BIN is set (${cliPath}) but the binary does not exist — ` +
            "build it: cargo build -p zeroship-migrate-js --bins"
        );
      }
      console.warn(
        `[skip] real gen-types e2e: ${cliPath} not built ` +
          "(cargo build -p zeroship-migrate-js --bins, or set ZEROSHIP_MIGRATE_JS_BIN)"
      );
      return;
    }

    const fx = await makeFixture({ [`migrations/${STEM}.ts`]: REAL_TS });
    try {
      // 1. Record the migration (`.ts` → committed `.ir.json`) via `build`.
      const { spawnSync } = await import("node:child_process");
      const rec = spawnSync(
        cliPath,
        ["build", "--dir", join(fx.root, "migrations"), "--owner-app", "app_e2e"],
        { encoding: "utf8" }
      );
      assert.equal(rec.status, 0, `record failed: ${rec.stderr}`);
      await fs.access(join(fx.root, "migrations", `${STEM}.ir.json`));

      // 2. Generate types from the migration fold via the plugin helper.
      const result = genTypesViaCli({
        root: fx.root,
        cliPath,
        genTypesOut: "generated/zeroship",
      });
      assert.equal(result.status, "ok");

      // 3. Both artifacts regenerated; env.db.ts is a real generated module.
      const dts = await fs.readFile(
        join(fx.root, "generated/zeroship/env.db.ts"),
        "utf8"
      );
      assert.match(dts, /GENERATED by `zeroship-migrate-js gen-types`/);
      assert.match(dts, /import \{ t, type Db \} from "@zeroship\/db";/);
      assert.match(dts, /real_notes:/, "the folded collection appears in the schema");
      assert.match(dts, /db: Db<typeof schema>;/);

      const runtime = JSON.parse(
        await fs.readFile(join(fx.root, "generated/zeroship/schema.runtime.json"), "utf8")
      ) as { version?: unknown; collections?: Record<string, unknown> };
      assert.equal(runtime.version, 1);
      assert.ok(
        runtime.collections &&
          Object.prototype.hasOwnProperty.call(runtime.collections, "real_notes"),
        `schema.runtime.json must carry the folded collection; got ${JSON.stringify(runtime)}`
      );

      // 4. --check now passes (committed == regenerated).
      const checkOk = genTypesViaCli({
        root: fx.root,
        cliPath,
        genTypesOut: "generated/zeroship",
        check: true,
        requireBinary: true,
      });
      assert.equal(checkOk.status, "ok");

      // 5. (LOW-1 review fix) REAL drift → throw, end-to-end. Tamper the committed
      //    env.db.ts and assert the REAL CLI's `--check` exits non-zero and
      //    `genTypesViaCli` THROWS (the production drift gate, locked by the suite
      //    against the actual binary — not only the stub).
      const dtsPath = join(fx.root, "generated/zeroship/env.db.ts");
      await fs.appendFile(dtsPath, "\n// drift: a hand-edit the migrations don't produce\n");
      assert.throws(
        () =>
          genTypesViaCli({
            root: fx.root,
            cliPath,
            genTypesOut: "generated/zeroship",
            check: true,
            requireBinary: true,
          }),
        (err: Error) => {
          // Non-zero exit surfaced as a throw (exact exit code is the CLI's; we
          // assert the gate fired, not the precise number).
          assert.match(err.message, /gen-types CLI/);
          assert.match(err.message, /--check/);
          return true;
        }
      );
    } finally {
      await fx.cleanup();
    }
  });
});
