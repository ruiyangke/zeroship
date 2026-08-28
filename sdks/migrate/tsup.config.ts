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
  // self-contained ESM file. No code-splitting (single file by construction), no
  // `.d.ts` (runtime artifact only). `@zeroship/db` stays external.
  //
  // THIS COMMENT USED TO SAY THE `zeroship-migrate` CRATE `include_str!`s THIS
  // FILE AS THE IN-V8 `@zeroship/migrate` MODULE. IT DOES NOT, AND NO CRATE DOES:
  // `grep -rn 'sdks/migrate' crates/` returns nothing. The only Rust
  // `include_str!` of an embedded recorder is
  // crates/zeroship-migrate-server/tests/author_and_apply_pg.rs:102-105, and it
  // names `packages/zero-migrate/dist/embedded-recorder.js` - the ENGINE's build,
  // not this one. The identically-worded comment in
  // packages/zero-migrate/tsup.config.ts describes the artifact that is really
  // consumed; this one was copied beside it and never re-checked.
  //
  // What actually imports this file: three of this package's own tests
  // (tests/ops.test.ts, tests/column-facets-lockstep.test.ts,
  // tests/sequences-exclusion.test.ts). Verified 2026-08-28. If you are about to
  // rely on a Rust consumer existing, it does not.
  {
    ...shared,
    entry: { "embedded-recorder": "src/embedded-recorder.ts" },
    splitting: false,
    dts: false,
    clean: false,
    external: ["@zeroship/db"],
  },
]);
