// A throwaway copy of the workspace, migrated once and served as two apps
// against the ONE database the copy's zeroship.jsonc declares.
//
// The copy keeps the workspace shape - `zeroship.jsonc`, `zeroship.workspace.ts`
// and `migrations/` at its root, the apps one level down - because the apps
// reach the config through `zeroship.workspace.ts`, which resolves relative to
// its own file. Flattening it would give each app a private database.

import { cp, mkdir, mkdtemp, readdir, symlink } from "node:fs/promises";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { spawn } from "node:child_process";
import { createServer } from "node:http";
import { rmSync } from "node:fs";
const source = fileURLToPath(new URL("../../", import.meta.url));
const fixtures = join(source, ".zeroship", "browser-fixtures");
await mkdir(fixtures, { recursive: true });
const root = await mkdtemp(join(fixtures, "run-"));
process.on("exit", () => rmSync(root, { recursive: true, force: true }));
const children = new Set();
let closing = false;
function close(code) {
  if (closing) return;
  closing = true;
  for (const child of children) { try { process.kill(-child.pid, "SIGTERM"); } catch (error) { if (error.code !== "ESRCH") throw error; } }
  // The gate holds the event loop open, so leaving it listening turns a
  // SIGTERM into a kill on the shutdown deadline - and a killed process never
  // reaches the `exit` handler that removes the copy.
  if (ready.listening) ready.close();
  process.exitCode = code;
}
process.on("SIGTERM", () => close(0));
process.on("SIGINT", () => close(0));
function start(args, cwd, env) {
  const child = spawn("pnpm", args, { cwd, env, stdio: "inherit", detached: true });
  children.add(child);
  child.on("exit", () => children.delete(child));
  return child;
}
// Answering `/` proves Vite is up; answering a procedure proves the runtime
// behind it is, which is the half that races. Both apps declare `gather.session`.
async function serving(origin) {
  try {
    const response = await fetch(new URL("/__zeroship/v1/gather.session", origin), {
      method: "POST", body: JSON.stringify({ json: {} }),
      headers: { "content-type": "application/json", "X-Method": "GET" },
      signal: AbortSignal.timeout(2_000),
    });
    return response.status !== 503;
  } catch {
    return false;
  }
}
const ready = createServer((_, response) => { response.writeHead(200).end("ready"); });
const storefrontPort = process.env.GATHER_TEST_PORT ?? "5198";
const backofficePort = process.env.GATHER_TEST_BACKOFFICE_PORT ?? "5200";
const readyPort = process.env.GATHER_TEST_READY_PORT ?? "5202";
const demo = { ...process.env, ZS_VAR_GATHER_MODE: "demo", ZS_VAR_GATHER_ADMIN_IDS: "pws_gatheroperator000001" };
// Let the COPY's own `zeroship.workspace.ts` choose the config file and the
// database file, so the migrate step and the two dev servers agree on one
// database inside the copy rather than inheriting the working demo's.
delete demo.ZEROSHIP_CONFIG;
delete demo.DATABASE_URL;
try {
  for (const entry of ["zeroship.jsonc", "zeroship.workspace.ts", "migrations", "package.json", "scripts"])
    await cp(join(source, entry), join(root, entry), { recursive: true });
  for (const entry of await readdir(join(source, "node_modules")))
    await symlink(join(source, "node_modules", entry), join(root, "node_modules", entry)).catch(async error => {
      if (error.code !== "ENOENT") throw error;
      await mkdir(join(root, "node_modules"), { recursive: true });
      await symlink(join(source, "node_modules", entry), join(root, "node_modules", entry));
    });
  await mkdir(join(root, "packages", "shared"), { recursive: true });
  for (const entry of ["src", "locales", "public", "package.json", "lingui.config.ts", "locales.ts"])
    await cp(join(source, "packages", "shared", entry), join(root, "packages", "shared", entry), { recursive: true });
  await symlink(join(source, "packages", "shared", "node_modules"), join(root, "packages", "shared", "node_modules"), "dir");
  for (const app of ["backoffice", "storefront"]) {
    const appRoot = join(root, "apps", app);
    await mkdir(appRoot, { recursive: true });
    for (const entry of ["src", "index.html", "package.json", "vite.config.ts", "tsconfig.json", "lingui.config.ts"])
      await cp(join(source, "apps", app, entry), join(appRoot, entry), { recursive: true });
    await mkdir(join(appRoot, "node_modules", "@gather"), { recursive: true });
    for (const entry of await readdir(join(source, "apps", app, "node_modules"))) {
      if (entry === "@gather") continue;
      await symlink(join(source, "apps", app, "node_modules", entry), join(appRoot, "node_modules", entry));
    }
    await symlink(join(root, "packages", "shared"), join(appRoot, "node_modules", "@gather", "meal-kit"), "dir");
  }
  // ONE migrate for the workspace: one database, applied before either app serves.
  await new Promise((resolve, reject) => {
    const child = start(["migrate"], root, demo);
    child.once("error", reject);
    child.once("exit", code => code === 0 ? resolve() : reject(new Error("Fixture migration failed")));
  });
  for (const app of ["backoffice", "storefront"]) {
    const env = { ...demo,
      GATHER_API_PORT: app === "backoffice" ? process.env.GATHER_TEST_BACKOFFICE_API_PORT ?? "3100" : process.env.GATHER_TEST_API_PORT ?? "3098",
    };
    const port = app === "backoffice" ? backofficePort : storefrontPort;
    const child = start(["exec", "vite", "--port", port], join(root, "apps", app), env);
    child.once("error", error => { console.error(error); close(1); });
    child.once("exit", code => { if (!closing) close(code || 1); });
    const deadline = Date.now() + 240_000;
    while (!closing && Date.now() < deadline) {
      if (await serving(`http://127.0.0.1:${port}/`)) break;
      await new Promise(resolve => setTimeout(resolve, 500));
    }
    if (closing) break;
    if (Date.now() >= deadline) throw new Error(`Fixture app ${app} never answered an RPC on port ${port}`);
    console.log(`[fixture] ${app} serving on port ${port}`);
  }
  // Binding this is the signal Playwright waits for. Vite answers `/` long
  // before the runtime behind it answers a procedure, so a spec started on an
  // open port races the worker; the gate opens only once both apps have
  // returned a real RPC response.
  if (!closing) ready.listen(Number(readyPort), "127.0.0.1", () => console.log(`[fixture] both apps serving; gate open on ${readyPort}`));
} catch (error) { console.error(error); close(1); }
