/** Built and dev imports must reach the host-owned zeroship module. */
import { test } from "node:test";
import assert from "node:assert/strict";
import { promises as fs } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";
import * as asyncHooks from "node:async_hooks";
import * as buffer from "node:buffer";
import * as crypto from "node:crypto";
import * as path from "node:path";
import * as util from "node:util";
import { ModuleRunner } from "vite/module-runner";
import { build, createServer, parseAst, searchForWorkspaceRoot, type DevEnvironment, type Plugin } from "vite";

import { zeroshipModulePlugin } from "../src/zeroship-module.js";
import { nodeCompatPlugin } from "../src/node-compat.js";
import { createZeroshipEnvironmentOptions } from "../src/environment.js";
import { zeroshipEvaluator } from "../src/dev-bootstrap/evaluator.js";
import { OFF_THREAD_SERIALIZER_PACKAGE, OFF_THREAD_SERIALIZER_SOURCE } from "./helpers/off-thread-serializer.js";

const ENTRY = `
import { env } from "zeroship";
export default { fetch() { return new Response(env.db?.name ?? "missing"); } };
`;

async function fixture() {
  const root = await fs.mkdtemp(join(tmpdir(), "zs-native-env-"));
  const entry = join(root, "entry.js");
  const packageRoot = join(root, "node_modules/zeroship");
  await fs.mkdir(packageRoot, { recursive: true });
  await fs.writeFile(join(packageRoot, "package.json"), JSON.stringify({
    name: "zeroship", type: "module", exports: "./index.js",
  }));
  const hostModule = join(packageRoot, "index.js");
  await fs.writeFile(hostModule, "export const env = {};\n");
  await fs.writeFile(entry, ENTRY);
  return { root, entry, hostModule };
}

async function buildAndExecute(plugins: Plugin[]) {
  const { root, entry, hostModule } = await fixture();
  try {
    const result = await build({
      root, configFile: false, logLevel: "silent", plugins,
      ssr: { noExternal: true, target: "webworker" },
      build: { ssr: entry, write: false, minify: false },
    });
    assert.ok(!Array.isArray(result) && "output" in result);
    const chunks = result.output.filter(output => output.type === "chunk" && output.isEntry);
    assert.equal(chunks.length, 1, "the fixture must produce an entry chunk");
    const chunk = chunks[0];
    assert.ok(chunk.type === "chunk");
    assert.deepEqual(chunk.exports, ["default"]);
    // Simulate the host supplying its module after the creator artifact is built.
    await fs.writeFile(hostModule, 'export const env = Object.freeze({ db: { name: "host" } });\n');
    const artifact = join(root, "artifact.mjs");
    await fs.writeFile(artifact, chunk.code);
    const app = await import(pathToFileURL(artifact).href);
    return { imports: chunk.imports, body: await app.default.fetch().text() };
  } finally {
    await fs.rm(root, { recursive: true, force: true });
  }
}

test("built artifacts preserve the host module import and use its environment", async () => {
  assert.deepEqual(await buildAndExecute([zeroshipModulePlugin()]), {
    imports: ["zeroship"], body: "host",
  });
  assert.deepEqual(await buildAndExecute([]), { imports: [], body: "missing" });
});

test("the dev environment delegates zeroship to the native module evaluator", async () => {
  const { root, entry } = await fixture();
  const server = await createServer({
    root, configFile: false, logLevel: "silent", plugins: [zeroshipModulePlugin()],
    server: { middlewareMode: true, watch: null },
    environments: { zeroship: createZeroshipEnvironmentOptions(entry) },
  });
  try {
    const result = await server.environments.zeroship.fetchModule("zeroship", entry);
    assert.ok("externalize" in result, JSON.stringify(result));
    assert.equal(result.externalize, "zeroship");
    await assert.rejects(zeroshipEvaluator.runExternalModule("unbundled-package"),
      /Configure Vite to bundle this dependency/);
  } finally {
    await server.close();
    await fs.rm(root, { recursive: true, force: true });
  }
});

