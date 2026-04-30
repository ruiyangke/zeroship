// SSR demo vite config.
//
// Two builds happen here:
//   1. Client build (this config): emits `index.html` + `_assets/*.js` for the
//      browser. We turn on `build.manifest: true` so a `dist/.vite/manifest.json`
//      is written; the SSR bundle imports it at build time to inject the right
//      hashed JS filename into the rendered HTML.
//   2. Server build: kicked off automatically by `@zeroship/vite-plugin` from
//      `writeBundle`. It bundles `src/server.ts` (which re-exports the SSR
//      entry) into `dist/server/index.js`.
//
// The plugin then packs both into `dist/app.zsapp`.
import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";
import { zeroship } from "@zeroship/vite-plugin";

export default defineConfig({
  plugins: [react(), zeroship()],
  build: {
    // Vite emits `dist/.vite/manifest.json` mapping source paths to hashed
    // filenames. The SSR bundle reads this at build time so it can inject
    // <script src="/assets/main-<hash>.js"> into the rendered HTML.
    //
    // KNOWN GAP: today the vite-plugin's `.zsapp` emitter explicitly skips
    // `.vite/` (see `zsapp.ts` collectFiles), so the SSR bundle CAN'T import
    // it from the bundled output. We work around this by reading the file
    // from disk in the SSR build's `closeBundle` and inlining it — see
    // README under "Known limitations".
    manifest: true,
  },
});
