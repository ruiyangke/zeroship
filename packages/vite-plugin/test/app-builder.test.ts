/**
 * `vite build` builds the worker from the app's own Vite config.
 *
 * The app's plugins, its top-level `define` and its `resolve.alias` reach
 * server code, exactly as they do in dev, where server code runs through the
 * `zeroship` environment with the app's whole plugin pipeline. Every build
 * here goes through `createBuilder(config, null)`, the call the `vite build`
 * CLI makes: it builds the worker only because `zeroship()` opts the app into
 * the builder. A config FILE is loaded, so each environment resolves its own
 * plugin instances and the discovery state reaches the manifest only through
 * the plugins the builder shares.
 */

import { test, describe } from "node:test";
import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { promises as fs } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";
import { createBuilder, build as viteBuild, type InlineConfig, type Plugin } from "vite";

import { zeroshipPlugins } from "../src/plugins.js";
import { readZship, workerModules } from "./helpers/zship-archive.js";

const PLUGINS = fileURLToPath(new URL("../src/plugins.ts", import.meta.url));

// A procedure whose result depends on everything the app config contributes:
// an import only the app's plugin can load, the app's `define`, the app's
// alias, a `process.env` value the runtime supplies when it runs, and the dead
// branch of a `process.env.NODE_ENV` check. A second procedure reports which
// build the worker bundled of a package with conditional exports, and of one
// with only entry fields.
const SERVER_TS = `
"use server";
import { query } from "@zeroship/rpc/server";
import greetingText from "./hello.greeting";
import { suffix } from "~lib/suffix";
import { picked } from "pick-me";
import { field } from "pick-by-field";

export const greet = query(async () => {
  if (process.env.NODE_ENV !== "production") return "a development branch shipped";
  return [greetingText, __APP_DEFINED__, suffix, String(process.env.PROBE_KEY)].join(" | ");
}, { id: "probe.greet" });

export const resolved = query(async () => [picked, field], { id: "probe.resolved" });

export default {
  fetch() {
    return new Response("ok");
  },
};
`;

const EXPECTED =
  "hello from an app plugin | from the app define | aliased | from the runtime env";

// A lazy procedure the worker reaches only through the synthetic entry's
// dynamic import; the client's import is what discovers it.
const LATER_TS = `
"use server";
import { query } from "@zeroship/rpc/server";

export const later = query(async () => "loaded later", { id: "probe.later", lazy: true });
`;

function greetingPlugin(clientOnly = false): Plugin {
  return {
    name: "fixture:greeting",
    ...(clientOnly
      ? { applyToEnvironment: (environment) => environment.config.consumer === "client" }
      : {}),
    transform(code, id) {
      if (!id.endsWith(".greeting")) return null;
      return { code: "export default " + JSON.stringify(code.trim()) + ";", map: null, moduleType: "js" };
    },
  };
}

interface AppOptions {
  /** Restrict the `.greeting` loader to the client environment. */
  greetingClientOnly?: boolean;
  /**
   * `none`: no client entry, so the plugin injects its empty client stub.
   * `object`: an explicit object `build.rollupOptions.input` naming an HTML
   * page whose script imports the procedures, one of them lazy.
   */
  clientInput?: "none" | "object";
  mode?: "full" | "static";
  /** Server code imports a module the app marks external. */
  externalImport?: boolean;
  /**
   * Give the CLIENT environment its own `build.outDir` (and `zeroship.jsonc`
   * the matching `build.dist`), leaving the top-level `build.outDir` alone.
   */
  clientOutDir?: string;
}

/** The fixture app's inline Vite config, without the plugins. */
function viteOptions(root: string, options: AppOptions): InlineConfig {
  const build: Record<string, unknown> = {};
  if (options.externalImport) build.rollupOptions = { external: ["left-out"] };
  if (options.clientInput === "object") {
    build.manifest = true;
    build.rollupOptions = { input: { main: resolve(root, "pages/main.html") } };
  }
  return {
    logLevel: "silent",
    define: { __APP_DEFINED__: JSON.stringify("from the app define") },
    resolve: { alias: { "~lib": resolve(root, "src/lib") } },
    build,
    ...(options.clientOutDir
      ? { environments: { client: { build: { outDir: options.clientOutDir } } } }
      : {}),
  };
}

