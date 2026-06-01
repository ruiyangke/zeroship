"use server";
// Sandbox server functions — proxy to the zeroship-sandbox controller's
// `/sandboxes/:id/...` HTTP API.
//
// These are the canvas-facing procs (listSandboxFiles, readSandboxFile,
// getLivePreview). They take an object input so the single-input RPC
// wire forwards every field, and reuse `getOrCreateSandboxFor` from
// `internal/sandbox-backend.ts` so the FilesCanvas/Preview attach to the
// same sandbox the Builder writes into — readers see writers' bytes
// immediately.

import { action } from "@zeroship/rpc/server";
import { z } from "zod";
import { SANDBOX_URL, SANDBOX_TOKEN } from "./internal/env";
import {
  DEFAULT_PREVIEW_PORT,
  ensureSandboxPreviewServer,
  getOrCreateSandboxFor,
} from "./internal/sandbox-backend";
import { UpstreamServiceError } from "./internal/upstream-error";

// The controller's `/sandboxes/:id/*` routes verify ownership via a
// `?user_id=<id>` query string and 404 on mismatch. Builder's backend
// always appends it; canvas-facing reads must too.
function ownerQ(userId: string): string {
  return `?user_id=${encodeURIComponent(userId)}`;
}

// ─── shared types / helpers ──────────────────────────────────────

export interface FileEntry { path: string; kind: "file" | "dir"; size: number }

function authHeaders(extra: Record<string, string> = {}): Record<string, string> {
  const h: Record<string, string> = { ...extra };
  const tok = SANDBOX_TOKEN();
  if (tok) h.authorization = `Bearer ${tok}`;
  return h;
}

export async function jsonOrThrow<T>(res: Response, op: string): Promise<T> {
  if (!res.ok) {
    const body = await res.text();
    throw new UpstreamServiceError({
      service: "sandbox",
      operation: op,
      status: res.status,
      body,
      publicMessage: "sandbox request failed",
    });
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

const appIdSchema = z.string().min(1).max(256);
const listSandboxFilesInputSchema = z.object({ appId: appIdSchema }).strict();
const readSandboxFileInputSchema = z.object({
  appId: appIdSchema,
  path: z.string().min(1).max(4_096),
}).strict();
const livePreviewInputSchema = z.object({
  appId: appIdSchema,
  port: z.number().int().min(1).max(65_535).optional(),
}).strict();

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
}, { id: "sandbox.files.list", input: listSandboxFilesInputSchema, maxInputBytes: 4_096 });

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
    throw new UpstreamServiceError({
      service: "sandbox",
      operation: "read file",
      status: res.status,
      body: await res.text(),
      publicMessage: "file read failed",
    });
  }
  return res.text();
}, { id: "sandbox.files.read", input: readSandboxFileInputSchema, maxInputBytes: 8_192 });

// ─── internal file read/write helpers (NOT RPC procs) ───────────
//
// `projects.ts` builds `.env` + `.zeroship/dev.log` on top of the same
// `/sandboxes/:id/files/{path}` GET/PUT the canvas uses. These are plain
// async functions (no `action()` wrapper) so they can be called
// server-to-server without becoming public endpoints. They resolve the
// project's sandbox the same way the canvas procs do, so a write here is
// visible to the agent and the preview immediately.

/**
 * Read a file from the project's sandbox. Returns `null` when the file
 * doesn't exist (404) — callers treat "absent" as empty rather than an
 * error (a fresh project has no `.env` and no `dev.log` yet). Any other
 * non-OK status throws.
 */
export async function readSandboxFileFor(
  appId: string,
  path: string,
): Promise<string | null> {
  const sandbox = await getOrCreateSandboxFor(appId, { projectSourceId: appId });
  const res = await fetch(
    `${SANDBOX_URL()}/sandboxes/${sandbox.id}/files/${encodePath(path)}${ownerQ(sandbox.userId)}`,
    { headers: authHeaders() },
  );
  if (res.status === 404) return null;
  if (!res.ok) {
    throw new UpstreamServiceError({
      service: "sandbox",
      operation: "read file",
      status: res.status,
      body: await res.text(),
      publicMessage: "file read failed",
    });
  }
  return res.text();
}

/**
 * Write a file into the project's sandbox (PUT `/files/{path}`). Creates
 * or overwrites. Throws an `UpstreamServiceError` on a non-OK status.
 */
export async function writeSandboxFileFor(
  appId: string,
  path: string,
  content: string,
): Promise<void> {
  const sandbox = await getOrCreateSandboxFor(appId, { projectSourceId: appId });
  const res = await fetch(
    `${SANDBOX_URL()}/sandboxes/${sandbox.id}/files/${encodePath(path)}${ownerQ(sandbox.userId)}`,
    {
      method: "PUT",
      headers: authHeaders({ "content-type": "text/plain" }),
      body: content,
    },
  );
  if (!res.ok) {
    throw new UpstreamServiceError({
      service: "sandbox",
      operation: "write file",
      status: res.status,
      body: await res.text(),
      publicMessage: "file write failed",
    });
  }
}

export interface LivePreviewInput {
  appId: string;
  port?: number;
}

export interface LivePreviewInfo {
  sandboxId: string;
  port: number;
  url: string;
  status: "ready";
}

export const getLivePreview = action(async (
  input: LivePreviewInput,
): Promise<LivePreviewInfo> => {
  const port = input.port ?? DEFAULT_PREVIEW_PORT;
  const sandbox = await getOrCreateSandboxFor(input.appId, {
    projectSourceId: input.appId,
  });
  const preview = await ensureSandboxPreviewServer(sandbox, port);
  if (preview.status !== "ready") {
    throw new Error(preview.output || `preview server did not start on :${port}`);
  }
  return {
    sandboxId: sandbox.id,
    port,
    url: `/api/preview/${encodeURIComponent(input.appId)}/${port}/`,
    status: "ready",
  };
}, { id: "sandbox.preview.get", input: livePreviewInputSchema, maxInputBytes: 4_096 });
