"use server";
// Projects server functions — the console is a PURE creator app: it
// crafts + previews apps in a sandbox and holds NO control-plane
// credential. There is no deploy, no deployed-app dashboard. A
// "project" is a per-thread sandbox session, tracked in the builder's
// own KV store (`internal/persist.ts`); env + logs are files inside the
// project's sandbox.
//
// See docs/superpowers/specs/2026-05-31-console-pure-creator-app-design.md.
//
// Surfaces:
//   - PROJECTS  — list/get/create/delete/archive/unarchive over KV
//                 (the project registry + the archive set).
//   - ENV       — getEnv/setEnv/deleteEnv: read-modify-write of a single
//                 `.env` file in the sandbox project root (collapses the
//                 old vars+secrets split — a preview sandbox has no
//                 secret vault).
//   - LOGS      — getLogs: tail `.zeroship/dev.log` (the preview-start
//                 command redirects the dev-server stdout/stderr there).

import { action, mutation } from "@zeroship/rpc/server";
import { typedIdFromUuid } from "@zeroship/server/typed-id";
import { persistGet, persistSet } from "./internal/persist.js";
import { currentCreatorId } from "./internal/creator-scope.js";
import { readSandboxFileFor, writeSandboxFileFor } from "./sandbox.js";

// ─── project registry: KV-backed ────────────────────────────────
//
// A project == a per-thread sandbox session. There is no control plane
// to enumerate apps from, so the registry IS the project list: a
// per-creator ordered set of `ProjectRecord`s persisted in KV
// (`internal/persist.ts`). The archive flag lives on the record (no
// separate archive set — one source of truth).
//
// Scoping: per current creator. The platform session identifies the
// creator (`currentUser()`, from the gateway-verified `ZeroShip-User`);
// the dev synthetic user is a single id, so the dashboard reads one
// stable list during dev.

export interface ProjectRecord {
  id: string;
  name: string;
  created_at: string;
  updated_at: string;
  /** Soft-delete flag. Archived projects are hidden from the default
   *  Home view but kept intact. */
  archived?: boolean;
}

// The acting creator's id resolves via `internal/creator-scope.ts`
// (shared with the issues/quality namespaces, SEC-10): platform subject
// when authenticated, the stable dev id behind `ZEROSHIP_DEV=1`, and a
// fail-closed throw otherwise.

function projectsKey(creatorId: string): string {
  return `projects:${creatorId}`;
}

async function loadProjects(): Promise<ProjectRecord[]> {
  const list = await persistGet<ProjectRecord[] | null>(
    projectsKey(currentCreatorId()),
    null,
  );
  return Array.isArray(list) ? list : [];
}

async function saveProjects(projects: ProjectRecord[]): Promise<void> {
  await persistSet(projectsKey(currentCreatorId()), projects);
}

export const listProjects = action(async (): Promise<ProjectRecord[]> => {
  return loadProjects();
}, { id: "projects.listProjects" });

export const getProject = action(async (id: string): Promise<ProjectRecord> => {
  const projects = await loadProjects();
  const found = projects.find((p) => p.id === id);
  if (found) return found;
  // A project the registry doesn't know about (e.g. an old link or a
  // hand-typed id). Surface a minimal record so the workspace can still
  // open the sandbox rather than dead-ending on "not found".
  return {
    id,
    name: id,
    created_at: new Date().toISOString(),
    updated_at: new Date().toISOString(),
  };
}, { id: "projects.getProject" });

/**
 * Mint a new project (a per-thread sandbox session). Generates a typed
 * `prj_` id locally — there is no control plane to allocate one — and
 * records it in the per-creator registry. The id is opaque to the route
 * (`/p/:appId/*`) and becomes the sandbox's `projectSourceId`.
 */
export const createProject = action(async (input: {
  name: string;
}): Promise<ProjectRecord> => {
  const name = input?.name?.trim() || "untitled";
  const now = new Date().toISOString();
  const record: ProjectRecord = {
    id: typedIdFromUuid("prj", crypto.randomUUID()),
    name,
    created_at: now,
    updated_at: now,
  };
  const projects = await loadProjects();
  projects.push(record);
  await saveProjects(projects);
  return record;
}, { id: "projects.createProject" });

export const deleteProject = action(async (
  id: string,
): Promise<{ deleted: boolean }> => {
  const projects = await loadProjects();
  const next = projects.filter((p) => p.id !== id);
  await saveProjects(next);
  return { deleted: next.length !== projects.length };
}, { id: "projects.deleteProject" });

export const archiveProject = mutation(async (
  input: { appId: string },
): Promise<{ archived: boolean }> => {
  await setArchived(input.appId, true);
  return { archived: true };
}, { id: "projects.archiveProject" });

export const unarchiveProject = mutation(async (
  input: { appId: string },
): Promise<{ archived: boolean }> => {
  await setArchived(input.appId, false);
  return { archived: false };
}, { id: "projects.unarchiveProject" });

