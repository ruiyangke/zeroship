// SSG demo vite config.
//
// We want three prerendered HTML pages (`/`, `/about`, `/docs/intro`)
// emitted as plain assets. No worker, no SSR, no JS at all.
//
// Approach: a tiny `ssgContentPlugin` walks `content/` and copies every
// `*.html` file into `dist/`. The `@zeroship/vite-plugin` runs in
// `mode: "static"`, which:
//   - skips the SSR sub-build entirely;
//   - injects a virtual stub `rollupOptions.input` so Vite still has an
//     entry to chew on (otherwise it errors out with "no input");
//   - deletes the stub chunk in `generateBundle` so no `_empty-<hash>.js`
//     ships in the deploy artifact.
//
// Result: `dist/` ends up with just the copied HTML, and `app.zsapp` has
// `worker: null` plus per-route static rules for every page.
import { defineConfig, type Plugin } from "vite";
import { promises as fs } from "node:fs";
import { dirname, join, relative, resolve } from "node:path";
import { zeroship } from "@zeroship/vite-plugin";

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
    // Run BEFORE zeroship's closeBundle so the copied content is on
    // disk by the time the .zsapp emitter walks dist/.
    enforce: "pre",
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
    // mode: "static" tells the plugin to skip the SSR sub-build and
    // inject the no-op stub input so Vite doesn't error on an empty
    // build. The emitter then walks dist/ (after content copy) and
    // packs the HTML files as assets.
    zeroship({ mode: "static" }),
  ],
});
