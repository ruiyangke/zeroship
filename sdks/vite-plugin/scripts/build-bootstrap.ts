import { build } from "esbuild";
import { resolve, dirname } from "path";
import { fileURLToPath } from "url";

const __dirname = dirname(fileURLToPath(import.meta.url));
const src = resolve(__dirname, "../src/dev-bootstrap/index.ts");
const out = resolve(__dirname, "../dist/dev-bootstrap.js");

await build({
  entryPoints: [src],
  bundle: true,
  format: "esm",
  platform: "neutral",
  target: "es2024",
  outfile: out,
  // vite/module-runner MUST be bundled: the V8 runtime has no module resolver
  // for external npm packages. ModuleRunner is small and self-contained.
  //
  // BUT `zeroship` MUST stay external — bundling it would bake the stub's
  // empty `env` into the bootstrap; at runtime the V8 kernel synthesizes the
  // real `"zeroship"` virtual module (`crates/runtime/src/init.rs::ZEROSHIP_MODULE_JS`)
  // that exposes the live `env.db` plugin handle. `@zeroship/db`
  // re-exports `installSchema(schema, env.db)`, which plants typed
  // Collection wrappers on the supplied env handle — must read the
  // live env, not the stub.
  external: ["zeroship"],
  banner: {
    js: "// Auto-generated dev bootstrap for zeroship V8 runtime\n",
  },
});

console.log(`[build-bootstrap] ${out}`);
