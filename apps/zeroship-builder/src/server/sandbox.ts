"use server";
// Sandbox server functions — proxy to zeroship-sandbox HTTP API.
// We hold the sandbox token here; the browser never sees it.

import { SANDBOX_URL, SANDBOX_TOKEN } from "./env";

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

export async function openSession(projectId: string): Promise<SessionInfo> {
  const res = await fetch(`${SANDBOX_URL()}/sessions`, {
    method: "POST",
    headers: authHeaders({ "content-type": "application/json" }),
    body: JSON.stringify({ project_id: projectId }),
  });
  return jsonOrThrow(res, "create session");
}

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

export async function readFile(projectId: string, path: string): Promise<string> {
  const sid = await sessionFor(projectId);
  const res = await fetch(
    `${SANDBOX_URL()}/sessions/${sid}/files/${encodePath(path)}`,
    { headers: authHeaders() },
  );
  if (!res.ok) throw new Error(`read ${path} → ${res.status}: ${await res.text()}`);
  return res.text();
}

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

export async function deleteFile(projectId: string, path: string): Promise<void> {
  const sid = await sessionFor(projectId);
  const res = await fetch(
    `${SANDBOX_URL()}/sessions/${sid}/files/${encodePath(path)}`,
    { method: "DELETE", headers: authHeaders() },
  );
  if (res.status === 404) return;
  await jsonOrThrow<unknown>(res, `delete ${path}`);
}

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

function encodePath(p: string): string {
  return p.split("/").map(encodeURIComponent).join("/");
}
