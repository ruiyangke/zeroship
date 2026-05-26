import { createHash } from "node:crypto";
import { mkdir, readdir, readFile, rm, writeFile } from "node:fs/promises";
import { dirname, extname, join, relative, resolve } from "node:path";
import { spawnSync } from "node:child_process";

const distDir = resolve(process.argv[2] ?? "dist");
const outputPath = resolve(process.argv[3] ?? join(distDir, "app.zship"));
const stageDir = join(distDir, ".zship-stage");

const contentTypes = new Map([
  [".html", "text/html; charset=utf-8"],
  [".js", "text/javascript; charset=utf-8"],
  [".mjs", "text/javascript; charset=utf-8"],
  [".css", "text/css; charset=utf-8"],
  [".json", "application/json; charset=utf-8"],
  [".svg", "image/svg+xml"],
  [".png", "image/png"],
  [".jpg", "image/jpeg"],
  [".jpeg", "image/jpeg"],
  [".gif", "image/gif"],
  [".webp", "image/webp"],
  [".ico", "image/x-icon"],
  [".txt", "text/plain; charset=utf-8"],
  [".woff", "font/woff"],
  [".woff2", "font/woff2"],
]);

function sha256(bytes) {
  return createHash("sha256").update(bytes).digest("hex");
}

function contentType(path) {
  return contentTypes.get(extname(path).toLowerCase()) ?? "application/octet-stream";
}

async function walk(dir) {
  const out = [];
  for (const entry of await readdir(dir, { withFileTypes: true })) {
    const abs = join(dir, entry.name);
    if (abs === outputPath || abs.startsWith(`${stageDir}/`)) continue;
    if (entry.name === ".zship-stage" || entry.name === "app.zship") continue;
    if (entry.isDirectory()) {
      out.push(...await walk(abs));
    } else if (entry.isFile()) {
      out.push(abs);
    }
  }
  return out.sort();
}

await rm(outputPath, { force: true });
await rm(stageDir, { recursive: true, force: true });
await mkdir(join(stageDir, "blobs"), { recursive: true });

const assets = {};
const blobs = new Map();
for (const file of await walk(distDir)) {
  const rel = relative(distDir, file).split("\\").join("/");
  if (rel.endsWith(".map")) continue;
  const bytes = await readFile(file);
  const hash = sha256(bytes);
  blobs.set(hash, bytes);
  assets[`/${rel}`] = {
    hash,
    content_type: contentType(rel),
    size: bytes.length,
  };
}

if (!assets["/index.html"]) {
  throw new Error("pack-zship: dist/index.html is required for the SPA shell");
}

const resources = {};
if (Object.keys(assets).some((path) => path.startsWith("/assets/"))) {
  resources["/assets/*"] = {
    static: { try: ["$path"] },
    cache: { max_age: 31536000, immutable: true },
  };
}
for (const path of ["/favicon.ico", "/robots.txt", "/sitemap.xml"]) {
  if (assets[path]) resources[path] = { static: { try: [path] } };
}
resources["/[...rest]"] = {
  static: { try: ["$path", "/index.html"] },
};

const manifest = {
  version: 1,
  assets,
  runtime_assets: {},
  asset_version: 0,
  sourcemaps: {},
  metadata: {
    compiler: "zeroship-sandbox-template",
    built_at: new Date().toISOString(),
  },
  resources,
  transformer: "json",
};

await writeFile(join(stageDir, "manifest.json"), JSON.stringify(manifest));
for (const [hash, bytes] of [...blobs.entries()].sort(([a], [b]) => a.localeCompare(b))) {
  await writeFile(join(stageDir, "blobs", hash), bytes);
}

await mkdir(dirname(outputPath), { recursive: true });
const blobArgs = [...blobs.keys()].sort().map((hash) => `blobs/${hash}`);
const tar = spawnSync("sh", [
  "-lc",
  `tar --format=ustar -cf - manifest.json ${blobArgs.map((x) => `'${x}'`).join(" ")} | zstd -q -f -o '${outputPath}'`,
], {
  cwd: stageDir,
  stdio: "inherit",
});
if (tar.status !== 0) {
  throw new Error(`pack-zship: tar/zstd failed with status ${tar.status}`);
}
await rm(stageDir, { recursive: true, force: true });

console.log(`packed ${outputPath} (${Object.keys(assets).length} assets, ${blobs.size} blobs)`);
