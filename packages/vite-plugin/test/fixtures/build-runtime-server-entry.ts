// Build the fixture app in `runtime-server-entry/` the way `vite build` does
// and print the worker modules it packed, entry first, for
// `crates/zeroship-runtime/tests/vite_server_entry.rs` to load into a runtime.
//
// The app loads `hello.greeting` through a plugin of its own, so the worker
// only builds when the app's plugins apply to server code. The app is copied
// to a temporary root, because the build writes its `dist` there.

import assert from "node:assert/strict";
import { cp, mkdir, mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { createBuilder } from "vite";
import { readZship, workerModules } from "../helpers/zship-archive.js";
import { OFF_THREAD_SERIALIZER_PACKAGE, OFF_THREAD_SERIALIZER_SOURCE } from "../helpers/off-thread-serializer.js";

const source = fileURLToPath(new URL("./runtime-server-entry/", import.meta.url));
const plugins = fileURLToPath(new URL("../../src/plugins.ts", import.meta.url));
const sdk = (path: string) => fileURLToPath(new URL(`../../../${path}`, import.meta.url));

// A CommonJS dependency as TypeScript emits one under `esModuleInterop`:
// `import path from "path"` becomes `__importDefault(require("path")).default`,
// which reads the `default` of the CommonJS view of an ES module.
const REQUIRES_BUILTINS = `"use strict";
var __importDefault = (this && this.__importDefault) || function (mod) {
  return (mod && mod.__esModule) ? mod : { "default": mod };
};
Object.defineProperty(exports, "__esModule", { value: true });
const path = require("path");
const path_1 = __importDefault(require("path"));
const zeroship = require("zeroship");
const process = require("process");
const timers = require("timers/promises");
const { createRequire } = require("module");
const { Worker } = require("worker_threads");
// An optional dependency that is not installed: the require throws and the
// fallback is taken.
let optional;
try {
  optional = require("absent-optional");
} catch (error) {
  optional = error;
}
exports.optional = () => optional instanceof Error ? "fallback" : "present";
exports.joined = () => path.join("c", "d");
exports.tsDefaultJoined = () => path_1.default.join("g", "h");
exports.kernel = () => typeof zeroship.env.probe.kind;
exports.processEnv = () => typeof process.env;
exports.slept = () => timers.setTimeout(1, "slept");
exports.requiredByCreateRequire = () => createRequire("/")("path").join("i", "j");
exports.constructedWorker = () => {
  try {
    new Worker("", { eval: true });
    return "constructed";
  } catch (error) {
    return error.code;
  }
};
`;

const root = await mkdtemp(join(tmpdir(), "zs-runtime-server-entry-"));
try {
  await cp(source, root, { recursive: true });
  // A CommonJS dependency, written here because node_modules is not tracked.
  await mkdir(join(root, "node_modules", "requires-path"), { recursive: true });
  await writeFile(join(root, "node_modules", "requires-path", "package.json"),
    JSON.stringify({ name: "requires-path", main: "index.js" }));
  await writeFile(join(root, "node_modules", "requires-path", "index.js"), REQUIRES_BUILTINS);
  await mkdir(join(root, "node_modules", "serializes-off-thread"), { recursive: true });
  await writeFile(join(root, "node_modules", "serializes-off-thread", "package.json"), OFF_THREAD_SERIALIZER_PACKAGE);
  await writeFile(join(root, "node_modules", "serializes-off-thread", "index.js"), OFF_THREAD_SERIALIZER_SOURCE);
  await writeFile(join(root, "vite.config.mjs"), `import { zeroshipPlugins } from ${JSON.stringify(plugins)};

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
  resolve: {
    alias: {
      "@zeroship/rpc/server": ${JSON.stringify(sdk("rpc/src/server.ts"))},
      "@zeroship/rpc/client": ${JSON.stringify(sdk("rpc/src/index.ts"))},
      "@zeroship/server": ${JSON.stringify(sdk("server/src/index.ts"))},
    },
  },
  plugins: [
    greeting(),
    ...zeroshipPlugins({ config: (c) => ({ build: { ...c.build, serverEntry: "user.ts" } }) }, {}),
  ],
};
`);
  // stdout carries the module graph; the build's own log goes to stderr.
  console.log = console.error;
  const builder = await createBuilder({
    root,
    configFile: join(root, "vite.config.mjs"),
    configLoader: "native",
    logLevel: "silent",
  }, null);
  await builder.buildApp();

  const { entry, modules } = workerModules(readZship(await readFile(join(root, "dist", "app.zship"))));
  assert.ok(modules.has(entry), "the packed worker must contain its entry");
  process.stdout.write(JSON.stringify(
    [entry, ...[...modules.keys()].filter(path => path !== entry)]
      .map(path => ({ specifier: path, source: modules.get(path)!.toString("utf8") })),
  ));
} finally {
  await rm(root, { recursive: true, force: true });
}
