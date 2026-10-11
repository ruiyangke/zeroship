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
try { await fs.symlink(dependencies, linked, link); }
catch (error) { if ((error as NodeJS.ErrnoException).code !== "EEXIST") throw error; }
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
/**
 * What the build's resolver answered for `@zeroship/workflows`, recorded by a
 * plugin that asks it the same question with the same importer. An answer of
 * `null` is an import the resolver could not find; the bundler then leaves it
 * unbundled without saying why.
 */
// Vite's plugin type, as the dev bundle's inline config takes it; this file
// sits outside the package that depends on Vite.
type InlinePlugins = NonNullable<Parameters<typeof buildDevBundle>[0]["config"]["inlineConfig"]["plugins"]>;
type Plugin = Extract<InlinePlugins[number], { name: string }>;
const resolutions: string[] = [];
const resolutionProbe: Plugin = {
  name: "fixture:resolution-probe",
  resolveId: {
    async handler(source, importer, options) {
      if (source !== "@zeroship/workflows") return null;
      const resolved = await this.resolve(source, importer, { ...options, skipSelf: true });
      resolutions.push(`from ${importer}: ${JSON.stringify(resolved)}`);
      return null;
    },
  },
};

/**
 * How the build's resolver and the file system see the linked dependencies
 * the build resolves `@zeroship/workflows` through. Printed when the build
 * fails, so a failure names the link it went through.
 */
async function describeLinkedWorkflows(): Promise<string> {
  const lines = [`[fixture] ${linked} is a ${link} to ${dependencies}`];
  for (const resolution of resolutions) lines.push(`[fixture] the resolver answered ${resolution}`);
  const probe = async (label: string, read: () => Promise<string>) => {
    try {
      lines.push(`[fixture] ${label}: ${await read()}`);
    } catch (error) {
      lines.push(`[fixture] ${label} failed: ${(error as Error).message}`);
    }
  };
  const workflows = join(linked, "@zeroship", "workflows");
  await probe("lstat of the link", async () => {
    const stat = await fs.lstat(linked);
    return `symbolic link ${stat.isSymbolicLink()}, directory ${stat.isDirectory()}`;
  });
  await probe("realpath of the link", () => fs.realpath(linked));
  await probe("realpath of @zeroship/workflows", () => fs.realpath(workflows));
  await probe("exports of @zeroship/workflows", async () =>
    JSON.stringify(JSON.parse(await fs.readFile(join(workflows, "package.json"), "utf8")).exports));
  return lines.join("\n");
}

let bundle: Awaited<ReturnType<typeof buildDevBundle>>;
try {
  bundle = await buildDevBundle({
    config: {
      root,
      configFile: undefined,
      // Warnings, not silence: an import the bundler cannot resolve is a
      // warning that names why, and it is the only account of a failed build.
      inlineConfig: { root, logLevel: "warn", plugins: [resolutionProbe] },
      mode: "development",
      plugins: () => zeroshipPlugins({}, {}),
    },
    project: defaultProjectConfig(),
    databases: [],
  });
} catch (error) {
  console.error(await describeLinkedWorkflows());
  throw error;
}
const pending = join(root, "pending.zship");
await fs.writeFile(pending, bundle.archive);
await fs.rename(pending, join(root, "app.zship"));
