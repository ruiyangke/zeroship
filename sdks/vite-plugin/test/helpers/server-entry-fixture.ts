import assert from "node:assert/strict";
import { mkdtemp, mkdir, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import type { TestContext } from "node:test";
import { fileURLToPath, pathToFileURL } from "node:url";
import { build } from "esbuild";
import { buildServerEntrySource, type ServerBinding } from "../../src/rpc-registry.js";

export function bindingMap(
  rows: Array<Pick<ServerBinding, "sourceFile" | "exportName"> & Partial<ServerBinding>>,
): Map<string, ServerBinding> {
  return new Map(rows.map((row) => [
    row.sourceFile + "::" + row.exportName,
    { kind: "mutation", wireId: row.exportName, ...row },
  ]));
}

export async function buildEntryFixture(
  t: TestContext,
  options: {
    files: Record<string, string>;
    bindings?: Map<string, ServerBinding>;
    source?: string;
  },
) {
  assert.ok(Object.keys(options.files).length > 0, "fixture must contain application modules");
  const dir = await mkdtemp(join(tmpdir(), "zeroship-entry-"));
  t.after(() => rm(dir, { recursive: true, force: true }));
  for (const [name, source] of Object.entries(options.files)) {
    const path = join(dir, name);
    await mkdir(dirname(path), { recursive: true });
    await writeFile(path, source);
  }
  await writeFile(join(dir, "entry.mjs"), options.source ?? buildServerEntrySource({
    userEntryRel: "./user.mjs",
    bindings: options.bindings,
  }));
  const result = await build({
    absWorkingDir: dir,
    entryPoints: ["entry.mjs"],
    outdir: join(dir, "dist"),
    outExtension: { ".js": ".mjs" },
    bundle: true,
    splitting: true,
    format: "esm",
    platform: "neutral",
    target: "es2024",
    metafile: true,
    // Use the workflow owner's actual collector while its cutover is pending.
    alias: {
      "@zeroship/bootstrap/normalize": fileURLToPath(
        new URL("../../../bootstrap/src/normalize.ts", import.meta.url),
      ),
    },
    external: ["zeroship"],
  });
  assert.ok(result.metafile.outputs["dist/entry.mjs"], "build must produce the server entry");
  return {
    dir,
    artifact: result.metafile,
    load: () => import(pathToFileURL(join(dir, "dist/entry.mjs")).href),
  };
}
