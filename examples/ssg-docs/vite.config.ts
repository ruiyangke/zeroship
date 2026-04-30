// SSG demo vite config.
//
// We want three prerendered HTML pages (`/`, `/about`, `/docs/intro`)
// emitted as plain assets. No worker, no SSR.
//
// Approach: a tiny custom plugin walks `content/`, copies every `*.html`
// file into `dist/` preserving the path layout. The `@zeroship/vite-plugin`
// then sees them as plain assets, and `buildRules()` in `zsapp.ts` emits
// per-route Match::Exact rules + the SPA-style fallback.
import { defineConfig, type Plugin } from "vite";
import { promises as fs } from "node:fs";
import { dirname, join, relative, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { zeroship } from "@zeroship/vite-plugin";

const HERE = dirname(fileURLToPath(import.meta.url));

/** Recursively walk a directory, returning every file path. */
async function walk(dir: string, base = dir): Promise<string[]> {
  const entries = await fs.readdir(dir, { withFileTypes: true });
  const files: string[] = [];
  for (const e of entries) {
    const p = join(dir, e.name);
    if (e.isDirectory()) files.push(...(await walk(p, base)));
    else if (e.isFile()) files.push(relative(base, p));
  }
  return files;
}

/** Copy `content/**` into the build outDir. */
function ssgContentPlugin(): Plugin {
  let outDir = "dist";
  let root = "";
  return {
    name: "ssg:copy-content",
    apply: "build",
    configResolved(config) {
      root = config.root;
      outDir = config.build.outDir;
    },
    async closeBundle() {
      const src = resolve(root, "content");
      const dst = resolve(root, outDir);
      try {
        await fs.access(src);
      } catch {
        return; // no content dir — nothing to do
      }
      const files = await walk(src);
      for (const f of files) {
        const from = join(src, f);
        const to = join(dst, f);
        await fs.mkdir(dirname(to), { recursive: true });
        await fs.copyFile(from, to);
      }
    },
  };
}

export default defineConfig({
  plugins: [
    ssgContentPlugin(),
    // The zeroship plugin discovers no `src/server.ts`, so it skips the
    // server build, leaves `worker: null`, and emits per-route static
    // rules for every `*.html` it finds in the dist tree.
    zeroship(),
  ],
  build: {
    rollupOptions: {
      // No JS/HTML entry — the SSG demo is pure static. The default
      // input would be `index.html` in the project root, which we don't
      // have. Point Rollup at an empty virtual entry so it produces an
      // empty bundle and we ship only the copied content.
      input: { _empty: resolve(HERE, "vite.empty.js") },
    },
  },
});
