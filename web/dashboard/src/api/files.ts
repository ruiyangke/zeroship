// ─── Sandbox file API (proxied through agent) ───────────────────
//
// Browser → vite /agent/* → agent → zeroship-sandbox. The agent
// holds the sandbox token; the browser doesn't need it.

export interface FileEntry { path: string; kind: "file" | "dir"; size: number }

const AGENT = "/agent";

export async function listProjectFiles(projectId: string): Promise<FileEntry[]> {
  const res = await fetch(`${AGENT}/projects/${encodeURIComponent(projectId)}/files`);
  if (!res.ok) throw new Error(`list files → ${res.status}: ${await res.text()}`);
  const data = await res.json();
  return data.entries ?? [];
}

export async function readProjectFile(projectId: string, path: string): Promise<string> {
  const url = `${AGENT}/projects/${encodeURIComponent(projectId)}/files/${encodePath(path)}`;
  const res = await fetch(url);
  if (!res.ok) throw new Error(`read ${path} → ${res.status}: ${await res.text()}`);
  return res.text();
}

export async function writeProjectFile(
  projectId: string,
  path: string,
  content: string,
): Promise<{ size: number }> {
  const url = `${AGENT}/projects/${encodeURIComponent(projectId)}/files/${encodePath(path)}`;
  const res = await fetch(url, {
    method: "PUT",
    headers: { "Content-Type": "text/plain" },
    body: content,
  });
  if (!res.ok) throw new Error(`write ${path} → ${res.status}: ${await res.text()}`);
  return res.json();
}

export async function deleteProjectFile(projectId: string, path: string): Promise<void> {
  const url = `${AGENT}/projects/${encodeURIComponent(projectId)}/files/${encodePath(path)}`;
  const res = await fetch(url, { method: "DELETE" });
  if (!res.ok && res.status !== 404) {
    throw new Error(`delete ${path} → ${res.status}: ${await res.text()}`);
  }
}

/** Per-segment encode but keep `/` raw — the agent route is
 *  `:project/files/*` which expects raw slashes in the trailing path. */
function encodePath(p: string): string {
  return p.split("/").map(encodeURIComponent).join("/");
}

/** Pick a CodeMirror language by file extension. */
export function languageFor(path: string): "javascript" | "html" | "css" | "json" | "plain" {
  const ext = path.split(".").pop()?.toLowerCase() ?? "";
  if (["ts", "tsx", "js", "jsx", "mjs", "cjs"].includes(ext)) return "javascript";
  if (ext === "html") return "html";
  if (ext === "css") return "css";
  if (ext === "json") return "json";
  return "plain";
}
