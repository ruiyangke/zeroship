/**
 * Pack a throwaway app and report whether a sentinel string reaches the
 * `.zship` ARCHIVE BYTES.
 *
 * Used by `tests/project_config_gate.sh` check 1 (the scope invariant: a
 * `zeroship.jsonc` is never packed). The three arms differ in exactly one
 * variable each:
 *
 *   --plant=none   no sentinel anywhere            -> expect ABSENT
 *   --plant=root   sentinel in <root>/zeroship.jsonc -> expect ABSENT
 *   --plant=dist   sentinel in <root>/dist/<file>    -> expect FOUND
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
 * content-addressed blob with no `manifest.assets` entry still trips it - which
 * is exactly the failure the proposal asks this check to survive (1.2).
 *
 *   node --import tsx tests/lib/project_config_pack_probe.mjs \
 *        --sentinel=<s> --plant=none|root|dist
 *
 * Prints one line: `FOUND` or `ABSENT`.
 */

import { mkdtempSync, mkdirSync, writeFileSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { zstdDecompressSync } from "node:zlib";

import { emitZship } from "../../sdks/vite-plugin/src/zship.ts";

function arg(name, fallback) {
  const hit = process.argv.find((a) => a.startsWith(`--${name}=`));
  return hit == null ? fallback : hit.slice(name.length + 3);
}

const sentinel = arg("sentinel");
const plant = arg("plant", "root");
if (sentinel == null) {
  console.error("usage: project_config_pack_probe.mjs --sentinel=<s> [--plant=none|root|dist]");
  process.exit(2);
}

const root = mkdtempSync(join(tmpdir(), "zs-pack-probe-"));
try {
  mkdirSync(join(root, "dist"), { recursive: true });
  writeFileSync(join(root, "dist", "index.html"), "<!doctype html><title>probe</title>\n");

  // The project config always exists; only its CONTENT differs between the
  // `none` arm and the other two, so "the packer walked the root" and "the
  // sentinel was in the file" stay separable.
  const configBody =
    plant === "none"
      ? '{ "name": "probe", "control": "http://localhost:9090", "runtime_date": "2026-08-14",' +
        ' "build": { "mode": "full", "dist": "dist", "output": "dist/app.zship" },' +
        ' "migrations": { "dir": "migrations", "out": "generated/zeroship" } }\n'
      : `{ "name": "probe", "control": "http://${sentinel}.example", "runtime_date": "2026-08-14",` +
        ' "build": { "mode": "full", "dist": "dist", "output": "dist/app.zship" },' +
        ' "migrations": { "dir": "migrations", "out": "generated/zeroship" } }\n';
  writeFileSync(join(root, "zeroship.jsonc"), configBody);

  if (plant === "dist") {
    // THE CONTROL. Same sentinel, one directory lower.
    writeFileSync(join(root, "dist", "sentinel.txt"), `${sentinel}\n`);
  }

  const result = await emitZship({
    root,
    distDir: join(root, "dist"),
    userHasDefaultFetch: false,
    migrations: false,
    silent: true,
  });

  const archive = readFileSync(result.outputPath);
  const tarBytes = zstdDecompressSync(archive);
  const found = tarBytes.includes(Buffer.from(sentinel, "utf8"));
  process.stdout.write(found ? "FOUND\n" : "ABSENT\n");
} finally {
  rmSync(root, { recursive: true, force: true });
}
