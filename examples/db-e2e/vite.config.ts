import { defineConfig } from "vite";
import { zeroship } from "@zeroship/vite-plugin";

const devServerPort = Number(process.env.DB_E2E_API_PORT ?? 3001);

export default defineConfig({
  plugins: [zeroship({ devServerPort })],
});
