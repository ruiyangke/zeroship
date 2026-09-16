import { spawnSync } from "node:child_process";
import { existsSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));

export default async function globalSetup(): Promise<void> {
  const link = join(here, "scripts", "link-playwright.sh");
  const linkResult = spawnSync("bash", [link], { stdio: "inherit" });
  if (linkResult.status !== 0) {
    throw new Error(`link-playwright.sh exited ${linkResult.status}`);
  }

  const rawBaseURL = process.env.ZEROSHIP_AUTH_UI_BASE_URL;
  if (!rawBaseURL) {
    throw new Error("ZEROSHIP_AUTH_UI_BASE_URL is required");
  }
  const baseURL = new URL(rawBaseURL);
  if (baseURL.protocol !== "http:" || baseURL.hostname !== "127.0.0.1") {
    throw new Error(`refusing non-loopback auth UI target: ${baseURL.href}`);
  }

  const authLog = process.env.ZEROSHIP_AUTH_UI_AUTH_LOG;
  if (!authLog || !existsSync(authLog)) {
    throw new Error("ZEROSHIP_AUTH_UI_AUTH_LOG must name the live auth log");
  }

  const response = await fetch(new URL("/readyz", baseURL), {
    signal: AbortSignal.timeout(5_000),
  });
  if (!response.ok) {
    throw new Error(`zeroship-auth readiness returned HTTP ${response.status}`);
  }
}
