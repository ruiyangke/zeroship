import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";
import tailwindcss from "@tailwindcss/vite";
import { zeroship } from "@zeroship/vite-plugin";

// Dev-runtime port. Every example defaults to 3001 and they collide OPAQUELY
// (the loser hangs rather than reporting a bound port), so this one is
// overridable and picks its own default. Same spelling as db-todos, db-e2e,
// db-chat, hr-system.
const devServerPort = Number(process.env.ISSUE_TRACKER_API_PORT ?? 3007);

// The CLIENT port, which is a different thing from `devServerPort` above (that
// one is the zeroship dev RUNTIME). `strictPort` is deliberate: vite's default
// is to walk to the next free port, and this example was observed serving on
// 5174 because something else held 5173. A browser test with a hardcoded
// baseURL then points at whatever else is listening, and reports failures about
// an app it never loaded. Failing to boot is the better outcome.
const webPort = Number(process.env.ISSUE_TRACKER_WEB_PORT ?? 5183);

export default defineConfig({
  // Migration-first build: the plugin reads `migrations/` and the generated
  // descriptor artifacts. No schema plugin option is needed.
  plugins: [tailwindcss(), react(), zeroship({ devServerPort })],
  resolve: {
    // ONE React instance, always.
    //
    // This app consumes React-facing workspace SDK entry points. Vite can
    // serve their built bundles from outside this package's tree (a `/@fs/`
    // URL), where a bare `react` import may otherwise resolve through a
    // different package tree. A second instance has its own hook dispatcher
    // and makes components fail with "Invalid hook call".
    //
    // The symptom named React versions, which sent me looking at the catalog
    // pins -- both were already 19.2.5. Two instances, not two versions.
    dedupe: ["react", "react-dom"],
  },
  server: {
    port: webPort,
    strictPort: true,
    // The dev runtime writes SQLite files under .zeroship/; keep vite's watcher
    // off those volatile DB/journal files.
    watch: { ignored: ["**/.zeroship/**"] },
  },
});
