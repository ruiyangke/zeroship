// Build the `zeroship` CLI the dev server runs, and report its path.
//
// The vite-plugin spawns the binary ZEROSHIP_BIN names, else the project's
// node_modules/.bin/zeroship, else `zeroship` on PATH - none of which this
// fixture controls. Asking cargo for the CLI through its own artifact report
// also makes the path follow whatever CARGO_TARGET_DIR the caller exported,
// rather than a fixed target/ layout.

import { spawnSync } from "node:child_process";
import { resolve } from "node:path";
import { fileURLToPath } from "node:url";

const example = fileURLToPath(new URL("../../", import.meta.url));
const root = resolve(example, "../..");

export function buildZeroshipBin(): string {
  const cargo = process.env.CARGO ?? "cargo";
  const built = spawnSync(
    cargo,
    ["build", "--message-format=json", "--locked", "--bins", "-p", "zeroship-cli"],
    { cwd: root, env: process.env, encoding: "utf8", maxBuffer: 256 * 1024 * 1024 },
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
      return value.executable;
    }
  }
  throw new Error("cargo reported no executable for the `zeroship` target");
}
