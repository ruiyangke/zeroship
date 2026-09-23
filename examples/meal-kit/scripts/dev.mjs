// Start both apps, and wait for each to answer before starting the next.
//
// The dev tier gives every local app the same app id and keeps that app's
// workflow deployment rows beside the database file. Both apps bind one
// database here, so both sets of rows are one set: the second runtime to
// register loses the activation, spends 65s failing to claim it, exits 1, and
// serves only after the supervisor restarts it. Waiting for a real RPC answer
// is what makes `pnpm dev` return once the pair is actually usable rather than
// once two ports are open.

import { spawn } from "node:child_process";
import { fileURLToPath } from "node:url";

const root = fileURLToPath(new URL("../", import.meta.url));
const apps = [
  { name: "backoffice", url: "http://localhost:5199/" },
  { name: "storefront", url: "http://127.0.0.1:5197/" },
];

await new Promise((resolve, reject) => {
  const child = spawn("pnpm", ["--dir", "packages/shared", "i18n:compile"], { cwd: root, stdio: "inherit" });
  child.on("error", reject);
  child.on("exit", code => code === 0 ? resolve() : reject(new Error("Translation compilation failed")));
});

const children = new Set();
let closing = false;
function close(code) {
  if (closing) return;
  closing = true;
  for (const child of children) {
    try { process.kill(-child.pid, "SIGTERM"); } catch (error) { if (error.code !== "ESRCH") throw error; }
  }
  process.exitCode = code;
}
process.on("SIGINT", () => close(0));
process.on("SIGTERM", () => close(0));

// Answering `/` proves Vite is up; answering an RPC proves the runtime behind
// it is, which is the half that races.
async function serving(url) {
  try {
    const response = await fetch(new URL("/__zeroship/v1/gather.session", url), {
      method: "POST", body: JSON.stringify({ json: {} }),
      headers: { "content-type": "application/json", "X-Method": "GET" },
      signal: AbortSignal.timeout(2_000),
    });
    return response.status !== 503;
  } catch {
    return false;
  }
}

for (const app of apps) {
  const child = spawn("pnpm", ["exec", "vite"], { cwd: `${root}apps/${app.name}`, stdio: "inherit", env: process.env, detached: true });
  children.add(child);
  child.on("error", error => { console.error(error); close(1); });
  child.on("exit", code => { children.delete(child); if (!closing) close(code || 1); });
  const deadline = Date.now() + 180_000;
  while (!closing && Date.now() < deadline) {
    if (await serving(app.url)) break;
    await new Promise(resolve => setTimeout(resolve, 500));
  }
  if (closing) break;
  if (Date.now() >= deadline) {
    console.error(`[gather] ${app.name} did not start serving at ${app.url}`);
    close(1);
    break;
  }
  console.log(`[gather] ${app.name} serving at ${app.url}`);
}
if (!closing) console.log("Storefront: http://127.0.0.1:5197 · Back office: http://localhost:5199");
