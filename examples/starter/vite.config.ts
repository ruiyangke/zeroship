import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";
import { zeroship } from "@zeroship/vite-plugin";

// Dev-runtime port. Overridable so this example can run alongside the other
// examples (they all default to 3001 otherwise and collide opaquely) — see
// tests/e2e-browser/demos.ts for the port band this feeds.
const devServerPort = Number(process.env.STARTER_API_PORT ?? 3001);

export default defineConfig({
  plugins: [react(), zeroship({ devServerPort })],
});
