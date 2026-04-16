import * as esbuild from "esbuild";
import { resolve, dirname } from "node:path";
import { fileURLToPath } from "node:url";

const __dirname = dirname(fileURLToPath(import.meta.url));
const root = resolve(__dirname, "..");

await esbuild.build({
  entryPoints: [resolve(root, "src/dev-bootstrap/index.ts")],
  bundle: true,
  format: "esm",
  platform: "neutral",
  target: "es2024",
  outfile: resolve(root, "dist/dev-bootstrap.js"),
  external: [],
  sourcemap: true,
  minify: false,
  banner: {
    js: "// @zeroship/vite-plugin dev bootstrap — runs inside V8 runtime",
  },
});

console.log("[zeroship] dev-bootstrap.js built successfully");
