import { defineConfig } from "vite";
import path from "path";
import react from "@vitejs/plugin-react";
import tailwindcss from "@tailwindcss/vite";
import { zeroship } from "@zeroship/vite-plugin";

// Dev-runtime port. Overridable so this example can run alongside the other
// examples, and so scripts/smoke.sh can start its own server on a free port
// instead of racing whatever else is on 3001. Examples that do NOT set this
// share the 3001 default and collide OPAQUELY -- the loser hangs rather than
// reporting a bound port. Same spelling as examples/db-todos, db-e2e, db-chat.
const devServerPort = Number(process.env.HR_SYSTEM_API_PORT ?? 3001);

export default defineConfig({
  plugins: [tailwindcss(), react(), zeroship({ devServerPort })],
  resolve: {
    alias: {
      "@": path.resolve(__dirname, "./src"),
    },
  },
});
