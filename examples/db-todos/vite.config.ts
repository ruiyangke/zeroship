import { defineConfig } from "vite";
import { zeroship } from "@zeroship/vite-plugin";

export default defineConfig({
  // `schema: "./src/index.ts"` — Stage 1 schema auto-discovery
  // (smoke). The entry module also re-exports its `default.schema`,
  // so the runtime falls back to it even when this field is removed.
  // The build records the resolved path in `manifest.exports.schema`;
  // the runtime ignores the field until Stage 2 wires the read.
  plugins: [zeroship({ schema: "./src/index.ts" })],
});
