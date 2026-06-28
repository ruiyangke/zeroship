import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";
import { zeroship } from "@zeroship/vite-plugin";

export default defineConfig({
  // Migration-first builds read committed migrations and generated
  // descriptor artifacts. No schema plugin option is needed.
  //
  // `react()` powers the client SPA in `src/`; `zeroship()` discovers
  // the server procedures in `src/index.ts` and bundles them for the dev
  // runtime. Client + server coexist (mirrors examples/csr-todo).
  plugins: [react(), zeroship()],
  // The dev runtime writes its SQLite files under .zeroship/; keep vite's
  // file watcher off those volatile DB/journal files.
  server: { watch: { ignored: ["**/.zeroship/**"] } },
});