test("the dev optimizer pre-bundles a dependency's requires of built-ins into modules the runtime can run", async () => {
  // The isolate has neither `require` nor `node:module`'s `createRequire`,
  // which the optimizer would otherwise pre-bundle each require into. The
  // dependency is CommonJS as TypeScript emits it, so it also reads the
  // `default` of a required ES module.
  const root = await fs.mkdtemp(join(tmpdir(), "zs-dev-optimizer-"));
  const dependency = join(root, "node_modules/requires-builtins");
  await fs.mkdir(dependency, { recursive: true });
  await fs.writeFile(join(dependency, "package.json"), JSON.stringify({ name: "requires-builtins", main: "index.js" }));
  await fs.writeFile(join(dependency, "index.js"), `"use strict";
var __importDefault = (this && this.__importDefault) || function (mod) {
  return (mod && mod.__esModule) ? mod : { "default": mod };
};
const path_1 = __importDefault(require("path"));
const process = require("process");
const timers = require("timers/promises");
const { createRequire } = require("module");
const { Worker } = require("worker_threads");
const zeroship = require("zeroship");
const constructed = () => { try { new Worker("", { eval: true }); return "constructed"; } catch (error) { return error.code; } };
exports.probe = async () => ({
  tsDefault: path_1.default.join("g", "h"),
  processEnv: typeof process.env,
  slept: await timers.setTimeout(1, "slept"),
  createRequire: createRequire("/")("path").join("i", "j"),
  worker: constructed(),
  kernel: zeroship.env.kind,
});
`);
  // The host module, for evaluating the pre-bundled dependency in Node.
  const kernel = join(root, "node_modules/zeroship");
  await fs.mkdir(kernel, { recursive: true });
  await fs.writeFile(join(kernel, "package.json"), JSON.stringify({ name: "zeroship", type: "module", exports: "./index.js" }));
  await fs.writeFile(join(kernel, "index.js"), `export const env = { kind: "host" };\n`);
  const entry = join(root, "server.js");
  await fs.writeFile(entry, `import { probe } from "requires-builtins";\nexport default probe;\n`);
  // Vite runs from the app's root, whose node_modules does not carry the
  // plugin's own dependencies.
  const cwd = process.cwd();
  process.chdir(root);
  const server = await createServer({
    root, configFile: false, logLevel: "silent",
    server: { middlewareMode: true, watch: null },
    plugins: [nodeCompatPlugin(), zeroshipModulePlugin()],
    environments: { zeroship: createZeroshipEnvironmentOptions(entry) },
  }).catch((error) => {
    process.chdir(cwd);
    throw error;
  });
  try {
    const environment = server.environments.zeroship as DevEnvironment;
    await environment.transformRequest(entry);
    const optimizer = environment.depsOptimizer;
    assert.ok(optimizer, "the dev environment pre-bundles dependencies");
    await optimizer.metadata.discovered["requires-builtins"]?.processing;
    const optimized = optimizer.metadata.optimized["requires-builtins"];
    assert.ok(optimized?.file, "the dependency was pre-bundled");
    const imports = parseAst(await fs.readFile(optimized.file, "utf8")).body
      .flatMap((node) => node.type === "ImportDeclaration" ? [String(node.source.value)] : []);
    // Runtime modules stay imports the runtime answers; the host module is
    // never bundled in.
    assert.ok(imports.includes("node:path"), JSON.stringify(imports));
    assert.ok(imports.includes("zeroship"), JSON.stringify(imports));
    assert.ok(!imports.includes("node:module"), JSON.stringify(imports));
    const pre = await import(pathToFileURL(optimized.file).href);
    assert.deepEqual(await pre.default.probe(), {
      tsDefault: "g/h",
      processEnv: "object",
      slept: "slept",
      createRequire: "i/j",
      worker: "ERR_ZEROSHIP_UNSUPPORTED_API",
      kernel: "host",
    });
  } finally {
    process.chdir(cwd);
    await server.close();
    await fs.rm(root, { recursive: true, force: true });
  }
});

