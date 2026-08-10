import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";
import { zeroship } from "@zeroship/vite-plugin";

// Dev-runtime port. Overridable so this example can run alongside the other
// examples (they all default to 3001 otherwise and collide opaquely) — see
// tests/e2e-browser/src/demos.ts for the port band this feeds.
const devServerPort = Number(process.env.DB_TODOS_API_PORT ?? 3001);

export default defineConfig({
  // Migration-first builds read committed migrations and generated
  // descriptor artifacts. No schema plugin option is needed.
  //
  // `react()` powers the client SPA in `src/`; `zeroship()` discovers
  // the server procedures in `src/index.ts` and bundles them for the dev
  // runtime. Client + server coexist (mirrors examples/csr-todo).
  plugins: [react(), zeroship({ devServerPort })],
  // The dev runtime writes its SQLite files under .zeroship/; keep vite's
  // file watcher off those volatile DB/journal files.
  server: { watch: { ignored: ["**/.zeroship/**"] } },
});
