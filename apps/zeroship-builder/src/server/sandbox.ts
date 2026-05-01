"use server";
// Sandbox server functions — proxy to zeroship-sandbox HTTP API.
//
// Two flavours live in this file:
//
//  1. Legacy `/sessions/...` procedures (openSession, listFiles,
//     readFile, writeFile, deleteFile, execCommand). These targeted
//     an earlier controller revision. They take positional args and
//     therefore don't survive the single-input RPC wire (the vite-
//     plugin transform forwards `args[0]` only). The orphan
//     workspace/tabs/FilesTab still imports them via api/files.ts;
//     once the canvas migration deletes that tree, these can be
//     dropped too.
//
//  2. New `/sandboxes/:id/...` procedures (listSandboxFiles,
//     readSandboxFile). These are the ones the FilesCanvas calls and
//     they take object input so the RPC wire forwards everything.
//     They reuse `getOrCreateSandboxFor` from `_sandbox_backend.ts`
//     so the FilesCanvas attaches to the same sandbox Builder writes
//     into — readers see writers' bytes immediately.

import { SANDBOX_URL, SANDBOX_TOKEN } from "./env";
import { getOrCreateSandboxFor } from "./_sandbox_backend";

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

export async function listSandboxFiles(
  input: ListSandboxFilesInput,
): Promise<FileEntry[]> {
  const { id } = await getOrCreateSandboxFor(input.appId);
  const res = await fetch(
    `${SANDBOX_URL()}/sandboxes/${id}/file-tree`,
    { headers: authHeaders() },
  );
  const data = await jsonOrThrow<{ entries: FileEntry[] }>(res, "list files");
  return data.entries ?? [];
}
listSandboxFiles.config = { id: "sandbox.listSandboxFiles" };

export interface ReadSandboxFileInput { appId: string; path: string }

export async function readSandboxFile(
  input: ReadSandboxFileInput,
): Promise<string> {
  const { id } = await getOrCreateSandboxFor(input.appId);
  const res = await fetch(
    `${SANDBOX_URL()}/sandboxes/${id}/files/${encodePath(input.path)}`,
    { headers: authHeaders() },
  );
  if (!res.ok) {
    throw new Error(`read ${input.path} → ${res.status}: ${await res.text()}`);
  }
  return res.text();
}
readSandboxFile.config = { id: "sandbox.readSandboxFile" };

// ─── legacy `/sessions/...` procs (kept for orphan tree, scheduled
// for deletion alongside workspace/tabs) ────────────────────────

export async function openSession(projectId: string): Promise<SessionInfo> {
  const res = await fetch(`${SANDBOX_URL()}/sessions`, {
    method: "POST",
    headers: authHeaders({ "content-type": "application/json" }),
    body: JSON.stringify({ project_id: projectId }),
  });
  return jsonOrThrow(res, "create session");
}
openSession.config = { id: "sandbox.openSession" };

async function sessionFor(projectId: string): Promise<string> {
  const s = await openSession(projectId);
  return s.session_id;
}

export async function listFiles(projectId: string): Promise<FileEntry[]> {
  const sid = await sessionFor(projectId);
  const res = await fetch(`${SANDBOX_URL()}/sessions/${sid}/file-tree`, { headers: authHeaders() });
  const data = await jsonOrThrow<{ entries: FileEntry[] }>(res, "list files");
  return data.entries ?? [];
}
listFiles.config = { id: "sandbox.listFiles" };

export async function readFile(projectId: string, path: string): Promise<string> {
  const sid = await sessionFor(projectId);
  const res = await fetch(
    `${SANDBOX_URL()}/sessions/${sid}/files/${encodePath(path)}`,
    { headers: authHeaders() },
  );
  if (!res.ok) throw new Error(`read ${path} → ${res.status}: ${await res.text()}`);
  return res.text();
}
readFile.config = { id: "sandbox.readFile" };

export async function writeFile(
  projectId: string, path: string, content: string,
): Promise<{ written: string; size: number }> {
  const sid = await sessionFor(projectId);
  const res = await fetch(
    `${SANDBOX_URL()}/sessions/${sid}/files/${encodePath(path)}`,
    {
      method: "PUT",
      headers: authHeaders({ "content-type": "text/plain" }),
      body: content,
    },
  );
  return jsonOrThrow(res, `write ${path}`);
}
writeFile.config = { id: "sandbox.writeFile" };

export async function deleteFile(projectId: string, path: string): Promise<void> {
  const sid = await sessionFor(projectId);
  const res = await fetch(
    `${SANDBOX_URL()}/sessions/${sid}/files/${encodePath(path)}`,
    { method: "DELETE", headers: authHeaders() },
  );
  if (res.status === 404) return;
  await jsonOrThrow<unknown>(res, `delete ${path}`);
}
deleteFile.config = { id: "sandbox.deleteFile" };

export async function execCommand(
  projectId: string,
  cmd: string,
  opts: { cwd?: string; timeoutMs?: number } = {},
): Promise<{ status: number; stdout: string; stderr: string }> {
  const sid = await sessionFor(projectId);
  const res = await fetch(`${SANDBOX_URL()}/sessions/${sid}/exec`, {
    method: "POST",
    headers: authHeaders({ "content-type": "application/json" }),
    body: JSON.stringify({ cmd, cwd: opts.cwd, timeout_ms: opts.timeoutMs }),
  });
  return jsonOrThrow(res, "exec");
}
execCommand.config = { id: "sandbox.execCommand" };
