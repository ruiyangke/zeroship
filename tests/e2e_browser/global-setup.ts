import { spawnSync } from "node:child_process";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";

const __dirname = dirname(fileURLToPath(import.meta.url));

/**
 * Bring the external zeroship stack up before any spec runs. Spawns
 * scripts/up.sh inheriting stdio so the bring-up log streams to the Playwright
 * output; a non-zero exit aborts the whole run (no browser tests against a
 * dead stack). On success it has written .stack.json for the specs/helpers.
 */
export default async function globalSetup(): Promise<void> {
  // Ensure @playwright/test resolves to the on-PATH runner's copy (idempotent).
  const link = join(__dirname, "scripts", "link-playwright.sh");
  const linkRes = spawnSync("bash", [link], { stdio: "inherit" });
  if (linkRes.status !== 0) {
    throw new Error(`link-playwright.sh exited ${linkRes.status}`);
  }

  const up = join(__dirname, "scripts", "up.sh");
  const res = spawnSync("bash", [up], { stdio: "inherit" });
  if (res.status !== 0) {
    throw new Error(`browser-E2E stack bring-up failed (scripts/up.sh exited ${res.status})`);
  }
}
