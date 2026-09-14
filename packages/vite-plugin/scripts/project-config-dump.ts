/**
 * Dump the TypeScript reader's view of a `zeroship.jsonc` as canonical JSON.
 *
 *   node --import tsx scripts/project-config-dump.ts <config-path> [--env=<name>]
 *
 * The ONLY consumer is `tests/project_config_gate.sh`, which byte-compares this
 * against `zeroship config show` on the same file. That comparison catches
 * divergent defaults, keys one side silently ignores, and type-coercion
 * differences in one diff, without a third parser policing the first two.
 *
 * It prints nothing else on stdout. Errors go to stderr with exit 1, so the
 * gate can tell "the two disagree" from "one of them refused", which are
 * different findings.
 */

import { resolve } from "node:path";

import {
  canonicalJson,
  loadProjectConfig,
  readProjectConfig,
  resolveProjectConfig,
} from "../src/project-config/index.js";

function main(): void {
  const args = process.argv.slice(2);
  const envFlag = args.find((a) => a.startsWith("--env="));
  const environment = envFlag?.slice("--env=".length);
  const rootFlag = args.find((a) => a.startsWith("--root="));

  if (rootFlag != null) {
    // The WHOLE read path from a directory, including the "no file found ->
    // schema defaults" arm. That arm is the plugin's `zeroship()`-in-a-scratch-
    // directory case and is deliberately absent from the Rust side, so the
    // gate needs a way to observe it.
    const { config } = readProjectConfig(resolve(rootFlag.slice("--root=".length)), { environment });
    process.stdout.write(canonicalJson(config) + "\n");
    return;
  }

  const path = args.find((a) => !a.startsWith("--"));
  if (path == null) {
    console.error("usage: project-config-dump.ts <config-path>|--root=<dir> [--env=<name>]");
    process.exit(1);
  }
  const loaded = loadProjectConfig(resolve(path));
  process.stdout.write(canonicalJson(resolveProjectConfig(loaded, environment)) + "\n");
}

try {
  main();
} catch (e) {
  console.error((e as Error).message);
  process.exit(1);
}
