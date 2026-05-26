import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";
import { zeroship } from "@zeroship/vite-plugin";

export default defineConfig({
  // Stage 5c — schema is read off `default.schema` of the entry by
  // the runtime bootstrap. No plugin option needed: the entry exports
  // `default = { schema, fetch, rpc }` and that's the wire contract.
  //
  // `react()` powers the client SPA in `src/`; `zeroship()` discovers
  // the server procedures in `src/index.ts` and bundles them for the dev
  // runtime. Client + server coexist (mirrors examples/csr-todo).
  plugins: [react(), zeroship()],
  // The dev runtime writes its SQLite files under .zeroship/; keep vite's
  // file watcher off those volatile DB/journal files.
  server: { watch: { ignored: ["**/.zeroship/**"] } },
});
