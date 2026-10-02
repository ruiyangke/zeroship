/**
 * The dev archive: the local deployment `pnpm dev` packs for its workflow
 * host, whose worker retained workflow runs execute.
 *
 * It is compiled the way `vite build` compiles the worker, from the dev
 * server's own Vite config: the fixture's config file carries an app plugin
 * that only it can load `.greeting` files with, and packages that resolve to
 * a different build under each set of conditions.
 */
import { test } from "node:test";
import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { promises as fs } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { tmpdir } from "node:os";
import { fileURLToPath, pathToFileURL } from "node:url";
import { createHash } from "node:crypto";
import { zstdDecompressSync } from "node:zlib";
import { extract } from "tar";
import { buildDevBundle, type DevBuildConfig } from "../src/dev-bundle.js";
import { zeroshipPlugins } from "../src/plugins.js";
import { defaultProjectConfig } from "../src/project-config/index.js";

const sdkRoot = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const PLUGINS = join(sdkRoot, "src/plugins.ts");
const descriptor = JSON.stringify({ version: 2, collections: {} });

const entry = `
"use server";
import { Workflow } from "@zeroship/workflows";
import { schedule, every } from "@zeroship/workflows/schedule";
import { prefix } from "./dependency.js";
import greeting from "./hello.greeting";
import { picked } from "pick-me";
import { field } from "pick-by-field";
export class Example extends Workflow {
  async run() {
    const { suffix } = await import("./lazy.js");
    return prefix + suffix;
  }
}
// What the app config contributes, and what the environment reads.
export class Probe extends Workflow {
  async run() {
    return [
      greeting,
      picked,
      field,
      String(globalThis.process.env.PROBE_KEY),
      String(process.env.NODE_ENV),
      String(globalThis.process.env.NODE_ENV),
    ];
  }
}
export const periodic = schedule({
  name: "periodic",
  workflow: Example,
  schedule: every.hour(),
});
`;

const VITE_CONFIG = `import { zeroshipPlugins } from ${JSON.stringify(PLUGINS)};

function greeting() {
  return {
    name: "fixture:greeting",
    transform(code, id) {
      if (!id.endsWith(".greeting")) return null;
      return { code: "export default " + JSON.stringify(code.trim()) + ";", map: null, moduleType: "js" };
    },
  };
}

export default {
  logLevel: "silent",
  plugins: [greeting(), ...zeroshipPlugins({}, {})],
};
`;

async function fixture(): Promise<string> {
  const root = await fs.mkdtemp(join(tmpdir(), "zs-app-bundle-"));
  const write = async (path: string, body: string) => {
    await fs.mkdir(dirname(join(root, path)), { recursive: true });
    await fs.writeFile(join(root, path), body);
  };
  await fs.mkdir(join(root, "node_modules"));
  await fs.symlink(join(sdkRoot, "node_modules/@zeroship"), join(root, "node_modules/@zeroship"), "dir");
  await write("vite.config.mjs", VITE_CONFIG);
  await write("src/server.ts", entry);
  await write("src/dependency.js", 'import { basename } from "node:path"; export const prefix = basename("/values/original:");');
  await write("src/lazy.js", 'export const suffix = "lazy";');
  await write("src/hello.greeting", "hello from an app plugin\n");
  // One build per condition a worker could resolve.
  await write("node_modules/pick-me/package.json", JSON.stringify({
    name: "pick-me",
    type: "module",
    exports: { ".": { worker: "./worker.js", browser: "./browser.js", default: "./default.js" } },
  }));
  for (const build of ["worker", "browser", "default"]) {
    await write(`node_modules/pick-me/${build}.js`, `export const picked = ${JSON.stringify(build)};\n`);
  }
  await write("node_modules/pick-by-field/package.json", JSON.stringify({
    name: "pick-by-field", browser: "./browser.js", module: "./module.js", main: "./main.js",
  }));
  for (const build of ["browser", "module"]) {
    await write(`node_modules/pick-by-field/${build}.js`, `export const field = ${JSON.stringify(build)};\n`);
  }
  await write("node_modules/pick-by-field/main.js", `exports.field = "main";\n`);
  return root;
}

/** The dev server's config source, as `configureServer` reads it. */
function devBuildConfig(root: string): DevBuildConfig {
  const configFile = join(root, "vite.config.mjs");
  return {
    root,
    configFile,
    inlineConfig: { root, configFile, configLoader: "native", logLevel: "silent" },
    mode: "development",
    plugins: () => zeroshipPlugins({}, {}),
  };
}

function options(root: string) {
  return {
    config: devBuildConfig(root),
    project: defaultProjectConfig(),
    databases: [{
      label: "main",
      id: "dbs_03evr3oqx1200yyd6zj2cebfw",
      primary: true,
      migrations: "migrations",
      descriptor,
    }],
  };
}

interface Manifest {
  worker: { entry: string; modules: Record<string, string> };
  workflows?: string[];
  schedules?: { name: string; workflowName: string }[];
  runtime_descriptor?: { label: string; database_id: string; primary: boolean; hash: string }[];
}

