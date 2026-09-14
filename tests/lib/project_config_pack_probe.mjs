/**
 * Pack a throwaway app and report whether a sentinel string reaches the
 * `.zship` ARCHIVE BYTES.
 *
 * Used by the scope-invariant check in `tests/project_config_gate.sh`: a
 * `zeroship.jsonc` is never packed). The three arms differ in exactly one
 * variable each:
 *
 *   --plant=none   no sentinel anywhere            -> expect ABSENT
 *   --plant=root   sentinel in <root>/zeroship.jsonc -> expect ABSENT
 *   --plant=dist   sentinel in <root>/dist/<file>    -> expect FOUND
 *   --dist=.       make the pack root the project root -> expect REJECTED
 *
 * `root` and `dist` are THE one-variable pair: same sentinel, same packer call,
 * same search code, and the only difference is whether the file holding it sits
 * inside the directory the packer walks or one level above it. Without the
 * `dist` arm, a search that could not find a sentinel that IS there would pass
 * the `root` arm forever and prove nothing.
 *
 * WHY IT SEARCHES THE DECOMPRESSED TAR. The archive is `tar.zst`; a plaintext
 * sentinel cannot appear in the compressed bytes, so grepping those would be
 * the vacuous check. Decompressing and searching the tar stream sees BOTH the
 * entry names and every blob body, so a future path that carried the file as a
 * content-addressed blob with no `manifest.assets` entry still trips it. This
 * keeps the check effective if the archive layout changes.
 *
 *   node --import tsx tests/lib/project_config_pack_probe.mjs \
 *        --sentinel=<s> --plant=none|root|dist
 *
 * Prints one line: `FOUND`, `ABSENT`, or `REJECTED` for an unsafe dist.
 */

import { mkdtempSync, mkdirSync, writeFileSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { zstdDecompressSync } from "node:zlib";

import { emitZship } from "../../packages/vite-plugin/src/zship.ts";

function arg(name, fallback) {
  const hit = process.argv.find((a) => a.startsWith(`--${name}=`));
  return hit == null ? fallback : hit.slice(name.length + 3);
}

const sentinel = arg("sentinel");
const plant = arg("plant", "root");
const dist = arg("dist", "dist");
if (sentinel == null) {
  console.error("usage: project_config_pack_probe.mjs --sentinel=<s> [--plant=none|root|dist]");
  process.exit(2);
}

const root = mkdtempSync(join(tmpdir(), "zs-pack-probe-"));
try {
  mkdirSync(join(root, dist), { recursive: true });
  writeFileSync(join(root, dist, "index.html"), "<!doctype html><title>probe</title>\n");

  // The project config always exists; only its CONTENT differs between the
  // `none` arm and the other two, so "the packer walked the root" and "the
  // sentinel was in the file" stay separable.
  const configBody =
    plant === "none"
      ? '{ "name": "probe", "control": "http://localhost:9090", "runtime_date": "2026-08-14",' +
        ` "build": { "mode": "full", "dist": ${JSON.stringify(dist)}, "output": "dist/app.zship" },` +
        ' "migrations": { "dir": "migrations", "out": "generated/zeroship" } }\n'
      : `{ "name": "probe", "control": "http://${sentinel}.example", "runtime_date": "2026-08-14",` +
        ` "build": { "mode": "full", "dist": ${JSON.stringify(dist)}, "output": "dist/app.zship" },` +
        ' "migrations": { "dir": "migrations", "out": "generated/zeroship" } }\n';
  writeFileSync(join(root, "zeroship.jsonc"), configBody);

  if (plant === "dist") {
    // THE CONTROL. Same sentinel, one directory lower.
    writeFileSync(join(root, dist, "sentinel.txt"), `${sentinel}\n`);
  }

  let result;
  try {
    result = await emitZship({
      root,
      distDir: join(root, dist),
      outputPath: join(root, ".probe-output", "app.zship"),
      userHasDefaultFetch: false,
      migrations: false,
      silent: true,
    });
  } catch (error) {
    if (String(error).includes("must be a descendant") && String(error).includes("project root")) {
      process.stdout.write("REJECTED\n");
      process.exitCode = 0;
      result = null;
    } else {
      throw error;
    }
  }

  if (result != null) {
    const archive = readFileSync(result.outputPath);
    const tarBytes = zstdDecompressSync(archive);
    const found = tarBytes.includes(Buffer.from(sentinel, "utf8"));
    process.stdout.write(found ? "FOUND\n" : "ABSENT\n");
  }
} finally {
  rmSync(root, { recursive: true, force: true });
}
