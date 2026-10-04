import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";
import { zeroship } from "@zeroship/vite-plugin";

// Dev-runtime port. Overridable so this example can run alongside the other
// examples (they all default to 3001 otherwise and collide opaquely). The test
// fixture (tests/fixture/settings.ts) gives its dev target a free port
// through it.
const devServerPort = Number(process.env.STARTER_API_PORT ?? 3001);

export default defineConfig({
  plugins: [react(), zeroship({ devServerPort })],
});
