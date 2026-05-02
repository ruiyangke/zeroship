"use server";
// Phase B.1: deepagents `SandboxBackendProtocolV2` adapter targeting the
// zeroship sandbox controller (`crates/sandbox`). Builder's built-in
// fs/exec tools (`ls`, `read_file`, `write_file`, `edit_file`,
// `execute`, `grep`, `glob`) flow through this class and end up as
// HTTP calls to the controller's `/sandboxes/:id/...` endpoints.
//
// Strategy: extend deepagents' `BaseSandbox` so we only have to supply
// `id`, `execute`, `uploadFiles`, `downloadFiles`. The base class
// derives ls/read/readRaw/grep/glob/edit from those primitives via
// pure POSIX shell. We override `write` to hit the controller's
// dedicated `PUT /files/{path}` endpoint (single round-trip; no
// existence-check pre-roundtrip from the base class) — `edit_file`
// remains base-class-driven because download-then-upload matches what
// the controller can do today (no native edit endpoint).
//
// Lifecycle: one sandbox per Builder thread. `getOrCreateSandboxFor`
// is idempotent — the controller's `POST /sandboxes` deduplicates on
// `(user_id, project_id)`, so even if the process-local cache is cold
// (e.g. after a worker restart) we re-attach to the existing sandbox
// rather than spawn a fresh one. The local cache just spares us a
// round-trip on the hot path.

import {
  BaseSandbox,
  type ExecuteResponse,
  type FileDownloadResponse,
  type FileUploadResponse,
  type WriteResult,
} from "deepagents";

import { SANDBOX_URL, SANDBOX_TOKEN } from "./env.js";

// ─── controller wire shapes (mirrors crates/sandbox/src/handlers.rs) ──

interface SandboxInfo {
  sandbox_id: string;
  user_id: string;
  project_id: string;
  backend: string;
  backend_hint: string;
  created_at_secs: number;
  last_used_at_secs: number;
}

interface ExecResponseWire {
  status: number;
  stdout: string;
  stderr: string;
  timed_out?: boolean;
}

// ─── module-local cache: threadId → sandbox_id ────────────────────────
//
// Idempotent on the wire (the controller dedups on (user_id,
// project_id)), but fetching a sandbox via the cache is cheaper than
// an HTTP round-trip on every chat turn. Process-local; vanishes on
// worker restart, at which point the controller's dedup keeps us
// honest.
const _sandboxCache = new Map<string, string>();

// In-flight create de-duplication so concurrent requests for the same
// thread don't race two POST /sandboxes calls. Two simultaneous calls
// would each hit the controller's "find existing" branch and end up
// with the same sandbox_id, but the second one wastes a round-trip.
const _inFlight = new Map<string, Promise<string>>();

// ─── controller HTTP client ──────────────────────────────────────────

function authHeaders(extra: Record<string, string> = {}): Record<string, string> {
  const tok = SANDBOX_TOKEN();
  if (!tok) {
    throw new Error(
      "SANDBOX_TOKEN is not set. The sandbox controller requires bearer-token auth; " +
        "set SANDBOX_TOKEN (≥32 random bytes) in apps/zeroship-builder/.env to match the " +
        "controller's SANDBOX_TOKEN.",
    );
  }
  return {
    ...extra,
    authorization: `Bearer ${tok}`,
  };
}

function controllerBase(): string {
  return SANDBOX_URL();
}

// The controller's id charset is tighter than typical UUIDs — lowercase
// `[a-z0-9-]{1,50}` starting with `[a-z0-9]`. `useChat` thread ids are
// UUIDs (lowercase hex + dashes, 36 chars) and pass cleanly. Anything
// else gets normalized.
function sanitizeId(raw: string, fallback: string): string {
  const lower = raw.toLowerCase();
  // Replace anything outside [a-z0-9-] with '-'.
  let cleaned = "";
  for (const ch of lower) {
    cleaned +=
      (ch >= "a" && ch <= "z") || (ch >= "0" && ch <= "9") || ch === "-"
        ? ch
        : "-";
  }
  // Trim leading dashes (controller requires first char alnum).
  cleaned = cleaned.replace(/^-+/, "");
  // Cap at 50.
  cleaned = cleaned.slice(0, 50);
  if (!cleaned) cleaned = fallback;
  return cleaned;
}

