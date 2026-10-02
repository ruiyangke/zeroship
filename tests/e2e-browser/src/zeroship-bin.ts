// Build the `zeroship` CLI the demos' dev servers run, and report its path.
//
// Each demo's vite-plugin spawns the binary ZEROSHIP_BIN names, else the
// demo's node_modules/.bin/zeroship, else `zeroship` on PATH - none of which
// this tier controls. Asking cargo for the CLI through its own artifact report
// also makes the path follow whatever CARGO_TARGET_DIR the caller exported,
// rather than a fixed target/release layout.

import { spawnSync } from "node:child_process";
import { REPO_ROOT } from "./demos.js";

let cached: string | undefined;

export function zeroshipBin(): string {
  if (cached !== undefined) return cached;
  const cargo = process.env.CARGO ?? "cargo";
  const built = spawnSync(
    cargo,
    ["build", "--message-format=json", "--locked", "--bins", "-p", "zeroship-cli"],
    { cwd: REPO_ROOT, env: process.env, encoding: "utf8", maxBuffer: 256 * 1024 * 1024 },
  );
  if (built.error) throw built.error;
  if (built.status !== 0) {
    throw new Error(`cargo build of the zeroship CLI exited ${built.status}:\n${built.stderr ?? ""}`);
  }
  for (const line of (built.stdout ?? "").split("\n")) {
    if (!line.startsWith("{")) continue;
    const value = JSON.parse(line) as {
      reason?: string;
      target?: { name?: string };
      executable?: string | null;
    };
    if (value.reason === "compiler-artifact" && value.target?.name === "zeroship" && value.executable) {
      cached = value.executable;
      return cached;
    }
  }
  throw new Error("cargo reported no executable for the `zeroship` target");
}
