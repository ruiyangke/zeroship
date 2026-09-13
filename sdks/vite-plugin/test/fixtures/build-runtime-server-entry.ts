import assert from "node:assert/strict";
import { fileURLToPath } from "node:url";
import { resolve } from "node:path";
import { build, type InlineConfig } from "vite";
import { buildSsrInlineConfig } from "../../src/build.js";
import { zeroshipModulePlugin } from "../../src/zeroship-module.js";
import { rpcRegistryPlugin, type ServerBinding, SERVER_ENTRY_VIRTUAL_ID } from "../../src/rpc-registry.js";

const root = fileURLToPath(new URL("./runtime-server-entry/", import.meta.url));
const procedures: ServerBinding[] = [
  { wireId: "eager", sourceFile: "./eager.ts", exportName: "eager", kind: "query" },
  { wireId: "__proto__", sourceFile: "./lazy.ts", exportName: "lazy", kind: "query", lazy: true },
  { wireId: "tokens", sourceFile: "./lazy.ts", exportName: "tokens", kind: "stream", lazy: true },
];
const bindings = new Map(procedures.map(binding => [
  binding.wireId, { ...binding, sourceFile: resolve(root, binding.sourceFile) },
]));
const config = buildSsrInlineConfig({
  root,
  ssrEntry: SERVER_ENTRY_VIRTUAL_ID,
  outDir: resolve(root, "dist"),
  ssrPlugins: [zeroshipModulePlugin(), rpcRegistryPlugin({
    userEntryRel: resolve(root, "user.ts"),
    getBindings: () => bindings,
  })],
}) as InlineConfig;
const artifact = await build({
  ...config,
  logLevel: "silent",
  resolve: {
    alias: {
      "@zeroship/rpc/server": fileURLToPath(new URL("../../../rpc/src/server.ts", import.meta.url)),
    },
  },
  build: {
    ...config.build,
    write: false,
  },
});
assert.ok(!Array.isArray(artifact) && "output" in artifact, "Vite must return its server artifact");
const chunks = artifact.output.filter(output => output.type === "chunk");
const entry = chunks.find(chunk => chunk.isEntry);
assert.ok(entry, "Vite must emit an entry chunk");
assert.ok(chunks.some(chunk => chunk.dynamicImports.length > 0), "fixture must exercise an emitted lazy chunk: " + JSON.stringify(chunks.map(chunk => ({ file: chunk.fileName, imports: chunk.dynamicImports, modules: chunk.moduleIds }))));
assert.ok(chunks.some(chunk => chunk.imports.includes("zeroship")), "artifact must import the native module");
process.stdout.write(JSON.stringify(
  [entry, ...chunks.filter(chunk => chunk !== entry)]
    .map(chunk => ({ specifier: chunk.fileName, source: chunk.code })),
));