async function unpack(archive: Buffer, output: string): Promise<Manifest> {
  await fs.mkdir(output);
  const tar = join(output, "bundle.tar");
  await fs.writeFile(tar, zstdDecompressSync(archive));
  await extract({ file: tar, cwd: output });
  const manifest: Manifest = JSON.parse(await fs.readFile(join(output, "manifest.json"), "utf8"));
  assert.ok(manifest.worker);
  assert.ok(manifest.worker.entry in manifest.worker.modules);
  await fs.writeFile(join(output, "package.json"), '{"type":"module"}');
  for (const [module, hash] of Object.entries(manifest.worker.modules)) {
    const bytes = await fs.readFile(join(output, "blobs", hash));
    assert.equal(createHash("sha256").update(bytes).digest("hex"), hash);
    const path = join(output, module);
    await fs.mkdir(dirname(path), { recursive: true });
    await fs.writeFile(path, bytes);
  }
  return manifest;
}

/**
 * Run a retained workflow of an unpacked archive in a fresh Node process, as
 * the workflow host runs it: the process's own `NODE_ENV` is `production`, so
 * a `NODE_ENV` the build did not replace reads `production`, and it carries
 * the `PROBE_KEY` a runtime would supply.
 */
function runRetained(path: string, manifest: Manifest, workflow: string): unknown {
  const code = `globalThis.__zs_env = () => ({});
const m = await import(${JSON.stringify(pathToFileURL(join(path, manifest.worker.entry)).href)});
process.stdout.write(JSON.stringify(await new m[${JSON.stringify(workflow)}]().run()));`;
  const run = spawnSync(process.execPath, ["--input-type=module", "-e", code], {
    cwd: path,
    env: { NODE_ENV: "production", PROBE_KEY: "from the runtime env" },
    encoding: "utf8",
  });
  assert.equal(run.status, 0, run.stderr);
  return JSON.parse(run.stdout);
}

test("the dev archive compiles server code from the dev server's Vite config", async () => {
  const root = await fixture();
  const retained = await fs.mkdtemp(join(tmpdir(), "zs-retained-workflows-"));
  try {
    const bundle = await buildDevBundle(options(root));
    const path = join(retained, "probe");
    const manifest = await unpack(bundle.archive, path);
    assert.deepEqual(manifest.workflows, ["Example", "Probe"]);
    // The app plugin loads the greeting; packages resolve by the documented
    // conditions; the runtime's environment stays live in every spelling;
    // and `NODE_ENV` is the dev tier's.
    assert.deepEqual(runRetained(path, manifest, "Probe"), [
      "hello from an app plugin",
      "worker",
      "module",
      "from the runtime env",
      "development",
      "development",
    ]);
  } finally {
    await fs.rm(root, { recursive: true, force: true });
    await fs.rm(retained, { recursive: true, force: true });
  }
});

test("the dev archive's zeroship plugins come from the dev server's options, not the config file's", async () => {
  // The config file's own copy of zeroship's plugins names a project file
  // that does not exist, which fails any build it takes part in. The dev
  // server's options name none.
  const root = await fixture();
  const retained = await fs.mkdtemp(join(tmpdir(), "zs-retained-workflows-"));
  try {
    await fs.writeFile(join(root, "vite.config.mjs"), VITE_CONFIG.replace(
      "...zeroshipPlugins({}, {})",
      "...zeroshipPlugins({ configPath: \"absent.jsonc\" }, {})",
    ));
    const manifest = await unpack((await buildDevBundle(options(root))).archive, join(retained, "probe"));
    assert.deepEqual(manifest.workflows, ["Example", "Probe"]);
  } finally {
    await fs.rm(root, { recursive: true, force: true });
    await fs.rm(retained, { recursive: true, force: true });
  }
});

