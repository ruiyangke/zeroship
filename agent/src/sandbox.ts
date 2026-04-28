/**
 * Thin client for `zeroship-sandbox`. Tools call into here so they
 * don't each duplicate the URL / auth / error wrapping.
 */
import { SANDBOX_URL, SANDBOX_TOKEN } from "./env.js";

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

export interface ExecResult {
  status: number;
  stdout: string;
  stderr: string;
}

function authHeaders(extra: Record<string, string> = {}): Record<string, string> {
  const h: Record<string, string> = { ...extra };
  if (SANDBOX_TOKEN) h.Authorization = `Bearer ${SANDBOX_TOKEN}`;
  return h;
}

async function jsonOrThrow(res: Response, op: string): Promise<any> {
  if (res.ok) return res.json();
  const body = await res.text();
  throw new Error(`sandbox ${op} → ${res.status}: ${body}`);
}

export async function createOrAttachSession(projectId: string): Promise<SessionInfo> {
  const res = await fetch(`${SANDBOX_URL}/sessions`, {
    method: "POST",
    headers: authHeaders({ "Content-Type": "application/json" }),
    body: JSON.stringify({ project_id: projectId }),
  });
  return jsonOrThrow(res, "create session");
}

export async function getSession(sessionId: string): Promise<SessionInfo> {
  const res = await fetch(`${SANDBOX_URL}/sessions/${sessionId}`, {
    headers: authHeaders(),
  });
  return jsonOrThrow(res, "get session");
}

export async function stopSession(sessionId: string): Promise<void> {
  const res = await fetch(`${SANDBOX_URL}/sessions/${sessionId}`, {
    method: "DELETE",
    headers: authHeaders(),
  });
  await jsonOrThrow(res, "stop session");
}

export async function execCommand(
  sessionId: string,
  cmd: string,
  opts: { cwd?: string; timeoutMs?: number } = {},
): Promise<ExecResult> {
  const res = await fetch(`${SANDBOX_URL}/sessions/${sessionId}/exec`, {
    method: "POST",
    headers: authHeaders({ "Content-Type": "application/json" }),
    body: JSON.stringify({
      cmd,
      cwd: opts.cwd,
      timeout_ms: opts.timeoutMs,
    }),
  });
  return jsonOrThrow(res, "exec");
}

export interface FileEntry { path: string; kind: "file" | "dir"; size: number }

export async function listFiles(sessionId: string): Promise<FileEntry[]> {
  const res = await fetch(`${SANDBOX_URL}/sessions/${sessionId}/file-tree`, {
    headers: authHeaders(),
  });
  const data = await jsonOrThrow(res, "file-tree");
  return data.entries ?? [];
}

export async function readFile(sessionId: string, path: string): Promise<string> {
  const res = await fetch(`${SANDBOX_URL}/sessions/${sessionId}/files/${encodePath(path)}`, {
    headers: authHeaders(),
  });
  if (res.status === 404) throw new Error(`file not found: ${path}`);
  if (!res.ok) {
    const body = await res.text();
    throw new Error(`sandbox read ${path} → ${res.status}: ${body}`);
  }
  return res.text();
}

export async function writeFile(sessionId: string, path: string, content: string): Promise<{ size: number }> {
  const res = await fetch(`${SANDBOX_URL}/sessions/${sessionId}/files/${encodePath(path)}`, {
    method: "PUT",
    headers: authHeaders({ "Content-Type": "application/octet-stream" }),
    body: content,
  });
  return jsonOrThrow(res, "write file");
}

export async function deleteFile(sessionId: string, path: string): Promise<void> {
  const res = await fetch(`${SANDBOX_URL}/sessions/${sessionId}/files/${encodePath(path)}`, {
    method: "DELETE",
    headers: authHeaders(),
  });
  if (res.status === 404) return; // idempotent
  await jsonOrThrow(res, "delete file");
}

/**
 * Encode each path segment but keep `/` separators intact (URL specs
 * say slashes inside an opaque path segment must be escaped, but our
 * route uses `{path:.*}` which expects raw slashes).
 */
function encodePath(p: string): string {
  return p.split("/").map((seg) => encodeURIComponent(seg)).join("/");
}
