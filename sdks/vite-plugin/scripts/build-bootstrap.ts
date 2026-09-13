import { build } from "esbuild";
import { resolve, dirname } from "path";
import { fileURLToPath } from "url";

const __dirname = dirname(fileURLToPath(import.meta.url));
const src = resolve(__dirname, "../src/dev-bootstrap/index.ts");
const out = resolve(__dirname, "../dist/dev-bootstrap.js");
const bootstrapDevSrc = resolve(__dirname, "../../bootstrap/src/dev-entry.ts");

await build({
  entryPoints: [src],
  bundle: true,
  format: "esm",
  platform: "neutral",
  target: "es2024",
  outfile: out,
  // Bundle directly from workspace sources so this package can build in a
  // fresh worktree without requiring prebuilt dist artifacts from sibling
  // framework-internal packages.
  alias: {
    "@zeroship/bootstrap/dev": bootstrapDevSrc,
  },
  // vite/module-runner MUST be bundled: the V8 runtime has no module resolver
  // for external npm packages. ModuleRunner is small and self-contained.
  //
  // BUT `zeroship` MUST stay external — bundling it would bake the stub's
  // empty `env` into the bootstrap; at runtime the V8 kernel synthesizes the
  // real `"zeroship"` virtual module (`crates/zeroship-runtime/src/core/zeroship_module.rs`)
  // that exposes the live native namespaces.
  external: ["zeroship"],
  banner: {
    js: "// Auto-generated dev bootstrap for zeroship V8 runtime\n",
  },
});

console.log(`[build-bootstrap] ${out}`);