test("local workflow bundles retain dependencies and rebuild declarations independently", async () => {
  const root = await fixture();
  const retained = await fs.mkdtemp(join(tmpdir(), "zs-retained-workflows-"));
  const serverEntry = join(root, "src/server.ts");
  try {
    const opts = options(root);
    const original = await buildDevBundle(opts);
    assert.ok(original.dependencies.includes(serverEntry));
    assert.ok(original.dependencies.includes(join(root, "src/dependency.js")));
    assert.ok(original.dependencies.includes(join(root, "src/lazy.js")));
    assert.ok(original.dependencies.includes(join(root, "src/hello.greeting")));
    const oldPath = join(retained, "original");
    const manifest = await unpack(original.archive, oldPath);
    assert.deepEqual(manifest.workflows, ["Example", "Probe"]);
    assert.deepEqual(manifest.schedules?.map(s => [s.name, s.workflowName]), [["periodic", "Example"]]);
    assert.ok(manifest.runtime_descriptor);
    assert.deepEqual(
      manifest.runtime_descriptor.map(e => [e.label, e.database_id, e.primary]),
      [["main", "dbs_03evr3oqx1200yyd6zj2cebfw", true]],
    );
    assert.equal(
      await fs.readFile(join(oldPath, "blobs", manifest.runtime_descriptor[0]!.hash), "utf8"),
      descriptor,
    );

    await fs.writeFile(join(root, "src/dependency.js"), 'import { basename } from "node:path"; export const prefix = basename("/values/replacement:");');
    const replacement = await buildDevBundle(opts);
    const newPath = join(retained, "replacement");
    const updated = await unpack(replacement.archive, newPath);

    await fs.writeFile(serverEntry, "export default { fetch() { return new Response('no workflows'); } };");
    const removed = await buildDevBundle({ ...opts, databases: [] });
    const absent = await unpack(removed.archive, join(retained, "removed"));
    assert.equal(absent.workflows, undefined);
    assert.equal(absent.schedules, undefined);
    assert.equal(absent.runtime_descriptor, undefined);
    assert.ok(!removed.dependencies.includes(join(root, "src/lazy.js")));
    assert.deepEqual(await fs.readdir(join(root, ".zeroship")), []);
    // The archive is staged under `.zeroship`: the app's own build output is
    // never written.
    await assert.rejects(fs.access(join(root, "dist")), { code: "ENOENT" });

    // Loading from retained archives must not consult the editable project.
    await fs.rm(root, { recursive: true, force: true });
    // The native host supplies this primitive; the archive carries no host env.
    Object.defineProperty(globalThis, "__zs_env", { value: () => ({}), configurable: true });
    try {
      const oldModule = await import(pathToFileURL(join(oldPath, manifest.worker.entry)).href);
      const newModule = await import(pathToFileURL(join(newPath, updated.worker.entry)).href);
      assert.equal(typeof oldModule.Example, "function");
      assert.equal(typeof newModule.Example, "function");
      assert.equal(Object.hasOwn(oldModule.default, "workflows"), false);
      assert.equal(Object.hasOwn(newModule.default, "workflows"), false);
      assert.equal(await new oldModule.Example().run(), "original:lazy");
      assert.equal(await new newModule.Example().run(), "replacement:lazy");
    } finally {
      Reflect.deleteProperty(globalThis, "__zs_env");
    }
  } finally {
    await fs.rm(root, { recursive: true, force: true });
    await fs.rm(retained, { recursive: true, force: true });
  }
});

test("failed workflow builds remove staging and never produce a partial archive", async () => {
  const root = await fixture();
  try {
    const opts = options(root);
    const serverEntry = join(root, "src/server.ts");
    await fs.writeFile(serverEntry, 'import "./missing.js";');
    await assert.rejects(buildDevBundle(opts), /missing/);
    assert.deepEqual(await fs.readdir(join(root, ".zeroship")), []);
    await fs.writeFile(serverEntry, entry);
    await assert.rejects(
      buildDevBundle({ ...opts, databases: [{ ...opts.databases[0]!, descriptor: "{}" }] }),
      /runtime_descriptor/,
    );
    assert.deepEqual(await fs.readdir(join(root, ".zeroship")), []);
  } finally {
    await fs.rm(root, { recursive: true, force: true });
  }
});

test("a worker pass that fails early still names the sources the discovery pass read", async () => {
  // The discovery pass reads the whole server graph and succeeds; the worker
  // pass fails before it reads anything. The greeting, which no extension
  // marks as a source, must stay watched.
  const root = await fixture();
  try {
    await fs.writeFile(join(root, "vite.config.mjs"), VITE_CONFIG.replace(
      "plugins: [greeting(), ",
      `plugins: [greeting(), {
    name: "fixture:worker-pass-fails",
    buildStart() {
      if (this.environment?.name === "zeroship" && this.environment.config.build.write !== false) {
        throw new Error("the worker pass failed");
      }
    },
  }, `,
    ));
    const failure = await buildDevBundle(options(root)).then(
      () => assert.fail("the build must fail"),
      (error: unknown) => error as { message: string; dependencies?: string[] },
    );
    assert.match(failure.message, /the worker pass failed/);
    assert.ok(failure.dependencies?.includes(join(root, "src/hello.greeting")), JSON.stringify(failure.dependencies));
  } finally {
    await fs.rm(root, { recursive: true, force: true });
  }
});

test("a failed build names the sources it read, so fixing one of them can rebuild", async () => {
  // The dev server watches what a build read. A build that fails reads a
  // partial graph, which still includes the source that broke it.
  const root = await fixture();
  try {
    await fs.writeFile(join(root, "src/hello.greeting"), "");
    await fs.writeFile(join(root, "vite.config.mjs"), VITE_CONFIG.replace(
      "if (!id.endsWith(\".greeting\")) return null;",
      "if (!id.endsWith(\".greeting\")) return null;\n      if (code.trim() === \"\") throw new Error(\"an empty greeting\");",
    ));
    const failure = await buildDevBundle(options(root)).then(
      () => assert.fail("the build must fail"),
      (error: unknown) => error as { message: string; dependencies?: string[] },
    );
    assert.match(failure.message, /an empty greeting/);
    assert.ok(failure.dependencies?.includes(join(root, "src/hello.greeting")), JSON.stringify(failure.dependencies));
  } finally {
    await fs.rm(root, { recursive: true, force: true });
  }
});
