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
import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";
import tailwindcss from "@tailwindcss/vite";
import { zeroship } from "@zeroship/vite-plugin";

const devServerPort = Number(process.env.ZEROSHIP_BUILDER_API_PORT ?? "3002");
const controlUrl = process.env.CONTROL_URL ?? "http://localhost:9090";

export default defineConfig({
  plugins: [react(), tailwindcss(), zeroship({ devServerPort })],
  server: {
    // The docker-compose stack serves the builder behind Caddy at
    // builder.zeroship.localhost. Vite's dev server rejects requests whose
    // Host header isn't allowlisted (DNS-rebinding protection), so permit the
    // dev domain (leading "." matches the host and any subdomain).
    allowedHosts: [".zeroship.localhost"],
    proxy: {
      "/auth": {
        target: controlUrl,
        changeOrigin: true,
      },
    },
  },
});
