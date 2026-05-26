import { copyFile, mkdir } from "node:fs/promises";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const root = join(here, "..");

await mkdir(join(root, "dist"), { recursive: true });
await copyFile(join(root, "src", "styles.css"), join(root, "dist", "styles.css"));
await copyFile(join(root, "src", "tailwind.css"), join(root, "dist", "tailwind.css"));
