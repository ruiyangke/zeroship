// CSR demo — single React SPA + one RPC endpoint, bundled by `@zeroship/vite-plugin`.
//
// `vite build` builds both halves, then the plugin packs them:
//   * Client: walks `index.html`, emits hashed JS/CSS chunks to dist/.
//   * Worker: the plugin's `zeroship` environment bundles the "use server"
//     exports of the `src/server.ts` it finds into dist/server/index.js.
//
// After both builds finish, the plugin packs everything into `dist/app.zship`.
import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";
import { zeroship } from "@zeroship/vite-plugin";

// Dev-runtime port. Overridable so this example can run alongside the other
// examples (they all default to 3001 otherwise and collide opaquely). The test
// fixture (tests/fixture/settings.ts) gives its dev target a free port
// through it.
const devServerPort = Number(process.env.CSR_TODO_API_PORT ?? 3001);

export default defineConfig({
  plugins: [react(), zeroship({ devServerPort })],
});
