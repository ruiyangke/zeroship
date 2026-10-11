import { promises as fs } from "node:fs";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { buildDevBundle } from "../../../../packages/vite-plugin/src/dev-bundle.js";
import { zeroshipPlugins } from "../../../../packages/vite-plugin/src/plugins.js";
import { defaultProjectConfig } from "../../../../packages/vite-plugin/src/project-config/index.js";

const [root, version, cooldown = "10ms", timeout = "1h"] = process.argv.slice(2);
if (!root || !version) throw new Error("expected a fixture project and version");
const dependencies = fileURLToPath(new URL("../../../../packages/vite-plugin/node_modules", import.meta.url));
await fs.mkdir(join(root, "src"), { recursive: true });
// Windows links a directory without elevation only as a junction.
const link = process.platform === "win32" ? "junction" : "dir";
const linked = join(root, "node_modules");

/**
 * Give the project the vite-plugin's dependencies: a real `node_modules`
 * holding one link per installed package, inside a real directory for each
 * `@scope`, each to the package's canonical directory. pnpm installs each
 * package as a link of its own, so linking the whole `node_modules` would
 * reach every package through a link nested inside another link; one link
 * per package to where it lives leaves nothing for the platform to resolve
 * through a second link.
 */
async function linkDependencies(): Promise<void> {
  await fs.mkdir(linked, { recursive: true });
  const linkPackage = async (installed: string, at: string) => {
    const canonical = await fs.realpath(installed);
    try { await fs.symlink(canonical, at, link); }
    catch (error) { if ((error as NodeJS.ErrnoException).code !== "EEXIST") throw error; }
  };
  for (const entry of await fs.readdir(dependencies, { withFileTypes: true })) {
    // `.bin`, caches and the package manager's own records are not packages.
    if (entry.name.startsWith(".")) continue;
    const installed = join(dependencies, entry.name);
    if (entry.name.startsWith("@") && entry.isDirectory()) {
      const scope = join(linked, entry.name);
      await fs.mkdir(scope, { recursive: true });
      for (const name of await fs.readdir(installed)) {
        await linkPackage(join(installed, name), join(scope, name));
      }
    } else {
      await linkPackage(installed, join(linked, entry.name));
    }
  }
}
await linkDependencies();
await fs.writeFile(join(root, "src/version.ts"), `export default ${JSON.stringify(version)};`);
await fs.writeFile(join(root, "src/tail.ts"), 'export const suffix = ":lazy";');
const entry = join(root, "src/server.ts");
await fs.writeFile(entry, `
"use server";
import { Workflow } from "@zeroship/workflows";
import version from "./version.js";
export class Example extends Workflow {
  async run(_trigger, step) {
    const saved = await step.run("saved", () => version);
    await step.sleep("cooldown", ${JSON.stringify(cooldown)});
    await step.waitForSignal("resume", { type: "resume", timeout: ${JSON.stringify(timeout)} });
    const { suffix } = await import("./tail.js");
    return saved + ":" + version + suffix;
  }
}
export default {
  async fetch(request, env) {
    const url = new URL(request.url);
    if (url.pathname === "/ping") return Response.json({ ready: true });
    if (url.pathname === "/version") {
      const { suffix } = await import("./tail.js");
      return Response.json({ version: version + suffix });
    }
    if (url.pathname === "/start") {
      const run = await env.workflows.Example.start();
      return Response.json({ id: run.id });
    }
    const run = env.workflows.Example.get(url.searchParams.get("id"));
    if (url.pathname === "/signal") return Response.json(await run.signal({ type: "resume" }));
    if (url.pathname === "/result") {
      const bytes = await run.readOutput();
      return Response.json({ size: bytes.length, value: JSON.parse(new TextDecoder().decode(bytes)) });
    }
    return Response.json(await run.status());
  }
};
`);
const bundle = await buildDevBundle({
  config: {
    root,
    configFile: undefined,
    // The level the dev server builds at, so a failed build's warnings print
    // beside its error rather than being silenced.
    inlineConfig: { root, logLevel: "warn" },
    mode: "development",
    plugins: () => zeroshipPlugins({}, {}),
  },
  project: defaultProjectConfig(),
  databases: [],
});
const pending = join(root, "pending.zship");
await fs.writeFile(pending, bundle.archive);
await fs.rename(pending, join(root, "app.zship"));
