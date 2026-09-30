// Build the fixture app in `runtime-server-entry/` the way `vite build` does
// and print the worker modules it packed, entry first, for
// `crates/zeroship-runtime/tests/vite_server_entry.rs` to load into a runtime.
//
// The app loads `hello.greeting` through a plugin of its own, so the worker
// only builds when the app's plugins apply to server code. The app is copied
// to a temporary root, because the build writes its `dist` there.

import assert from "node:assert/strict";
import { cp, mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { createBuilder } from "vite";
import { readZship, workerModules } from "../helpers/zship-archive.js";

const source = fileURLToPath(new URL("./runtime-server-entry/", import.meta.url));
const plugins = fileURLToPath(new URL("../../src/plugins.ts", import.meta.url));
const sdk = (path: string) => fileURLToPath(new URL(`../../../${path}`, import.meta.url));

const root = await mkdtemp(join(tmpdir(), "zs-runtime-server-entry-"));
try {
  await cp(source, root, { recursive: true });
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
