import { spawnSync } from "node:child_process";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";

const __dirname = dirname(fileURLToPath(import.meta.url));

/** Tear the external stack down after the whole run (kill PIDs, rm PG, rm work). */
export default async function globalTeardown(): Promise<void> {
  const down = join(__dirname, "scripts", "down.sh");
  spawnSync("bash", [down], { stdio: "inherit" });
}