// We don't yet have a real per-creator user_id flowing into Builder —
// auth lives one layer up (see `auth.ts`) but the chat handler doesn't
// currently thread it through. Placeholder until Phase B.3 routes the
// real uid here. Project_id derives from threadId so a user with one
// chat per project still gets one sandbox per chat.
export const BUILDER_USER_ID = "builder";

async function controllerCreateSandbox(
  userId: string,
  projectId: string,
): Promise<SandboxInfo> {
  const res = await fetch(`${controllerBase()}/sandboxes`, {
    method: "POST",
    headers: authHeaders({ "content-type": "application/json" }),
    body: JSON.stringify({ user_id: userId, project_id: projectId }),
  });
  if (!res.ok) {
    const body = await res.text().catch(() => "");
    throw new Error(
      `sandbox create failed (${res.status}): ${body}. ` +
        `Is the sandbox controller running on ${controllerBase()}?`,
    );
  }
  return (await res.json()) as SandboxInfo;
}

/**
 * Look up (or create) the sandbox for this Builder thread. Cached
 * process-locally so a long chat doesn't pay the controller round-trip
 * on every turn. Idempotent at the wire (the controller dedups on
 * (user_id, project_id)).
 */
export async function getOrCreateSandboxFor(threadId: string): Promise<{ id: string }> {
  const cached = _sandboxCache.get(threadId);
  if (cached) return { id: cached };
  const pending = _inFlight.get(threadId);
  if (pending) return { id: await pending };

  const projectId = sanitizeId(`builder-${threadId}`, "builder-default");
  const userId = sanitizeId(BUILDER_USER_ID, "builder");
  const promise = controllerCreateSandbox(userId, projectId)
    .then((info) => {
      _sandboxCache.set(threadId, info.sandbox_id);
      return info.sandbox_id;
    })
    .finally(() => {
      _inFlight.delete(threadId);
    });
  _inFlight.set(threadId, promise);
  return { id: await promise };
}
getOrCreateSandboxFor.config = { id: "_internal.getOrCreateSandboxFor" };

// ─── path encoding ────────────────────────────────────────────────────
//
// The controller mounts `/sandboxes/:id/files/{path}*` as a wildcard
// segment, so slashes in the path stay slashes (no double-encoding) but
// every other byte must be percent-encoded.
function encodePath(p: string): string {
  // Drop any leading slash — the controller's path is relative to the
  // workspace root.
  const trimmed = p.startsWith("/") ? p.slice(1) : p;
  return trimmed.split("/").map(encodeURIComponent).join("/");
}

// ─── SandboxBackendProtocolV2 implementation ──────────────────────────
//
// Extends `BaseSandbox` so the boilerplate (ls/read/readRaw/grep/glob/
// edit) comes for free — we only own the controller-specific calls.
//
// The class is constructed from a sandbox id (returned by
// `getOrCreateSandboxFor`). One instance per Builder turn is cheap;
// constructing many doesn't cost the controller anything (no
// per-construction RPC).
export class ZeroshipSandboxBackend extends BaseSandbox {
  public readonly id: string;

  constructor(opts: { id: string }) {
    super();
    this.id = opts.id;
  }

  // POST /sandboxes/:id/exec
  async execute(command: string): Promise<ExecuteResponse> {
    const url = `${controllerBase()}/sandboxes/${this.id}/exec?user_id=${encodeURIComponent(BUILDER_USER_ID)}`;
    const res = await fetch(url, {
      method: "POST",
      headers: authHeaders({ "content-type": "application/json" }),
      // The controller accepts `{cmd, cwd?, timeout_ms?}`. Default
      // timeout (60s) is fine for build/test commands; the controller
      // caps it at 600s anyway.
      body: JSON.stringify({ cmd: command }),
    });
    if (!res.ok) {
      const body = await res.text().catch(() => "");
      // Surface the failure as a non-zero-exit ExecuteResponse rather
      // than throwing — deepagents tools can render the error to the
      // model, whereas an exception would crash the run.
      return {
        output: `[sandbox controller error ${res.status}] ${body}`,
        exitCode: -1,
        truncated: false,
      };
    }
    const wire = (await res.json()) as ExecResponseWire;
    return {
      // BaseSandbox parses combined stdout+stderr; we follow the same
      // convention so parent-class shell parsers (ls/grep/glob) work.
      output: wire.stdout + (wire.stderr ? wire.stderr : ""),
      exitCode: wire.status,
      truncated: false,
    };
  }