async function setArchived(id: string, archived: boolean): Promise<void> {
  const projects = await loadProjects();
  const found = projects.find((p) => p.id === id);
  if (!found) return;
  found.archived = archived;
  found.updated_at = new Date().toISOString();
  await saveProjects(projects);
}

// ─── env: a single `.env` in the sandbox project root ────────────
//
// vars+secrets collapse to ONE `.env` (a preview sandbox has no secret
// vault). The canvas reads/writes it via the existing
// `/sandboxes/:id/files/{path}` GET/PUT. Vite auto-restarts the dev
// server on `.env` change, so edits apply on the next preview reload.

const ENV_PATH = ".env";

export interface EnvVar {
  key: string;
  value: string;
}

export const getEnv = action(async (
  appId: string,
): Promise<{ vars: EnvVar[] }> => {
  const text = await readSandboxFileFor(appId, ENV_PATH);
  return { vars: parseDotenv(text ?? "") };
}, { id: "projects.getEnv" });

export const setEnv = action(async (
  input: { appId: string; key: string; value: string },
): Promise<void> => {
  const key = normalizeEnvKey(input.key);
  const existing = parseDotenv((await readSandboxFileFor(input.appId, ENV_PATH)) ?? "");
  const next = existing.filter((v) => v.key !== key);
  next.push({ key, value: input.value });
  await writeSandboxFileFor(input.appId, ENV_PATH, serializeDotenv(next));
}, { id: "projects.setEnv" });

export const deleteEnv = action(async (
  input: { appId: string; key: string },
): Promise<void> => {
  const key = normalizeEnvKey(input.key);
  const existing = parseDotenv((await readSandboxFileFor(input.appId, ENV_PATH)) ?? "");
  const next = existing.filter((v) => v.key !== key);
  // Only write back when something actually changed — avoids spurious
  // dev-server restarts when deleting a key that isn't there.
  if (next.length !== existing.length) {
    await writeSandboxFileFor(input.appId, ENV_PATH, serializeDotenv(next));
  }
}, { id: "projects.deleteEnv" });

function normalizeEnvKey(key: string): string {
  const trimmed = (key ?? "").trim();
  if (!trimmed) throw new Error("env key is required");
  return trimmed;
}

/**
 * Parse simple `KEY=value` lines. Blank lines and `#` comments are
 * skipped; surrounding single/double quotes on the value are stripped.
 * Last assignment wins (mirrors dotenv semantics). Deliberately small —
 * a preview `.env` is a flat key/value list, not a full shell parser.
 */
export function parseDotenv(text: string): EnvVar[] {
  const out = new Map<string, string>();
  for (const rawLine of text.split(/\r?\n/)) {
    const line = rawLine.trim();
    if (!line || line.startsWith("#")) continue;
    const eq = line.indexOf("=");
    if (eq <= 0) continue;
    const key = line.slice(0, eq).trim();
    if (!key) continue;
    let value = line.slice(eq + 1).trim();
    if (
      value.length >= 2 &&
      ((value.startsWith('"') && value.endsWith('"')) ||
        (value.startsWith("'") && value.endsWith("'")))
    ) {
      value = value.slice(1, -1);
    }
    out.set(key, value);
  }
  return [...out.entries()].map(([key, value]) => ({ key, value }));
}

/**
 * Serialize back to `KEY=value` lines. Values containing whitespace,
 * `#`, or quotes are double-quoted (with embedded quotes/backslashes
 * escaped) so they round-trip through `parseDotenv`.
 */
export function serializeDotenv(vars: EnvVar[]): string {
  const lines = vars.map(({ key, value }) => `${key}=${quoteIfNeeded(value)}`);
  return lines.length > 0 ? `${lines.join("\n")}\n` : "";
}

function quoteIfNeeded(value: string): string {
  if (value === "") return "";
  if (/[\s#"'\\]/.test(value)) {
    const escaped = value.replace(/\\/g, "\\\\").replace(/"/g, '\\"');
    return `"${escaped}"`;
  }
  return value;
}

// ─── logs: tail `.zeroship/dev.log` ──────────────────────────────
//
// The preview-start command (sandbox-backend.ts) redirects the
// dev-server stdout+stderr to `.zeroship/dev.log`. The LogsCanvas tails
// it via this proc (it polls ~2s). Returns the lines; an absent log
// (preview never started) reads back as empty, not an error.

const DEV_LOG_PATH = ".zeroship/dev.log";

export const getLogs = action(async (appId: string): Promise<string[]> => {
  const text = await readSandboxFileFor(appId, DEV_LOG_PATH);
  if (!text) return [];
  // Drop the trailing empty element from a final newline so the canvas
  // doesn't render a blank last row.
  const lines = text.split(/\r?\n/);
  if (lines.length > 0 && lines[lines.length - 1] === "") lines.pop();
  return lines;
}, { id: "projects.getLogs" });

// `appPreviewUrl` lives in `src/client/lib/preview-url.ts` — it's a pure
// URL-builder the iframe consumes synchronously, so it must not live in
// a "use server" module (the vite-plugin would otherwise turn it into an
// async RPC stub and the iframe src would receive a stringified
// Promise).
