import { defineConfig } from "vite";
import { zeroship } from "@zeroship/vite-plugin";

const devServerPort = Number(process.env.STORAGE_GALLERY_API_PORT ?? 3021);

export default defineConfig({
  plugins: [zeroship({ devServerPort })],
});
