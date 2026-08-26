// Resolve the addon the host suite loads, and refuse one that cannot be trusted.
//
// The `.node` is a build artifact, gitignored and never committed, so every
// checkout carries whatever that machine last built. Seventeen files used to
// resolve it themselves and fall back silently, which is how a binary older than
// its own sources got loaded and reported green - and how a months-old copy once
// produced a plausible mostly-red suite that read as drift rather than as a
// missing rebuild.
//
// Freshness is checked as an ORDERING, not an equality. There is no committed
// copy to diff against, but "was this built after the code it is built from" is
// answerable from mtimes alone, and that is the question that matters.
//
// The check applies to an explicitly supplied ZERO_MIGRATE_ADDON_PATH as well as
// to the default. An environment variable records intent, not freshness: a shell
// profile or a wrapper script can point at a stale binary just as easily.
//
// Importing this module performs the check and throws on failure, so a suite that
// cannot trust its addon fails to load rather than running against the wrong one.

import { existsSync, readdirSync, statSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const HERE = dirname(fileURLToPath(import.meta.url));
const REPO_ROOT = resolve(HERE, "../../../..");
const CRATES = join(REPO_ROOT, "crates");
const REBUILD = "pnpm --filter zero-migrate-node build";

/** The `.node` napi writes for this platform (its Linux triple carries `-gnu`). */
function defaultAddonPath(): string {
  const { platform, arch } = process;
  const abi = platform === "linux" ? "-gnu" : "";
  return join(CRATES, "zero-migrate-node", `zero-migrate-node.${platform}-${arch}${abi}.node`);
}

/** The most recently modified Rust source the addon is built from, or null when
 *  none can be found - which is a cannot-answer, not a pass. */
function newestRustSource(): { path: string; mtimeMs: number } | null {
  if (!existsSync(CRATES)) return null;
  let newest: { path: string; mtimeMs: number } | null = null;
  for (const entry of readdirSync(CRATES, { recursive: true, withFileTypes: true })) {
    if (!entry.isFile() || !entry.name.endsWith(".rs")) continue;
    const full = join(entry.parentPath ?? entry.path, entry.name);
    const { mtimeMs } = statSync(full);
    if (!newest || mtimeMs > newest.mtimeMs) newest = { path: full, mtimeMs };
  }
  return newest;
}

function stamp(ms: number): string {
  return new Date(ms).toISOString();
}

function resolveVerifiedAddon(): string {
  const explicit = process.env.ZERO_MIGRATE_ADDON_PATH;
  const addon = explicit ?? defaultAddonPath();
  const origin = explicit ? "ZERO_MIGRATE_ADDON_PATH" : "the default path for this platform";

  if (!existsSync(addon)) {
    throw new Error(
      `the host suite's addon is missing.\n` +
        `  wanted: ${addon}\n` +
        `  from:   ${origin}\n` +
        `  build it with: ${REBUILD}`,
    );
  }

  const newest = newestRustSource();
  if (newest === null) {
    throw new Error(
      `cannot tell whether the addon is current: no .rs sources found under ${CRATES}.\n` +
        `  The freshness check could not run, which is not the same as passing.\n` +
        `  addon: ${addon}`,
    );
  }

  const addonMs = statSync(addon).mtimeMs;
  if (newest.mtimeMs > addonMs) {
    throw new Error(
      `the host suite's addon is older than the sources it is built from.\n` +
        `  addon:      ${stamp(addonMs)}  ${addon}\n` +
        `  newest src: ${stamp(newest.mtimeMs)}  ${newest.path}\n` +
        `  from:       ${origin}\n` +
        `  rebuild it with: ${REBUILD}\n` +
        `  (mtime is a conservative proxy: a comment-only edit also trips this, and a\n` +
        `   rebuild is the correct answer either way.)`,
    );
  }

  process.env.ZERO_MIGRATE_ADDON_PATH = addon;
  return addon;
}

/** The verified `.node` every host test loads. Reading it is what runs the check. */
export const ADDON_PATH: string = resolveVerifiedAddon();
