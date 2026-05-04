// CSR demo — single React SPA + one RPC endpoint, bundled by `@zeroship/vite-plugin`.
//
// The plugin handles both halves:
//   * Client build: `vite build` walks `index.html`, emits hashed JS/CSS chunks to dist/.
//   * Server build: kicked off automatically from `writeBundle` for any `src/server.ts`
//     it finds — bundles "use server" exports into dist/server/index.js.
//
// After both builds finish, `closeBundle` packs everything into `dist/app.zship`.
import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";
import { zeroship } from "@zeroship/vite-plugin";

export default defineConfig({
  plugins: [react(), zeroship()],
});
