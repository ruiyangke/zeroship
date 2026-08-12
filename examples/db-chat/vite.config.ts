import { defineConfig } from "vite";
import { zeroship } from "@zeroship/vite-plugin";

// Dev-runtime port. Overridable so this example can run alongside the other
// examples, and so scripts/smoke.sh can start its own server on a free port
// instead of racing whatever else is on 3001. Examples that do NOT set this
// share the 3001 default and collide OPAQUELY -- the second one hangs rather
// than reporting a bound port. Same spelling as examples/db-todos and
// examples/db-e2e.
const devServerPort = Number(process.env.DB_CHAT_API_PORT ?? 3001);

export default defineConfig({
  plugins: [zeroship({ devServerPort })],
  // The dev runtime writes its SQLite files under .zeroship/; keep vite's file
  // watcher off those volatile DB/journal files.
  server: { watch: { ignored: ["**/.zeroship/**"] } },
});
