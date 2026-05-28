/* Jest config for @storybook/test-runner.
 *
 * The test runner spawns Jest with `rootDir = process.cwd()`, then
 * builds a Haste module map from there. Inside the zeroship
 * monorepo (with its many sibling worktrees), Jest tries to
 * fingerprint every `package.json` reachable from the workspace root
 * and trips over duplicate `@zeroship/ui` entries in sibling
 * worktrees plus malformed package.json files under `refs/`. Pinning
 * `roots` to `sdks/ui` keeps Haste scoped to the package we're
 * testing. */
const path = require("node:path");
const { getJestConfig } = require("@storybook/test-runner");

const base = getJestConfig();
const packageRoot = path.resolve(__dirname, "..");

module.exports = {
  ...base,
  rootDir: packageRoot,
  roots: [packageRoot],
  /* These ignore patterns scope Jest's file crawl to the package
   * we're testing. We deliberately do NOT ignore `.worktrees/` here:
   * this very package lives under `.worktrees/storybook-test-infra/`,
   * so a `.worktrees/` ignore would drop every story file. With
   * `rootDir` and `roots` pinned to the package root, the crawl never
   * leaves `sdks/ui`, so sibling worktrees are out of scope anyway. */
  testPathIgnorePatterns: [
    "/node_modules/",
    "/storybook-static/",
    "/coverage/",
    "/dist/",
  ],
  modulePathIgnorePatterns: ["/node_modules/", "/storybook-static/"],
  haste: {
    ...(base.haste ?? {}),
    /* Don't try to provide a hasteImplModulePath — just let Jest fall
     * back to the default but scoped to rootDir. */
  },
};
