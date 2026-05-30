import { defineConfig, type Options } from "tsup";

const shared = {
  format: ["esm"],
  dts: true,
  target: "es2022",
  outDir: "dist",
  sourcemap: true,
  treeshake: true,
  splitting: false,
  // `zeroship` (server entry only) + the optional React peer stay external.
  // The client/types entries are pure browser code with no runtime deps.
  external: ["zeroship", "react", "react/jsx-runtime"],
} satisfies Options;

export default defineConfig({
  ...shared,
  entry: ["src/server.ts", "src/client.ts", "src/react.tsx", "src/types.ts"],
  clean: true,
});