async function makeApp(options: AppOptions = {}): Promise<string> {
  const root = join(tmpdir(), `zs-app-builder-${process.pid}-${Math.random().toString(36).slice(2)}`);
  const write = async (path: string, body: string) => {
    await fs.mkdir(resolve(root, path, ".."), { recursive: true });
    await fs.writeFile(resolve(root, path), body, "utf8");
  };
  await write(
    "src/server.ts",
    options.externalImport ? SERVER_TS.replace('"use server";\n', '"use server";\nimport "left-out";\n') : SERVER_TS,
  );
  await write("src/hello.greeting", "hello from an app plugin\n");
  await write("src/lib/suffix.ts", `export const suffix = "aliased";\n`);
  await write("public/robots.txt", "User-agent: *\n");
  // One build per condition a worker could resolve.
  await write(
    "node_modules/pick-me/package.json",
    JSON.stringify({
      name: "pick-me",
      type: "module",
      exports: { ".": { worker: "./worker.js", browser: "./browser.js", default: "./default.js" } },
    }),
  );
  for (const build of ["worker", "browser", "default"]) {
    await write(`node_modules/pick-me/${build}.js`, `export const picked = ${JSON.stringify(build)};\n`);
  }
  await write(
    "node_modules/pick-by-field/package.json",
    JSON.stringify({ name: "pick-by-field", browser: "./browser.js", module: "./module.js", main: "./main.js" }),
  );
  for (const build of ["browser", "module"]) {
    await write(`node_modules/pick-by-field/${build}.js`, `export const field = ${JSON.stringify(build)};\n`);
  }
  await write("node_modules/pick-by-field/main.js", `exports.field = "main";\n`);
  await write(
    "node_modules/@zeroship/rpc/package.json",
    JSON.stringify({ type: "module", exports: { "./server": "./server.js", "./client": "./client.js" } }),
  );
  await write(
    "node_modules/@zeroship/rpc/server.js",
    `export function query(handler, config = {}) {
      Object.defineProperty(handler, "config", {
        value: { ...config, kind: "query" },
        enumerable: true,
        configurable: true,
      });
      return handler;
    }`,
  );
  await write(
    "node_modules/@zeroship/rpc/client.js",
    `export function createRpcClient() {
      return new Proxy({}, { get: () => (name) => async () => name });
    }`,
  );
  await write(
    "node_modules/@zeroship/server/package.json",
    JSON.stringify({ type: "module", exports: "./index.js" }),
  );
  await write(
    "node_modules/@zeroship/server/index.js",
    `export function __makeServerProcedure(handler, metadata) {
      return Object.assign((input) => handler(input), metadata);
    }`,
  );
  if (options.clientInput === "object") {
    await write(
      "pages/main.html",
      `<!doctype html><script type="module" src="/src/client.ts"></script>\n`,
    );
    await write("src/later.ts", LATER_TS);
    await write(
      "src/client.ts",
      `import { greet } from "./server";\nimport { later } from "./later";\ndocument.title = String(await greet()) + String(await later());\n`,
    );
  }
  if (options.mode === "static" || options.clientOutDir) {
    const dist = options.clientOutDir ?? "dist";
    if (options.mode === "static") await write("public/index.html", "<!doctype html><title>static</title>\n");
    await write(
      "zeroship.jsonc",
      JSON.stringify({
        name: "fixture-app",
        control: "http://localhost:9090",
        runtime_date: "2042-03-04",
        build: { mode: options.mode ?? "full", dist, output: `${dist}/app.zship` },
        databases: {},
        apps: { app: { databases: [] } },
      }),
    );
  }
  await write(
    "vite.config.mjs",
    `import { zeroshipPlugins } from ${JSON.stringify(PLUGINS)};

function greeting() {
  return {
    name: "fixture:greeting",
    ${options.greetingClientOnly ? `applyToEnvironment: (environment) => environment.config.consumer === "client",` : ""}
    transform(code, id) {
      if (!id.endsWith(".greeting")) return null;
      return { code: "export default " + JSON.stringify(code.trim()) + ";", map: null, moduleType: "js" };
    },
  };
}

export default {
  ...${JSON.stringify(viteOptions(root, options))},
  // An empty process environment: nothing in the test's shell reaches the plugin.
  plugins: [greeting(), ...zeroshipPlugins({}, {})],
};
`,
  );
  return root;
}

function appConfig(root: string, extra: InlineConfig = {}): InlineConfig {
  return {
    root,
    configFile: resolve(root, "vite.config.mjs"),
    configLoader: "native",
    logLevel: "silent",
    ...extra,
  };
}

