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
if (!/^[a-z0-9][a-z0-9\-_]{0,63}$/.test(projectName)) {
  console.error(`error: "${projectName}" is not a valid npm package name (lowercase, no spaces)`);
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
