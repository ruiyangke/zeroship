/**
 * ISS-66 regression — the production SSR build MUST resolve the bare
 * `zeroship` specifier to the runtime virtual module (which reads
 * `__zs_env()`), NOT to the file-linked `zeroship-stub` package
 * (`export const env = {}`).
 *
 * Root cause: the production SSR build's plugin list (build.ts
 * `writeBundle`) omitted `zeroshipModulePlugin()`. With `noExternal: true`,
 * the bare `import { env } from "zeroship"` then fell through to the
 * `zeroship-stub` Node package and got inlined. At runtime `env.db` was
 * `undefined`, so every env.db (and env.auth/kv/storage) RPC procedure
 * threw `Cannot read properties of undefined` — the dispatch reached the
 * handler but the handler couldn't touch any platform primitive.
 *
 * This test runs a REAL SSR `viteBuild` of a fixture that imports
 * `{ env } from "zeroship"`, with the same `zeroshipModulePlugin()` the
 * production path now installs, and asserts the emitted bundle wires the
 * import to `__zs_env` and carries NO stub sentinel. Pre-fix (plugin
 * absent) the stub sentinel is present and `__zs_env` is not.
 */

import { test, describe } from "node:test";
import assert from "node:assert/strict";
import { promises as fs } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { randomUUID } from "node:crypto";
import { build as viteBuild } from "vite";

import { zeroshipModulePlugin } from "../src/zeroship-module.js";
import { nodeCompatPlugin } from "../src/node-compat.js";

const STUB_SENTINEL = "outside the zeroship V8 runtime";

async function buildSsrEntry(
  entryCode: string,
  plugins: unknown[],
): Promise<string> {
  const root = join(tmpdir(), `zs-env-res-${randomUUID()}`);
  await fs.mkdir(root, { recursive: true });
  const entry = resolve(root, "entry.js");
  await fs.writeFile(entry, entryCode, "utf8");
  try {
    const out = (await viteBuild({
      root,
      configFile: false,
      logLevel: "silent",
      // eslint-disable-next-line @typescript-eslint/no-explicit-any
      plugins: plugins as any,
      ssr: { noExternal: true, target: "webworker" },
      build: {
        ssr: entry,
        write: false,
        minify: false,
        outDir: resolve(root, "out"),
        rollupOptions: { output: { format: "esm" } },
      },
      // eslint-disable-next-line @typescript-eslint/no-explicit-any
    })) as any;
    const output = Array.isArray(out) ? out[0].output : out.output;
    const chunk = output.find(
      (o: { type: string; isEntry?: boolean }) => o.type === "chunk" && o.isEntry,
    );
    return chunk.code as string;
  } finally {
    await fs.rm(root, { recursive: true, force: true });
  }
}

const ENTRY = `
import { env } from "zeroship";
export default { fetch() { return new Response(typeof env.db); } };
`;

describe("ISS-66 — zeroship specifier resolves to the runtime env, not the stub", () => {
  test("with zeroshipModulePlugin: bundle wires __zs_env, no stub sentinel", async () => {
    const code = await buildSsrEntry(ENTRY, [
      nodeCompatPlugin(),
      zeroshipModulePlugin(),
    ]);
    assert.ok(
      code.includes("__zs_env"),
      "bundle must resolve `zeroship` to the runtime virtual module (__zs_env)",
    );
    assert.ok(
      !code.includes(STUB_SENTINEL),
      "bundle must NOT inline the zeroship-stub package",
    );
  });

  test("resolver maps the bare `zeroship` specifier (so it is never left for the stub)", () => {
    const plugin = zeroshipModulePlugin();
    // resolveId is the load-bearing hook: it claims `zeroship` before the
    // default resolver (which would otherwise resolve the `zeroship-stub`
    // package). Mirrors the production SSR plugin list.
    // eslint-disable-next-line @typescript-eslint/no-explicit-any
    const resolved = (plugin.resolveId as any).call({}, "zeroship");
    assert.equal(typeof resolved, "string");
    assert.notEqual(resolved, "zeroship");
    // And `load` returns the runtime virtual module body (reads __zs_env).
    // eslint-disable-next-line @typescript-eslint/no-explicit-any
    const loaded = (plugin.load as any).call({}, resolved) as string;
    assert.ok(loaded.includes("__zs_env"));
    assert.ok(!loaded.includes(STUB_SENTINEL));
  });
});
