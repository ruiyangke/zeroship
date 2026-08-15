#!/usr/bin/env node
// create-zeroship-app — copy the `template/` dir into a new project,
// rewrite package.json with the user-supplied name, and print the
// three commands they need to run next. No prompts, no framework
// choice in v1: one opinionated template that wires up db + storage + kv.
//
// Invocation:
//   npm create zeroship-app my-app
//   # or explicitly:
//   npx create-zeroship-app my-app

import { cpSync, existsSync, readFileSync, writeFileSync, mkdirSync, readdirSync, statSync } from "node:fs";
import { join, dirname, resolve, basename } from "node:path";
import { fileURLToPath } from "node:url";

const __dirname = dirname(fileURLToPath(import.meta.url));
const TEMPLATE_DIR = resolve(__dirname, "..", "template");

function usage() {
  console.error("usage: npm create zeroship-app <project-name>");
  process.exit(1);
}

const projectName = process.argv[2];
if (!projectName) usage();
if (!/^[a-z0-9][a-z0-9-]{0,62}$/.test(projectName)) {
  console.error(
    `error: invalid project name "${projectName}": ` +
      "use 1-63 lowercase letters, digits, or hyphens; start with a letter or digit",
  );
  process.exit(1);
}

const targetDir = resolve(process.cwd(), projectName);
if (existsSync(targetDir)) {
  console.error(`error: directory "${projectName}" already exists`);
  process.exit(1);
}

function copyTree(src, dest) {
  mkdirSync(dest, { recursive: true });
  for (const name of readdirSync(src)) {
    const s = join(src, name);
    const d = join(dest, name);
    if (statSync(s).isDirectory()) {
      copyTree(s, d);
    } else {
      // Rename `_gitignore` → `.gitignore` (npm publishes dot-prefixed
      // files inconsistently; the leading underscore is the convention).
      const finalDest = name === "_gitignore" ? join(dest, ".gitignore") : d;
      cpSync(s, finalDest);
    }
  }
}

copyTree(TEMPLATE_DIR, targetDir);

// Rewrite package.json `name` to match the project.
const pkgPath = join(targetDir, "package.json");
const pkg = JSON.parse(readFileSync(pkgPath, "utf-8"));
pkg.name = projectName;
writeFileSync(pkgPath, JSON.stringify(pkg, null, 2) + "\n");

// Rewrite `zeroship.jsonc`'s `name` by SPLICING that one scalar rather than
// re-serialising. The file is JSONC and its comments are half of what it is
// for; a JSON.parse + stringify round trip would delete every one of them. The
// template value is a unique literal, so a plain replace is safe and the result
// is byte-identical everywhere else.
//
// `runtime_date` is NOT stamped with today's date, and that is a decision.
// Stamping it would make two creators who scaffold on different days receive
// different files, and the golden-path harness re-runs THIS scaffolder and
// requires byte equality against `examples/scaffold-app`. The comparison
// could then never hold. The date buys nothing that pays for that: nothing
// reads `runtime_date`, and its whole stated value is that projects are in the
// habit of carrying one. The template's date does that. When a dated behaviour
// gate actually exists, setting it becomes a deliberate act rather than a
// silent side effect of `npm create`.
const cfgPath = join(targetDir, "zeroship.jsonc");
if (existsSync(cfgPath)) {
  const before = readFileSync(cfgPath, "utf-8");
  const after = before.replace(
    '"name": "zeroship-app"',
    `"name": ${JSON.stringify(projectName)}`,
  );
  // A template whose literal moved must fail loudly. Writing the file back
  // unchanged would scaffold every project under the name "zeroship-app", and
  // the failure would only surface as two creators' apps colliding.
  if (after === before) {
    console.error('error: zeroship.jsonc no longer contains the template\'s "zeroship-app" name');
    process.exit(1);
  }
  writeFileSync(cfgPath, after);
}

console.log(`✓ scaffolded ${basename(targetDir)}`);
console.log("");
console.log("next steps:");
console.log(`  cd ${projectName}`);
console.log("  npm install");
console.log("  npm run dev");
console.log("");
console.log("  → SQLite boots automatically (zero-config local db)");
console.log("  → Files persist in .zeroship/ (already git-ignored)");
console.log("  → open http://localhost:5173");
