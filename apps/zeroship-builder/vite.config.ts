// Vite config for the zeroship-builder app.
//
// `@zeroship/vite-plugin` does the heavy lifting:
//   - server-function transform: any module starting with `"use server"`
//     gets compiled into RPC stubs the client side can call as plain
//     async functions, while the server-side code is bundled into the
//     fetch handler that ships in the .appbundle.
//   - dev mode: spins a tiny zeroship runtime in-process so calls hit
//     the real server-functions during `vite dev`.
//   - build mode: emits dist/ + zeroship.appbundle.
import { defineConfig, loadEnv, type PluginOption } from "vite";
import react from "@vitejs/plugin-react";
import tailwindcss from "@tailwindcss/vite";
import { zeroship } from "@zeroship/vite-plugin";
import path from "node:path";

export default defineConfig(({ mode }) => {
  const env = loadEnv(mode, process.cwd(), "VITE_");
  // Cast each plugin to `PluginOption` so TS's structural-comparison
  // doesn't spiral when Vite + tailwind + zeroship plugins share
  // identically-shaped (but distinct) Plugin types.
  const plugins: PluginOption[] = [
    react() as unknown as PluginOption,
    tailwindcss() as unknown as PluginOption,
    zeroship() as unknown as PluginOption,
  ];
  return {
    plugins,
    resolve: {
      alias: {
        "@": path.resolve(import.meta.dirname, "./src/client"),
      },
    },
    server: {
      // While developing locally we proxy to the same control / sandbox
      // services the dashboard talks to — the deployed build proxies
      // identically through env vars on the deployed app.
      proxy: {
        "/api/control": env.VITE_PROXY_CONTROL ?? "http://localhost:9090",
        // The preview iframe in the workspace points at `/apps/<name>/`
        // which is path-style routing on the production gateway. In dev
        // we forward those to the local zeroship-gate so the iframe
        // shows the live deployed app instead of vite's SPA fallback.
        "/apps": {
          target: env.VITE_PROXY_GATEWAY ?? "http://localhost:8001",
          changeOrigin: true,
        },
      },
    },
  };
});
