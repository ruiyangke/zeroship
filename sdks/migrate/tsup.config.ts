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
    // The public DSL entry (`.`) + the HOST facade (`./host`, §D.3) + the pure-JS
    // host recorder (`./host-recorder`, §D.1). `pg`/`mysql2` are optionalDependencies
    // resolved at runtime — external so the bundle never inlines them; the addon is
    // loaded via `createRequire` at runtime (a `.node`), never bundled.
    entry: {
      index: "src/index.ts",
      host: "src/host/index.ts",
      "host-recorder": "src/host-recorder.ts",
    },
    external: ["pg", "mysql2", "mysql2/promise"],
    clean: true,
  },
  // The engine-embedded recorder artifact (DSL redesign S0.5): ONE
  // self-contained ESM file the `zeroship-migrate` crate `include_str!`s as the
  // in-V8 `@zeroship/migrate` module (replaces the hand-kept `migrate_ops.js`
  // twin). No code-splitting (must be a single file for `include_str!`), no
  // `.d.ts` (runtime artifact only). `@zeroship/db` stays external — the engine
  // module graph resolves it, exactly as the npm package does.
  {
    ...shared,
    entry: { "embedded-recorder": "src/embedded-recorder.ts" },
    splitting: false,
    dts: false,
    clean: false,
    external: ["@zeroship/db"],
  },
]);
