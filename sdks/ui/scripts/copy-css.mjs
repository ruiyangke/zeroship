import { copyFile, mkdir, readFile, writeFile } from "node:fs/promises";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const root = join(here, "..");

await mkdir(join(root, "dist"), { recursive: true });

async function inlineImports(filePath, seen = new Set()) {
  const absolutePath = resolve(filePath);
  if (seen.has(absolutePath)) return "";
  seen.add(absolutePath);

  const source = await readFile(absolutePath, "utf8");
  const imports = [];
  const body = source.replace(
    /^@import\s+"([^"]+)";\n?/gm,
    (_match, specifier) => {
      imports.push(resolve(dirname(absolutePath), specifier));
      return "";
    },
  );

  const inlined = await Promise.all(imports.map((importPath) => inlineImports(importPath, seen)));
  return [...inlined, body].filter(Boolean).join("\n");
}

await writeFile(
  join(root, "dist", "styles.css"),
  await inlineImports(join(root, "src", "styles.css")),
);
await copyFile(join(root, "src", "tailwind.css"), join(root, "dist", "tailwind.css"));