  // PUT /sandboxes/:id/files/{path}
  //
  // We override `write` (instead of letting `BaseSandbox.write` go via
  // `uploadFiles`) for two reasons:
  //   1. Single round-trip — no `downloadFiles` existence pre-check.
  //   2. The base class's "refuse to write existing file" guard isn't
  //      what Builder wants; agents need to overwrite freely. (Edit-vs-
  //      write distinction is enforced by the LLM picking the right
  //      tool, not by us refusing one.)
  async write(filePath: string, content: string): Promise<WriteResult> {
    const url = `${controllerBase()}/sandboxes/${this.id}/files/${encodePath(filePath)}?user_id=${encodeURIComponent(BUILDER_USER_ID)}`;
    const res = await fetch(url, {
      method: "PUT",
      headers: authHeaders({ "content-type": "text/plain" }),
      body: content,
    });
    if (!res.ok) {
      const body = await res.text().catch(() => "");
      return {
        error: `Failed to write to ${filePath} (${res.status}): ${body}`,
      };
    }
    return {
      path: filePath,
      // External backend (the file's already on disk in the sandbox);
      // no LangGraph state slice to update.
      filesUpdate: null,
    };
  }

  // GET /sandboxes/:id/files/{path}
  //
  // The protocol exposes `downloadFiles(paths)`; the controller can
  // only fetch one at a time, so we issue them sequentially. (Parallel
  // would be marginally faster for large batches, but Builder rarely
  // batches reads — and the controller has no per-id mutex penalty.)
  async downloadFiles(paths: string[]): Promise<FileDownloadResponse[]> {
    const out: FileDownloadResponse[] = [];
    for (const p of paths) {
      const url = `${controllerBase()}/sandboxes/${this.id}/files/${encodePath(p)}?user_id=${encodeURIComponent(BUILDER_USER_ID)}`;
      const res = await fetch(url, { headers: authHeaders() });
      if (res.status === 404) {
        out.push({ path: p, content: null, error: "file_not_found" });
        continue;
      }
      if (!res.ok) {
        // Map controller errors to deepagents' tight enum. 400 with
        // "is a directory" message is the most common non-404 in
        // practice; otherwise we fall back to `invalid_path`.
        const body = await res.text().catch(() => "");
        const error = /directory/i.test(body) ? "is_directory" : "invalid_path";
        out.push({ path: p, content: null, error });
        continue;
      }
      const buf = await res.arrayBuffer();
      out.push({ path: p, content: new Uint8Array(buf), error: null });
    }
    return out;
  }

  // PUT /sandboxes/:id/files/{path} (per-file)
  //
  // Reused by `BaseSandbox.edit` (download → mutate → upload). The
  // controller has no batch-upload endpoint, so we issue puts
  // sequentially.
  async uploadFiles(
    files: Array<[string, Uint8Array]>,
  ): Promise<FileUploadResponse[]> {
    const out: FileUploadResponse[] = [];
    for (const [p, bytes] of files) {
      const url = `${controllerBase()}/sandboxes/${this.id}/files/${encodePath(p)}?user_id=${encodeURIComponent(BUILDER_USER_ID)}`;
      // Wrap the Uint8Array as a Blob — the Web Fetch typings vary
      // between platforms over whether `BodyInit` accepts a raw
      // typed-array directly. A Blob is universally accepted and
      // adds no measurable overhead for the file sizes Builder writes.
      const res = await fetch(url, {
        method: "PUT",
        headers: authHeaders({ "content-type": "application/octet-stream" }),
        body: new Blob([bytes as BlobPart]),
      });
      if (!res.ok) {
        const body = await res.text().catch(() => "");
        const error = /directory/i.test(body) ? "is_directory" : "invalid_path";
        out.push({ path: p, error });
        continue;
      }
      out.push({ path: p, error: null });
    }
    return out;
  }
}
