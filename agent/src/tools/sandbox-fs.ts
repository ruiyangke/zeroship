/**
 * Sandbox file-system tools — the agent's primary surface for
 * working with a project. All operations target the editor session's
 * Docker container via the `zeroship-sandbox` HTTP API.
 *
 * The tools take a `session_id` (returned by `open_session`) so the
 * agent doesn't have to know about the underlying container, IP, or
 * workspace path.
 */
import { tool } from "@langchain/core/tools";
import { z } from "zod";
import {
  createOrAttachSession,
  execCommand,
  listFiles,
  readFile,
  writeFile,
  deleteFile,
} from "../sandbox.js";

export const openSession = tool(
  async ({ project_id }) => {
    const info = await createOrAttachSession(project_id);
    return JSON.stringify({
      ok: true,
      session_id: info.session_id,
      project_id: info.project_id,
      container_ip: info.container_ip,
      workspace_path: info.workspace_path,
      message:
        "Session ready. Use this session_id for file ops + run_command. The workspace is pre-seeded with a Vite + React + Tailwind starter.",
    });
  },
  {
    name: "open_session",
    description:
      "Open (or re-attach to) a sandbox session for a project. Idempotent — calling twice with the same project_id returns the same session. Returns a session_id you must pass to every other sandbox tool.",
    schema: z.object({
      project_id: z
        .string()
        .min(1)
        .max(64)
        .regex(/^[a-zA-Z0-9_-]+$/, "alphanumeric, dash, underscore only")
        .describe("Stable per-project identifier (typically the app's UUID)."),
    }),
  },
);

export const sandboxListFiles = tool(
  async ({ session_id }) => {
    const entries = await listFiles(session_id);
    if (entries.length === 0) {
      return JSON.stringify({ ok: true, entries: [], message: "Workspace is empty." });
    }
    return JSON.stringify({
      ok: true,
      entries: entries.map((e) => `${e.kind === "dir" ? "d" : "-"} ${e.path}${e.kind === "file" ? ` (${e.size}b)` : ""}`),
    });
  },
  {
    name: "list_files",
    description:
      "Walk the project workspace and return every file/dir (skipping node_modules, .git, dist).",
    schema: z.object({
      session_id: z.string().uuid(),
    }),
  },
);

export const sandboxReadFile = tool(
  async ({ session_id, path }) => {
    try {
      const content = await readFile(session_id, path);
      return JSON.stringify({ ok: true, path, content });
    } catch (e: any) {
      return JSON.stringify({ ok: false, error: e?.message ?? String(e) });
    }
  },
  {
    name: "read_file",
    description: "Read a file from the project workspace. Returns its full content as a string.",
    schema: z.object({
      session_id: z.string().uuid(),
      path: z.string().min(1).describe("Path relative to the workspace root, e.g. 'src/App.tsx'."),
    }),
  },
);

export const sandboxWriteFile = tool(
  async ({ session_id, path, content }) => {
    try {
      const r = await writeFile(session_id, path, content);
      return JSON.stringify({ ok: true, path, size: r.size });
    } catch (e: any) {
      return JSON.stringify({ ok: false, error: e?.message ?? String(e) });
    }
  },
  {
    name: "write_file",
    description:
      "Create or overwrite a file in the project workspace. Parent directories are created automatically. 5 MB cap.",
    schema: z.object({
      session_id: z.string().uuid(),
      path: z.string().min(1).describe("Workspace-relative path."),
      content: z.string().describe("The full new file content."),
    }),
  },
);

export const sandboxDeleteFile = tool(
  async ({ session_id, path }) => {
    try {
      await deleteFile(session_id, path);
      return JSON.stringify({ ok: true, deleted: path });
    } catch (e: any) {
      return JSON.stringify({ ok: false, error: e?.message ?? String(e) });
    }
  },
  {
    name: "delete_file",
    description: "Delete a file from the project workspace. Idempotent.",
    schema: z.object({
      session_id: z.string().uuid(),
      path: z.string().min(1),
    }),
  },
);

export const runCommand = tool(
  async ({ session_id, cmd, cwd, timeout_ms }) => {
    try {
      const out = await execCommand(session_id, cmd, { cwd, timeoutMs: timeout_ms });
      return JSON.stringify({
        ok: out.status === 0,
        status: out.status,
        stdout: out.stdout.length > 4000 ? out.stdout.slice(0, 4000) + "\n…(truncated)" : out.stdout,
        stderr: out.stderr.length > 4000 ? out.stderr.slice(0, 4000) + "\n…(truncated)" : out.stderr,
      });
    } catch (e: any) {
      return JSON.stringify({ ok: false, error: e?.message ?? String(e) });
    }
  },
  {
    name: "run_command",
    description:
      "Run a shell command inside the project's sandbox container. Useful for `npm install`, `npm run build`, `git commit -m ...`, `git log`, etc. Default cwd is /workspace; default timeout 60s; max 600s. Output truncated at 4 KB per stream.",
    schema: z.object({
      session_id: z.string().uuid(),
      cmd: z.string().min(1).describe("Shell command line. Runs under `sh -c`, so pipes / && / >> all work."),
      cwd: z.string().optional().describe("Override working directory (default /workspace)."),
      timeout_ms: z.number().int().positive().max(600_000).optional()
        .describe("Per-command timeout in milliseconds (default 60000, max 600000)."),
    }),
  },
);
