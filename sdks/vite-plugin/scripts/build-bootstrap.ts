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
  external: ["vite/module-runner"],
  banner: {
    js: "// Auto-generated dev bootstrap for zeroship V8 runtime\n",
  },
});

console.log(`[build-bootstrap] ${out}`);
