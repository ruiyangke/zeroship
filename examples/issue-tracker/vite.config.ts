import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";
import { zeroship } from "@zeroship/vite-plugin";

// Dev-runtime port. Every example defaults to 3001 and they collide OPAQUELY
// (the loser hangs rather than reporting a bound port), so this one is
// overridable and picks its own default. Same spelling as db-todos, db-e2e,
// db-chat, hr-system.
const devServerPort = Number(process.env.ISSUE_TRACKER_API_PORT ?? 3007);

export default defineConfig({
  // Migration-first build: the plugin reads `migrations/` and the generated
  // descriptor artifacts. No schema plugin option is needed.
  plugins: [react(), zeroship({ devServerPort })],
  // The dev runtime writes SQLite files under .zeroship/; keep vite's watcher
  // off those volatile DB/journal files.
  server: { watch: { ignored: ["**/.zeroship/**"] } },
});
