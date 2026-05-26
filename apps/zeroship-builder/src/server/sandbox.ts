"use server";
// Sandbox server functions — proxy to zeroship-sandbox HTTP API.
//
// Two flavours live in this file:
//
//  1. Legacy `/sessions/...` procedures (openSession, listFiles,
//     readFile, writeFile, deleteFile, execCommand). These targeted
//     an earlier controller revision. They now use the same single-
//     input wire convention as the rest of the builder RPC surface,
//     but the orphan workspace/tabs/FilesTab tree no longer imports
//     them. Once that tree is deleted, these can be dropped too.
//
//  2. New `/sandboxes/:id/...` procedures (listSandboxFiles,
//     readSandboxFile). These are the ones the FilesCanvas calls and
//     they take object input so the RPC wire forwards everything.
//     They reuse `getOrCreateSandboxFor` from `internal/sandbox-backend.ts`
//     so the FilesCanvas attaches to the same sandbox Builder writes
//     into — readers see writers' bytes immediately.

import { action } from "@zeroship/rpc/server";
import { SANDBOX_URL, SANDBOX_TOKEN } from "./internal/env";
import { getOrCreateSandboxFor } from "./internal/sandbox-backend";

// The controller's `/sandboxes/:id/*` routes verify ownership via a
// `?user_id=<id>` query string and 404 on mismatch. Builder's backend
// always appends it; canvas-facing reads must too.
function ownerQ(userId: string): string {
  return `?user_id=${encodeURIComponent(userId)}`;
}

// ─── shared types / helpers ──────────────────────────────────────

export interface SessionInfo {
  session_id: string;
  project_id: string;
  container_id: string;
  container_name: string;
  container_ip: string;
  workspace_path: string;
  created_at_secs: number;
  last_used_at_secs: number;
}

export interface FileEntry { path: string; kind: "file" | "dir"; size: number }

function authHeaders(extra: Record<string, string> = {}): Record<string, string> {
  const h: Record<string, string> = { ...extra };
  const tok = SANDBOX_TOKEN();
  if (tok) h.authorization = `Bearer ${tok}`;
  return h;
}

async function jsonOrThrow<T>(res: Response, op: string): Promise<T> {
  if (!res.ok) {
    const body = await res.text();
    throw new Error(`sandbox ${op} → ${res.status}: ${body}`);
  }
  if (res.status === 204) return undefined as T;
  return res.json();
}

function encodePath(p: string): string {
  const trimmed = p.startsWith("/") ? p.slice(1) : p;
  return trimmed.split("/").map(encodeURIComponent).join("/");
}

// ─── new `/sandboxes/:id/...` procs (canvas-facing) ──────────────
//
// Both take an object input so the single-input RPC wire delivers
// every field intact. `appId` is forwarded straight to
// `getOrCreateSandboxFor` — the same key Builder uses for its own
// sandbox lookup, so reads from the canvas hit the same workspace
// the agent is writing into.

export interface ListSandboxFilesInput { appId: string }

export const listSandboxFiles = action(async (
  input: ListSandboxFilesInput,
): Promise<FileEntry[]> => {
  const sandbox = await getOrCreateSandboxFor(input.appId, {
    projectSourceId: input.appId,
  });
  const res = await fetch(
    `${SANDBOX_URL()}/sandboxes/${sandbox.id}/file-tree${ownerQ(sandbox.userId)}`,
    { headers: authHeaders() },
  );
  const data = await jsonOrThrow<{ entries: FileEntry[] }>(res, "list files");
  return data.entries ?? [];
}, { id: "sandbox.listSandboxFiles" });

export interface ReadSandboxFileInput { appId: string; path: string }

export const readSandboxFile = action(async (
  input: ReadSandboxFileInput,
): Promise<string> => {
  const sandbox = await getOrCreateSandboxFor(input.appId, {
    projectSourceId: input.appId,
  });
  const res = await fetch(
    `${SANDBOX_URL()}/sandboxes/${sandbox.id}/files/${encodePath(input.path)}${ownerQ(sandbox.userId)}`,
    { headers: authHeaders() },
  );
  if (!res.ok) {
    throw new Error(`read ${input.path} → ${res.status}: ${await res.text()}`);
  }
  return res.text();
}, { id: "sandbox.readSandboxFile" });

// ─── legacy `/sessions/...` procs (kept for orphan tree, scheduled
// for deletion alongside workspace/tabs) ────────────────────────

export const openSession = action(async (projectId: string): Promise<SessionInfo> => {
  const res = await fetch(`${SANDBOX_URL()}/sessions`, {
    method: "POST",
    headers: authHeaders({ "content-type": "application/json" }),
    body: JSON.stringify({ project_id: projectId }),
  });
  return jsonOrThrow(res, "create session");
}, { id: "sandbox.openSession" });

async function sessionFor(projectId: string): Promise<string> {
  const s = await openSession(projectId);
  return s.session_id;
}

export const listFiles = action(async (projectId: string): Promise<FileEntry[]> => {
  const sid = await sessionFor(projectId);
  const res = await fetch(`${SANDBOX_URL()}/sessions/${sid}/file-tree`, { headers: authHeaders() });
  const data = await jsonOrThrow<{ entries: FileEntry[] }>(res, "list files");
  return data.entries ?? [];
}, { id: "sandbox.listFiles" });

export const readFile = action(async (
  input: { projectId: string; path: string },
): Promise<string> => {
  const sid = await sessionFor(input.projectId);
  const res = await fetch(
    `${SANDBOX_URL()}/sessions/${sid}/files/${encodePath(input.path)}`,
    { headers: authHeaders() },
  );
  if (!res.ok) throw new Error(`read ${input.path} → ${res.status}: ${await res.text()}`);
  return res.text();
}, { id: "sandbox.readFile" });

export const writeFile = action(async (
  input: { projectId: string; path: string; content: string },
): Promise<{ written: string; size: number }> => {
  const sid = await sessionFor(input.projectId);
  const res = await fetch(
    `${SANDBOX_URL()}/sessions/${sid}/files/${encodePath(input.path)}`,
    {
      method: "PUT",
      headers: authHeaders({ "content-type": "text/plain" }),
      body: input.content,
    },
  );
  return jsonOrThrow(res, `write ${input.path}`);
}, { id: "sandbox.writeFile" });

export const deleteFile = action(async (
  input: { projectId: string; path: string },
): Promise<void> => {
  const sid = await sessionFor(input.projectId);
  const res = await fetch(
    `${SANDBOX_URL()}/sessions/${sid}/files/${encodePath(input.path)}`,
    { method: "DELETE", headers: authHeaders() },
  );
  if (res.status === 404) return;
  await jsonOrThrow<unknown>(res, `delete ${input.path}`);
}, { id: "sandbox.deleteFile" });

export const execCommand = action(async (
  input: { projectId: string; cmd: string; opts?: { cwd?: string; timeoutMs?: number } },
): Promise<{ status: number; stdout: string; stderr: string }> => {
  const sid = await sessionFor(input.projectId);
  const res = await fetch(`${SANDBOX_URL()}/sessions/${sid}/exec`, {
    method: "POST",
    headers: authHeaders({ "content-type": "application/json" }),
    body: JSON.stringify({
      cmd: input.cmd,
      cwd: input.opts?.cwd,
      timeout_ms: input.opts?.timeoutMs,
    }),
  });
  return jsonOrThrow(res, "exec");
}, { id: "sandbox.execCommand" });
