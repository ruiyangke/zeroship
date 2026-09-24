// Apply the shared schema, through the SAME two settings the apps use.
//
// The migrate CLI resolves `zeroship.jsonc` from its cwd and `DATABASE_URL`
// from the dev default relative to that cwd, so running it anywhere but the
// workspace root would write a schema into a file no app opens. Naming both
// here makes the apply and the two dev servers agree by construction rather
// than by the operator happening to be in the right directory.
//
// THE TARGET IS THE DATABASE. `main` is the one this workspace declares, and
// both apps make it their `env.db`; naming either app here would have been a
// coin toss that said nothing about which schema is written.

import { spawn } from "node:child_process";
import { fileURLToPath } from "node:url";
import { useWorkspaceEnvironment, workspaceRoot } from "../zeroship.workspace.ts";

useWorkspaceEnvironment();
const cli = fileURLToPath(
  new URL("../node_modules/@zeroship/vite-plugin/dist/cli/migrate-dev.js", import.meta.url),
);
const child = spawn(process.execPath, [cli, "--database=main", ...process.argv.slice(2)], {
  cwd: workspaceRoot,
  stdio: "inherit",
  env: process.env,
});
child.on("exit", code => { process.exitCode = code ?? 1; });
