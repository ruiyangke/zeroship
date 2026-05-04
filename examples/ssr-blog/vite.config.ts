// SSR demo vite config.
//
// Two builds happen here:
//   1. Client build (this config): emits `index.html` + `assets/*.js` for the
//      browser. We turn on `build.manifest: true` so a `dist/.vite/manifest.json`
//      is written; the SSR bundle imports it at build time to inject the right
//      hashed JS filename into the rendered HTML.
//   2. Server build: kicked off automatically by `@zeroship/vite-plugin` from
//      `writeBundle`. It bundles `src/server.ts` (which exports `default.fetch`)
//      into `dist/server/index.js`.
//
// The plugin exposes `virtual:zeroship/client-manifest` to the SSR build so
// `src/server.ts` can `import clientManifest from "virtual:zeroship/client-manifest"`
// and look up the hashed filenames by source path.
//
// The plugin then packs both into `dist/app.zship`.
/// <reference types="@zeroship/vite-plugin/types" />
import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";
import { zeroship } from "@zeroship/vite-plugin";

export default defineConfig({
  plugins: [react(), zeroship()],
  build: {
    // Required for the `virtual:zeroship/client-manifest` virtual module
    // to have anything to inline — the plugin reads `dist/.vite/manifest.json`
    // off disk after the client build's writeBundle, before the SSR build runs.
    manifest: true,
  },
});
