#!/usr/bin/env node
// Materialize the canonical skills in `skills/` into the template's per-agent
// directories. Claude Code and opencode read the same SKILL.md format and
// differ only in where they look, so one source serves both.
//
// `skills/` is the source and is NOT published (package.json `files` ships
// `bin` and `template`). The copies under `template/` are what a scaffolded
// project receives. Run this after editing anything in `skills/`;
// `test/skills.test.js` fails when the copies drift.

import { readdirSync, readFileSync, writeFileSync, mkdirSync, rmSync, existsSync } from "node:fs";
import { join, dirname } from "node:path";
import { fileURLToPath } from "node:url";

const ROOT = join(dirname(fileURLToPath(import.meta.url)), "..");
export const SOURCE = join(ROOT, "skills");
export const TARGETS = [
  join(ROOT, "template", ".claude", "skills"),
  join(ROOT, "template", ".opencode", "skills"),
];

export function readSkills() {
  return readdirSync(SOURCE, { withFileTypes: true })
    .filter((e) => e.isDirectory())
    .map((e) => e.name)
    .sort()
    .map((name) => ({ name, body: readFileSync(join(SOURCE, name, "SKILL.md"), "utf-8") }));
}

export function sync() {
  const skills = readSkills();
  for (const target of TARGETS) {
    if (existsSync(target)) rmSync(target, { recursive: true });
    for (const { name, body } of skills) {
      mkdirSync(join(target, name), { recursive: true });
      writeFileSync(join(target, name, "SKILL.md"), body);
    }
  }
  return skills.map((s) => s.name);
}

if (process.argv[1] === fileURLToPath(import.meta.url)) {
  const names = sync();
  console.log(`synced ${names.length} skills to ${TARGETS.length} agent directories:`);
  for (const n of names) console.log(`  ${n}`);
}
