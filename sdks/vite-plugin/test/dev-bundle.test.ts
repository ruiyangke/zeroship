import { test } from "node:test";
import assert from "node:assert/strict";
import { promises as fs } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { tmpdir } from "node:os";
import { fileURLToPath, pathToFileURL } from "node:url";
import { createHash } from "node:crypto";
import { zstdDecompressSync } from "node:zlib";
import { extract } from "tar";
import { buildDevBundle } from "../src/dev-bundle.js";
import { defaultProjectConfig } from "../src/project-config/index.js";

const sdkRoot = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const descriptor = JSON.stringify({ version: 2, collections: {} });

const entry = `
"use server";
import { Workflow } from "@zeroship/workflows";
import { schedule, every } from "@zeroship/workflows/schedule";
import { prefix } from "./dependency.js";
export class Example extends Workflow {
  async run() {
    const { suffix } = await import("./lazy.js");
    return prefix + suffix;
  }
}
export const periodic = schedule({
  name: "periodic",
  workflow: Example,
  schedule: every.hour(),
});
`;

async function fixture(): Promise<string> {
  const root = await fs.mkdtemp(join(tmpdir(), "zs-app-bundle-"));
  await fs.mkdir(join(root, "src"));
  await fs.symlink(join(sdkRoot, "node_modules"), join(root, "node_modules"), "dir");
  await fs.writeFile(join(root, "src/server.ts"), entry);
  await fs.writeFile(join(root, "src/dependency.js"), 'import { basename } from "node:path"; export const prefix = basename("/values/original:");');
  await fs.writeFile(join(root, "src/lazy.js"), 'export const suffix = "lazy";');
  return root;
}

interface Manifest {
  worker: { entry: string; modules: Record<string, string> };
  workflows?: string[];
  schedules?: { name: string; workflowName: string }[];
  runtime_descriptor?: { hash: string };
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

test("local workflow bundles retain dependencies and rebuild declarations independently", async () => {
  const root = await fixture();
  const retained = await fs.mkdtemp(join(tmpdir(), "zs-retained-workflows-"));
  try {
    const opts = {
      root,
      entry: join(root, "src/server.ts"),
      project: defaultProjectConfig(),
      runtimeDescriptor: descriptor,
    };
    const original = await buildDevBundle(opts);
    assert.ok(original.dependencies.includes(opts.entry));
    assert.ok(original.dependencies.includes(join(root, "src/dependency.js")));
    assert.ok(original.dependencies.includes(join(root, "src/lazy.js")));
    const oldPath = join(retained, "original");
    const manifest = await unpack(original.archive, oldPath);
    assert.deepEqual(manifest.workflows, ["Example"]);
    assert.deepEqual(manifest.schedules?.map(s => [s.name, s.workflowName]), [["periodic", "Example"]]);
    assert.ok(manifest.runtime_descriptor);
    assert.equal(await fs.readFile(join(oldPath, "blobs", manifest.runtime_descriptor.hash), "utf8"), descriptor);

    await fs.writeFile(join(root, "src/dependency.js"), 'import { basename } from "node:path"; export const prefix = basename("/values/replacement:");');
    const replacement = await buildDevBundle(opts);
    const newPath = join(retained, "replacement");
    const updated = await unpack(replacement.archive, newPath);

    await fs.writeFile(opts.entry, "export default { fetch() { return new Response('no workflows'); } };");
    const removed = await buildDevBundle({ ...opts, runtimeDescriptor: undefined });
    const absent = await unpack(removed.archive, join(retained, "removed"));
    assert.equal(absent.workflows, undefined);
    assert.equal(absent.schedules, undefined);
    assert.equal(absent.runtime_descriptor, undefined);
    assert.ok(!removed.dependencies.includes(join(root, "src/lazy.js")));
    assert.deepEqual(await fs.readdir(join(root, ".zeroship")), []);

    // Loading from retained archives must not consult the editable project.
    await fs.rm(root, { recursive: true, force: true });
    // The native host supplies this primitive; the archive carries no host env.
    Object.defineProperty(globalThis, "__zs_env", { value: () => ({}), configurable: true });
    try {
      const oldModule = await import(pathToFileURL(join(oldPath, manifest.worker.entry)).href);
      const newModule = await import(pathToFileURL(join(newPath, updated.worker.entry)).href);
      assert.equal(await new oldModule.default.workflows.Example().run(), "original:lazy");
      assert.equal(await new newModule.default.workflows.Example().run(), "replacement:lazy");
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
    const opts = {
      root,
      entry: join(root, "src/server.ts"),
      project: defaultProjectConfig(),
      runtimeDescriptor: descriptor,
    };
    await fs.writeFile(opts.entry, 'import "./missing.js";');
    await assert.rejects(buildDevBundle(opts), /missing/);
    assert.deepEqual(await fs.readdir(join(root, ".zeroship")), []);
    await fs.writeFile(opts.entry, entry);
    await assert.rejects(buildDevBundle({ ...opts, runtimeDescriptor: "{}" }), /runtime_descriptor/);
    assert.deepEqual(await fs.readdir(join(root, ".zeroship")), []);
  } finally {
    await fs.rm(root, { recursive: true, force: true });
  }
});
