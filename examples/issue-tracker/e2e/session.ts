import { createHmac } from "node:crypto";
import { execFileSync } from "node:child_process";
import { readFileSync } from "node:fs";

import type { BrowserContext } from "@playwright/test";

/**
 * Sign a dev session the way the runtime expects, and hand it to the browser
 * as a cookie.
 *
 * The dev runtime resolves `__zeroship_dev_session` BEFORE dispatch
 * (`crates/runtime/src/core/serve.rs`) and threads the decoded user through the
 * same `call_fetch_handler_with_user` path the worker uses for the gateway's
 * `ZeroShip-User` header, so a request carrying this cookie reaches the real
 * handlers with a real identity. The envelope is
 * `base64url(user_json) "." hex-hmac-sha256(secret, payload)`, defined by
 * `dev_auth::sign_dev_session`.
 *
 * This is NOT a way to skip the login UI's own logic -- it is how a
 * already-signed-in browser looks. The login form itself is not covered by
 * these specs.
 */

const DEV_USER = {
  id: "pws_devalice0000000000",
  email: "alice@localhost",
  name: "Alice Dev",
  avatar: null,
  email_verified: true,
  scopes: ["openid", "profile", "email"],
};

/**
 * The secret is minted per dev server by the Vite plugin and passed to the
 * spawned `zeroship serve` child, so it is read back out of that child's
 * environment. There is no file to read it from and no endpoint that exposes
 * it, which is the point.
 */
function devAuthSecret(runtimePort: number): string {
  let pids: string[];
  try {
    pids = execFileSync("lsof", ["-ti", `tcp:${runtimePort}`], { encoding: "utf8" })
      .trim()
      .split("\n")
      .filter(Boolean);
  } catch {
    throw new Error(
      `nothing is listening on the dev runtime port ${runtimePort}. ` +
        `The vite client and the zeroship runtime are separate ports; this is the runtime one.`,
    );
  }
  if (pids.length === 0) throw new Error(`no pid found listening on ${runtimePort}`);

  // EVERY pid on the port is checked, not just the first. More than one process
  // holds this socket (the vite parent and the spawned `zeroship serve` child),
  // and only the child carries the secret -- taking `head -1` picked the parent
  // and reported "no secret" for a dev server that had one.
  const reasons: string[] = [];
  for (const pid of pids) {
    let environ: string[];
    try {
      environ = readFileSync(`/proc/${pid}/environ`, "utf8").split("\0");
    } catch (err) {
      reasons.push(`${pid}: environ unreadable (${(err as Error).message})`);
      continue;
    }
    const secret = environ
      .find((entry) => entry.startsWith("ZEROSHIP_DEV_AUTH_SECRET="))
      ?.slice("ZEROSHIP_DEV_AUTH_SECRET=".length);
    if (secret) return secret;
    reasons.push(`${pid}: no ZEROSHIP_DEV_AUTH_SECRET`);
  }

  // Refusing here rather than continuing is deliberate. Without the secret
  // every mutation 401s, and the specs would report a wall of failures about
  // the app when the real problem is the harness.
  throw new Error(
    `no process on port ${runtimePort} carries ZEROSHIP_DEV_AUTH_SECRET, so an ` +
      `authenticated spec cannot pass. Checked: ${reasons.join("; ")}`,
  );
}

export function signDevSession(runtimePort: number): string {
  const payload = Buffer.from(JSON.stringify(DEV_USER)).toString("base64url");
  const mac = createHmac("sha256", devAuthSecret(runtimePort)).update(payload).digest("hex");
  return `${payload}.${mac}`;
}

export async function signIn(
  context: BrowserContext,
  { runtimePort, baseURL }: { runtimePort: number; baseURL: string },
): Promise<void> {
  const url = new URL(baseURL);
  await context.addCookies([
    {
      name: "__zeroship_dev_session",
      value: signDevSession(runtimePort),
      domain: url.hostname,
      path: "/",
      httpOnly: true,
      sameSite: "Lax",
    },
  ]);
}

export const devUser = DEV_USER;
