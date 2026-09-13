/** Built and dev imports must reach the host-owned zeroship module. */
import { test } from "node:test";
import assert from "node:assert/strict";
import { promises as fs } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { pathToFileURL } from "node:url";
import { build, createServer, type Plugin } from "vite";

import { zeroshipModulePlugin } from "../src/zeroship-module.js";
import { createZeroshipEnvironmentOptions } from "../src/environment.js";
import { zeroshipEvaluator } from "../src/dev-bootstrap/evaluator.js";

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
