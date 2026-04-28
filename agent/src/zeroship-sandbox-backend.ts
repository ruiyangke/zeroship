/**
 * `ZeroshipSandboxBackend` — a deepagents `BaseSandbox` backed by
 * the zeroship-sandbox HTTP service.
 *
 * Plugging this into `createDeepAgent({ backend })` makes the
 * built-in deepagents tools (`read_file`, `write_file`,
 * `edit_file`, `ls`, `grep`, `glob`, `execute`) operate against a
 * real Docker container per editor session. We get the canonical
 * tool names + descriptions deepagents has invested prompt
 * engineering in, instead of carrying a parallel `sandbox_*`
 * surface we have to maintain ourselves.
 *
 * `BaseSandbox` only requires three abstract methods:
 *   - `execute(cmd)` — run a shell command in the container
 *   - `uploadFiles(files)` — write multiple files
 *   - `downloadFiles(paths)` — read multiple files
 *
 * It supplies default `lsInfo` / `read` / `write` / `edit` /
 * `grepRaw` / `globInfo` implementations on top of those primitives
 * (using POSIX `find`, `cat`, `head`, etc. via execute()), so we
 * don't reimplement them.
 */
import {
  BaseSandbox,
  type ExecuteResponse,
  type FileUploadResponse,
  type FileDownloadResponse,
} from "deepagents";
import {
  createOrAttachSession,
  execCommand,
  readFile,
  writeFile,
  type SessionInfo,
} from "./sandbox.js";

export class ZeroshipSandboxBackend extends BaseSandbox {
  /** Stable id for this sandbox instance — the project_id. */
  readonly id: string;

  /** Per-instance cache so the deepagents helper functions that
   *  poke .id repeatedly don't trigger re-lookups. */
  private session: SessionInfo;

  private constructor(session: SessionInfo) {
    super();
    this.session = session;
    this.id = session.project_id;
  }

  /**
   * Factory. Eagerly opens (or re-attaches to) a sandbox session
   * for `projectId` so the container is ready by the time the
   * agent issues its first tool call.
   */
  static async open(projectId: string): Promise<ZeroshipSandboxBackend> {
    const session = await createOrAttachSession(projectId);
    return new ZeroshipSandboxBackend(session);
  }

  /** Run a shell command inside the container. deepagents' built-in
   *  `execute` tool calls this directly; built-in `read_file` /
   *  `ls` / `grep` use it via the BaseSandbox defaults. */
  async execute(command: string): Promise<ExecuteResponse> {
    const out = await execCommand(this.session.session_id, command, {
      timeoutMs: 60_000,
    });
    // deepagents' contract: combined output (stdout + stderr) in `output`.
    const combined = out.stderr ? `${out.stdout}\n${out.stderr}` : out.stdout;
    const TRUNC_AT = 16_000;
    const truncated = combined.length > TRUNC_AT;
    return {
      output: truncated ? combined.slice(0, TRUNC_AT) + "\n…(output truncated)" : combined,
      exitCode: out.status,
      truncated,
    };
  }

  /** Upload N files. We loop because the underlying HTTP API is
   *  per-file; deepagents' contract allows partial success and we
   *  surface per-file errors in the response. */
  async uploadFiles(files: Array<[string, Uint8Array]>): Promise<FileUploadResponse[]> {
    const out: FileUploadResponse[] = [];
    for (const [p, content] of files) {
      try {
        // The sandbox HTTP API expects path relative to the
        // workspace root; strip a leading `/workspace/` if the
        // caller passed an absolute path (deepagents tools do).
        const rel = stripWorkspacePrefix(p);
        const text = textFromBytes(content);
        await writeFile(this.session.session_id, rel, text);
        out.push({ path: p, error: null });
      } catch (e: any) {
        out.push({
          path: p,
          error: { code: "WRITE_FAILED", message: e?.message ?? String(e) } as any,
        });
      }
    }
    return out;
  }

  /** Download N files. Same per-file loop pattern as upload. */
  async downloadFiles(paths: string[]): Promise<FileDownloadResponse[]> {
    const out: FileDownloadResponse[] = [];
    for (const p of paths) {
      try {
        const rel = stripWorkspacePrefix(p);
        const text = await readFile(this.session.session_id, rel);
        out.push({
          path: p,
          content: new TextEncoder().encode(text),
          error: null,
        });
      } catch (e: any) {
        out.push({
          path: p,
          content: null,
          error: { code: "READ_FAILED", message: e?.message ?? String(e) } as any,
        });
      }
    }
    return out;
  }

  /** Expose the underlying session so callers can pass session_id /
   *  workspace_path to other tools (build_and_publish, etc.). */
  get sessionInfo(): SessionInfo {
    return this.session;
  }
}

/**
 * deepagents passes paths like `/workspace/src/App.tsx` (absolute
 * inside the container). Our HTTP API takes paths relative to the
 * workspace root. Strip a leading `/workspace/` (or `/`) so both
 * sides agree.
 */
function stripWorkspacePrefix(p: string): string {
  if (p.startsWith("/workspace/")) return p.slice("/workspace/".length);
  if (p === "/workspace") return "";
  if (p.startsWith("/")) return p.slice(1);
  return p;
}

/** UTF-8 decode a Uint8Array; never throws. */
function textFromBytes(b: Uint8Array): string {
  try {
    return new TextDecoder("utf-8", { fatal: false }).decode(b);
  } catch {
    // Best-effort: lossy ASCII fallback so binary blobs at least
    // round-trip via the text-shaped writeFile API.
    let out = "";
    for (let i = 0; i < b.length; i++) out += String.fromCharCode(b[i]);
    return out;
  }
}