/** Build the way `vite build` does. */
async function viteBuildCli(root: string, extra: InlineConfig = {}): Promise<void> {
  const builder = await createBuilder(appConfig(root, extra), null);
  await builder.buildApp();
}

/**
 * Call a procedure of the packed worker in a fresh Node process. Its own
 * `NODE_ENV` is `development`, so a `process.env.NODE_ENV` check the build did
 * not replace would take its development branch, and it carries the
 * `PROBE_KEY` a runtime would supply.
 */
function callPacked(root: string, entryPath: string, wireId: string): unknown {
  const code = `const m = await import(${JSON.stringify(pathToFileURL(entryPath).href)});
const binding = m.default.rpc[${JSON.stringify(wireId)}];
const procedure = typeof binding === "function" ? binding : await binding.load();
process.stdout.write(JSON.stringify(await procedure()));`;
  const run = spawnSync(process.execPath, ["--input-type=module", "-e", code], {
    cwd: root,
    env: { NODE_ENV: "development", PROBE_KEY: "from the runtime env" },
    encoding: "utf8",
  });
  assert.equal(run.status, 0, run.stderr);
  return JSON.parse(run.stdout);
}

/** Every file under `dir`, relative to it. */
async function listFiles(dir: string): Promise<string[]> {
  const entries = await fs.readdir(dir, { recursive: true, withFileTypes: true });
  return entries
    .filter((entry) => entry.isFile())
    .map((entry) => join(entry.parentPath, entry.name).slice(dir.length + 1))
    .sort();
}

async function unpackWorker(root: string, archive = "dist/app.zship") {
  const contents = readZship(await fs.readFile(resolve(root, archive)));
  const { entry, modules } = workerModules(contents);
  for (const [path, body] of modules) {
    await fs.mkdir(resolve(root, "unpacked", path, ".."), { recursive: true });
    await fs.writeFile(resolve(root, "unpacked", path), body);
  }
  return { contents, entry, modules, entryPath: resolve(root, "unpacked", entry) };
}

