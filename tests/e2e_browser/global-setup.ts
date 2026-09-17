import { spawnSync } from "node:child_process";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";

const __dirname = dirname(fileURLToPath(import.meta.url));

/**
 * Point @playwright/test at the on-PATH runner's copy, which is idempotent and
 * is what makes the Nix-provided browser resolvable. The stack these specs
 * address is provisioned externally.
 */
export default async function globalSetup(): Promise<void> {
  const link = join(__dirname, "scripts", "link-playwright.sh");
  const linkRes = spawnSync("bash", [link], { stdio: "inherit" });
  if (linkRes.status !== 0) {
    throw new Error(`link-playwright.sh exited ${linkRes.status}`);
  }
}
