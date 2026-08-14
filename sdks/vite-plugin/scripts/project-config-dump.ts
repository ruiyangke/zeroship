/**
 * Dump the TypeScript reader's view of a `zeroship.jsonc` as canonical JSON.
 *
 *   node --import tsx scripts/project-config-dump.ts <config-path> [--env=<name>]
 *
 * The ONLY consumer is `tests/project_config_gate.sh`, which byte-compares this
 * against `zeroship config show` on the same file. That comparison is the whole
 * of "two parsers, one file" (proposal 7.2 check 3): it catches divergent
 * defaults, a key one side silently ignores, and type-coercion differences, in
 * one diff, without a third parser being written to police the first two.
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
    // directory case and the half of proposal 7.3 the Rust side deliberately
    // does not have, so the gate needs a way to observe it.
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
