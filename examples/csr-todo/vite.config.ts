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

// Dev-runtime port. Overridable so this example can run alongside the other
// examples (they all default to 3001 otherwise and collide opaquely) — see
// tests/e2e-browser/demos.ts for the port band this feeds.
const devServerPort = Number(process.env.CSR_TODO_API_PORT ?? 3001);

export default defineConfig({
  plugins: [react(), zeroship({ devServerPort })],
});
