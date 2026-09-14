// Resolve the addon produced by the host suite's build command.
//
// The `.node` is a gitignored build artifact. The package test command builds it
// before Node starts this suite, so freshness is established by construction
// instead of inferred by scanning Rust source mtimes.

import { existsSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const HERE = dirname(fileURLToPath(import.meta.url));
const CRATES = join(resolve(HERE, "../../../.."), "crates");
const REBUILD = "pnpm --filter zeroship-migrate-node build";

/** The `.node` napi writes for this platform (its Linux triple carries `-gnu`). */
function defaultAddonPath(): string {
  const { platform, arch } = process;
  const abi = platform === "linux" ? "-gnu" : "";
  return join(CRATES, "zeroship-migrate-node", `zeroship-migrate-node.${platform}-${arch}${abi}.node`);
}

function resolveBuiltAddon(): string {
  const addon = defaultAddonPath();
  if (!existsSync(addon)) {
    throw new Error(
      `the host suite's addon is missing.\n` +
        `  wanted: ${addon}\n` +
        `  build it with: ${REBUILD}`,
    );
  }

  process.env.ZERO_MIGRATE_ADDON_PATH = addon;
  return addon;
}

/** The freshly built `.node` every host test loads. */
export const ADDON_PATH: string = resolveBuiltAddon();
