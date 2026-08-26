import { defineConfig, type Options } from "tsup";

const shared = {
  format: ["esm"],
  dts: true,
  target: "es2022",
  outDir: "dist",
  sourcemap: true,
  treeshake: true,
  splitting: true,
} satisfies Options;

export default defineConfig([
  {
    ...shared,
    // The host runtime entry (`.`): the facade over the prebuilt N-API addon +
    // the pg/mysql2 driver adapters. `pg`/`mysql2` are optionalDependencies
    // resolved at runtime — external so the bundle never inlines them; the addon
    // is loaded via `createRequire` at runtime (a `.node`), never bundled. The
    // authoring DSL + pure-JS recorder live in the separate `zero-migrate` package
    // (imported here, also external).
    entry: {
      index: "src/index.ts",
      // The CLI: the arg-parser (`cli.ts`) + the executable entry
      // (`cli-bin.ts`, mapped to the `zero-migrate` bin). `cli-bin.ts` carries a
      // leading `#!/usr/bin/env node` shebang which esbuild preserves on the entry
      // chunk, so the emitted `dist/cli-bin.js` is directly executable. `zero-migrate`
      // (the DSL) is external so a migration's `import { table } from "zero-migrate"`
      // resolves to the ONE recorder instance this package shares (not a duplicate).
      cli: "src/cli.ts",
      "cli-bin": "src/cli-bin.ts",
    },
    // `tsx` is an optionalDependency the CLI lazily `import()`s to load `.ts`
    // migrations; keep it external so esbuild never inlines the loader.
    external: [
      "pg",
      "mysql2",
      "mysql2/promise",
      "zero-migrate",
      "zero-migrate-node",
      "tsx",
      "tsx/esm/api",
    ],
    clean: true,
  },
]);
