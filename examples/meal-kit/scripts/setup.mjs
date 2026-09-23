// One-time local setup: the two app environments, then the shared schema.
//
// There is no connection to configure between the apps - they bind the same
// database - so the only values here are the ones the back office reads for
// the demo payment simulation and the administrator list.

import { readFile, writeFile } from "node:fs/promises";
import { fileURLToPath } from "node:url";
import { spawn } from "node:child_process";

const root = fileURLToPath(new URL("../", import.meta.url));
const settings = {
  backoffice: {
    ZS_VAR_GATHER_MODE: "demo",
    ZS_VAR_GATHER_ADMIN_IDS: "pws_gatheroperator000001",
  },
  storefront: {
    ZS_VAR_GATHER_MODE: "demo",
    ZS_VAR_GATHER_ADMIN_IDS: "pws_gatheroperator000001",
  },
};
const value = (text, name) => text.match(new RegExp(`^${name}=(.*)$`, "m"))?.[1].trim();
for (const [app, defaults] of Object.entries(settings)) {
  const path = `${root}apps/${app}/.env`;
  let text = await readFile(path, "utf8").catch(error => {
    if (error.code !== "ENOENT") throw error;
    return "";
  });
  for (const [key, fallback] of Object.entries(defaults))
    if (!value(text, key)) text += `${text.endsWith("\n") || !text ? "" : "\n"}${key}=${fallback}\n`;
  await writeFile(path, text, { mode: 0o600 });
}
await new Promise((resolve, reject) => {
  const child = spawn("pnpm", ["migrate"], { cwd: root, stdio: "inherit", env: process.env });
  child.on("error", reject);
  child.on("exit", code => code === 0 ? resolve() : reject(new Error("Migration failed")));
});
console.log("Applied the shared schema and wrote each app's .env. Run pnpm dev from this example.");
