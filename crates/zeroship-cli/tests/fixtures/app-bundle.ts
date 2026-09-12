import { promises as fs } from "node:fs";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { buildDevBundle } from "../../../../sdks/vite-plugin/src/dev-bundle.js";
import { defaultProjectConfig } from "../../../../sdks/vite-plugin/src/project-config/index.js";

const [root, version] = process.argv.slice(2);
if (!root || !version) throw new Error("expected a fixture project and version");
const dependencies = fileURLToPath(new URL("../../../../sdks/vite-plugin/node_modules", import.meta.url));
await fs.mkdir(join(root, "src"), { recursive: true });
try { await fs.symlink(dependencies, join(root, "node_modules"), "dir"); }
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
    await step.sleep("cooldown", "10ms");
    await step.waitForSignal("resume", { type: "resume", timeout: "1h" });
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
    return Response.json(await run.status());
  }
};
`);
const bundle = await buildDevBundle({
  root, entry, project: defaultProjectConfig(), runtimeDescriptor: undefined,
});
const pending = join(root, "pending.zship");
await fs.writeFile(pending, bundle.archive);
await fs.rename(pending, join(root, "app.zship"));
