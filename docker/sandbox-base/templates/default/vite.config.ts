import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

export default defineConfig({
  base: "./",
  plugins: [react()],
  server: {
    host: "0.0.0.0",
    port: 5173,
    strictPort: true,
    // Vite blocks unknown hosts by default; the preview proxy will
    // forward arbitrary `*.preview.zeroship.ai` hostnames, so we
    // explicitly allow any host for dev. Tighten in production by
    // setting `allowedHosts: [...]`.
    allowedHosts: true,
  },
});