describe("vite build compiles the worker from the app's config", () => {
  for (const clientInput of ["none", "object"] as const) {
    test(`app plugins, define and alias reach server code (client input: ${clientInput})`, async () => {
      const root = await makeApp({ clientInput });
      try {
        await viteBuildCli(root);
        const { contents, entry, modules, entryPath } = await unpackWorker(root);

        // The worker is one module: neither the client stub nor an HTML page
        // the app names as client input reaches the worker's own input, a
        // lazy procedure's module is not split into a chunk of its own, and
        // neither the public directory nor the app's build manifest lands
        // among the worker's modules.
        assert.equal(entry, "index.js");
        assert.deepEqual([...modules.keys()], ["index.js"]);
        assert.deepEqual(await listFiles(resolve(root, "dist", "server")), ["index.js"]);

        const resources = (contents.manifest.resources ?? {}) as Record<string, unknown>;
        assert.ok(resources["rpc:probe.greet"], "the manifest declares the discovered procedure");

        assert.equal(callPacked(root, entryPath, "probe.greet"), EXPECTED);
        if (clientInput === "object") {
          assert.ok(resources["rpc:probe.later"], "the client's import discovers the lazy procedure");
          assert.equal(callPacked(root, entryPath, "probe.later"), "loaded later");
        }
      } finally {
        await fs.rm(root, { recursive: true, force: true });
      }
    });
  }

  test("the worker resolves packages to their browser builds", async () => {
    // The worker resolves packages as an `ssr` environment built for
    // `ssr.target: "webworker"`: the `browser` export condition and no
    // `worker`, and the `browser` entry field ahead of `module`.
    const root = await makeApp();
    try {
      await viteBuildCli(root);
      const { entryPath } = await unpackWorker(root);
      assert.deepEqual(callPacked(root, entryPath, "probe.resolved"), ["browser", "browser"]);
    } finally {
      await fs.rm(root, { recursive: true, force: true });
    }
  });

  test("the vite CLI builds the same worker, whatever NODE_ENV its shell carries", async () => {
    const root = await makeApp();
    try {
      // The CLI in a child process whose shell says `development`: Vite would
      // otherwise replace `process.env.NODE_ENV` with that value.
      const vite = fileURLToPath(new URL("bin/vite.js", import.meta.resolve("vite/package.json")));
      const run = spawnSync(
        process.execPath,
        ["--import", import.meta.resolve("tsx"), vite, "build", "--configLoader", "native"],
        { cwd: root, env: { NODE_ENV: "development" }, encoding: "utf8" },
      );
      assert.equal(run.status, 0, run.stderr + run.stdout);
      const { entryPath } = await unpackWorker(root);
      assert.equal(callPacked(root, entryPath, "probe.greet"), EXPECTED);
    } finally {
      await fs.rm(root, { recursive: true, force: true });
    }
  });

  test("the worker is packed from the client environment's outDir", async () => {
    // Only the client environment names an outDir; the top-level `build`
    // keeps Vite's default. The packer walks the client's outDir.
    const root = await makeApp({ clientOutDir: "client-out" });
    try {
      await viteBuildCli(root);
      assert.deepEqual(await listFiles(resolve(root, "client-out", "server")), ["index.js"]);
      const { entryPath } = await unpackWorker(root, "client-out/app.zship");
      assert.equal(callPacked(root, entryPath, "probe.greet"), EXPECTED);
    } finally {
      await fs.rm(root, { recursive: true, force: true });
    }
  });

  test("a worker that imports a module it did not bundle is refused", async () => {
    const root = await makeApp({ externalImport: true });
    try {
      await assert.rejects(viteBuildCli(root), /unbundled import: left-out/);
    } finally {
      await fs.rm(root, { recursive: true, force: true });
    }
  });

  test("a loader the app scopes to the client leaves the worker unable to import", async () => {
    const root = await makeApp({ greetingClientOnly: true });
    try {
      await assert.rejects(viteBuildCli(root), /hello\.greeting/);
    } finally {
      await fs.rm(root, { recursive: true, force: true });
    }
  });

  test("static mode packs the client output and no worker", async () => {
    const root = await makeApp({ mode: "static" });
    try {
      await viteBuildCli(root);
      const contents = readZship(await fs.readFile(resolve(root, "dist", "app.zship")));
      assert.equal(contents.manifest.worker ?? null, null, "a static app packs no worker");
      const assets = (contents.manifest.assets ?? {}) as Record<string, unknown>;
      assert.ok(assets["/index.html"], "the public page is packed");
      assert.deepEqual(
        Object.keys(assets).filter((path) => path.endsWith(".js")),
        [],
        "the empty client stub is not packed",
      );
    } finally {
      await fs.rm(root, { recursive: true, force: true });
    }
  });

  test("a programmatic vite.build() is refused before the client writes output", async () => {
    const root = await makeApp({ clientInput: "object" });
    try {
      await assert.rejects(viteBuild(appConfig(root)), /createBuilder\(config\)\.buildApp\(\)/);
      await assert.rejects(fs.access(resolve(root, "dist")), "nothing was written to dist");
    } finally {
      await fs.rm(root, { recursive: true, force: true });
    }
  });

  test("plugin instances reused after a build still refuse a programmatic vite.build()", async () => {
    // An inline config reuses the same plugin objects for every build it
    // drives, so what one build records must not carry into the next.
    const root = await makeApp();
    try {
      const inline: InlineConfig = {
        ...viteOptions(root, {}),
        root,
        configFile: false,
        plugins: [greetingPlugin(), ...zeroshipPlugins({}, {})],
      };
      await (await createBuilder(inline, null)).buildApp();
      await fs.rm(resolve(root, "dist"), { recursive: true, force: true });
      await assert.rejects(viteBuild(inline), /createBuilder\(config\)\.buildApp\(\)/);
    } finally {
      await fs.rm(root, { recursive: true, force: true });
    }
  });

  test("a builder that builds only the worker environment is not refused", async () => {
    // A tool that drives the builder itself, without `buildApp`, and builds
    // the worker alone.
    const root = await makeApp();
    try {
      const builder = await createBuilder(appConfig(root), null);
      await builder.build(builder.environments.zeroship!);
      assert.deepEqual(await listFiles(resolve(root, "dist")), ["server/index.js"]);
    } finally {
      await fs.rm(root, { recursive: true, force: true });
    }
  });

  test("vite build --watch is refused", async () => {
    const root = await makeApp();
    try {
      await assert.rejects(viteBuildCli(root, { build: { watch: {} } }), /--watch/);
    } finally {
      await fs.rm(root, { recursive: true, force: true });
    }
  });
});