test("the dev environment serves the plugin's replacement built-ins as modules the runtime can run", async () => {
  // Evaluated the way the dev runtime evaluates server code: a ModuleRunner
  // fed by the environment's `fetchModule`, with a native bridge that, like
  // the runtime's, answers only the `node:` names it implements. The
  // pre-bundled dependency's optional require of a package that is not
  // installed goes through `createRequire` from `node:module`.
  const root = await fs.mkdtemp(join(tmpdir(), "zs-dev-replacements-"));
  const dependency = join(root, "node_modules/optional-require");
  await fs.mkdir(dependency, { recursive: true });
  await fs.writeFile(join(dependency, "package.json"), JSON.stringify({ name: "optional-require", main: "index.js" }));
  await fs.writeFile(join(dependency, "index.js"), `let optional;
try {
  optional = require("absent-optional");
} catch (error) {
  optional = error;
}
exports.optional = () => optional instanceof Error ? "fallback" : "present";
`);
  const serializer = join(root, "node_modules/serializes-off-thread");
  await fs.mkdir(serializer, { recursive: true });
  await fs.writeFile(join(serializer, "package.json"), OFF_THREAD_SERIALIZER_PACKAGE);
  await fs.writeFile(join(serializer, "index.js"), OFF_THREAD_SERIALIZER_SOURCE);
  const entry = join(root, "entry.js");
  await fs.writeFile(entry, `import process from "node:process";
import { setTimeout as sleep, setImmediate as immediate, setInterval as every } from "node:timers/promises";
import { createRequire, isBuiltin } from "node:module";
import { Worker } from "node:worker_threads";
import { optional } from "optional-require";
import { probe as offThread } from "serializes-off-thread";
const code = (probe) => { try { probe(); return "accepted"; } catch (error) { return error.code; } };
// How a call fails: by throwing, or through the promise it returns.
const failure = async (call) => {
  let pending;
  try { pending = call(); } catch (error) { return "threw " + error.code; }
  try { await pending; return "fulfilled"; } catch (error) { return "rejected " + error.code; }
};
const signal = new AbortController().signal;
export const probe = async () => ({
  process: process === globalThis.process,
  slept: await sleep(1, "slept"),
  createRequire: createRequire("/")("path").join("i", "j"),
  createRequireMissing: code(() => createRequire("/")("fs")),
  createRequireResolve: [createRequire("/").resolve("path"), createRequire("/").resolve("node:crypto")],
  createRequireResolveMissing: code(() => createRequire("/").resolve("fs")),
  isBuiltin: isBuiltin("node:crypto"),
  timersSignal: await failure(() => sleep(1, "x", { signal })),
  immediateSignal: await failure(() => immediate("x", { signal })),
  intervalSignal: await failure(() => every(1, "x", { signal }).next()),
  worker: code(() => new Worker("", { eval: true })),
  offThread: await offThread(),
  optional: optional(),
});
`);
  const server = await createServer({
    root, configFile: false, logLevel: "silent",
    // An app in the workspace may load files from the workspace root, where
    // the plugin's own unenv lives. This temporary root is outside it.
    server: { middlewareMode: true, watch: null, fs: { allow: [root, searchForWorkspaceRoot(fileURLToPath(import.meta.url))] } },
    plugins: [nodeCompatPlugin(), zeroshipModulePlugin()],
    environments: { zeroship: createZeroshipEnvironmentOptions(entry) },
  });
  const native: Record<string, unknown> = {
    "node:async_hooks": asyncHooks, "node:buffer": buffer, "node:crypto": crypto,
    "node:path": { ...path, default: path }, "node:util": util,
  };
  const bridge = (specifier: string) => {
    if (!Object.hasOwn(native, specifier)) throw new Error(`unknown specifier '${specifier}'`);
    return native[specifier];
  };
  Object.defineProperty(globalThis, "__zeroshipNodeBuiltin", { value: bridge, configurable: true });
  const environment = server.environments.zeroship as DevEnvironment;
  const runner = new ModuleRunner({
    hmr: false,
    transport: {
      async invoke(payload: any) {
        const { name, data } = payload.data;
        if (name === "getBuiltins") return { result: ["zeroship", "/@id/zeroship"] };
        try {
          return { result: await environment.fetchModule(data[0], data[1], data[2]) };
        } catch (error) {
          return { error: { message: String((error as Error).message) } };
        }
      },
    },
  }, zeroshipEvaluator);
  try {
    await environment.transformRequest(entry);
    const optimizer = environment.depsOptimizer;
    assert.ok(optimizer, "the dev environment pre-bundles dependencies");
    await optimizer.metadata.discovered["optional-require"]?.processing;
    await optimizer.metadata.discovered["serializes-off-thread"]?.processing;
    assert.ok(optimizer.metadata.optimized["optional-require"], "the dependency was pre-bundled");
    assert.ok(optimizer.metadata.optimized["serializes-off-thread"], "the dependency was pre-bundled");
    const module = await runner.import(entry);
    assert.deepEqual(await module.probe(), {
      process: true,
      slept: "slept",
      createRequire: "i/j",
      createRequireMissing: "MODULE_NOT_FOUND",
      createRequireResolve: ["path", "node:crypto"],
      createRequireResolveMissing: "MODULE_NOT_FOUND",
      isBuiltin: true,
      timersSignal: "rejected ERR_ZEROSHIP_UNSUPPORTED_OPTION",
      immediateSignal: "rejected ERR_ZEROSHIP_UNSUPPORTED_OPTION",
      intervalSignal: "rejected ERR_ZEROSHIP_UNSUPPORTED_OPTION",
      worker: "ERR_ZEROSHIP_UNSUPPORTED_API",
      offThread: "inline",
      optional: "fallback",
    });
  } finally {
    Reflect.deleteProperty(globalThis, "__zeroshipNodeBuiltin");
    await runner.close();
    await server.close();
    await fs.rm(root, { recursive: true, force: true });
  }
});
