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
import { defineConfig, type Plugin } from "vite";
import react from "@vitejs/plugin-react";
import { zeroship } from "@zeroship/vite-plugin";

const CLIENT_MANIFEST_VIRTUAL_ID = "virtual:zeroship/client-manifest";
const CLIENT_MANIFEST_RESOLVED_ID = "\0" + CLIENT_MANIFEST_VIRTUAL_ID;
const RUNTIME_ORIGIN = "http://localhost:3001";
export const SSR_DEV_PROXY_PATTERN =
  "^/(?!(@vite/|@react-refresh|@id/|@fs/|__vite_ping|__open-in-editor|src/|node_modules/|assets/|_zs/|__zeroship_|.*\\.[\\w]+(?:[?#].*)?$)).*";
const SSR_DEV_PROXY_RE = new RegExp(SSR_DEV_PROXY_PATTERN);

export function shouldProxySsrDevPath(path: string): boolean {
  const normalized = path.startsWith("/") ? path : `/${path}`;
  return SSR_DEV_PROXY_RE.test(normalized);
}

function devClientManifestPlugin(): Plugin {
  return {
    name: "ssr-blog:dev-client-manifest",
    apply: "serve",
    enforce: "pre",
    resolveId(id) {
      if (id === CLIENT_MANIFEST_VIRTUAL_ID) return CLIENT_MANIFEST_RESOLVED_ID;
      return null;
    },
    load(id) {
      if (id !== CLIENT_MANIFEST_RESOLVED_ID) return null;
      return "export default {};";
    },
  };
}

export default defineConfig({
  plugins: [devClientManifestPlugin(), react(), zeroship()],
  server: {
    proxy: {
      [SSR_DEV_PROXY_PATTERN]: {
        target: RUNTIME_ORIGIN,
        changeOrigin: true,
      },
    },
  },
  build: {
    // Required for the `virtual:zeroship/client-manifest` virtual module
    // to have anything to inline — the plugin reads `dist/.vite/manifest.json`
    // off disk after the client build's writeBundle, before the SSR build runs.
    manifest: true,
  },
});
