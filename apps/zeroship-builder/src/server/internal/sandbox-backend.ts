"use server";
// deepagents `SandboxBackendProtocolV2` adapter targeting the zeroship
// sandbox controller (`crates/sandbox`). Builder's built-in
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
import { currentUser } from "zeroship";
import {
  isTypedId,
  retagTypedId,
  typedIdFromStableSeed,
  typedIdFromUuid,
} from "@zeroship/server/typed-id";

import { SANDBOX_URL, SANDBOX_TOKEN, ZEROSHIP_SDK_REGISTRY } from "./env.js";
import { publicErrorWithRequestId, UpstreamServiceError } from "./upstream-error.js";

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

export const DEFAULT_PREVIEW_PORT = 5173;

export interface SandboxHandle {
  id: string;
  userId: string;
  projectId: string;
}

export interface SandboxLookupOptions {
  /**
   * Stable project/app/thread seed. The workspace passes appId; app-less
   * dev/test chats fall back to threadId.
   */
  projectSourceId?: string;
  /**
   * Test/pre-authenticated escape hatch. Production callers leave this
   * empty so the current request's authenticated user is resolved below.
   */
  userId?: string;
}

export interface SandboxExecuteOptions {
  cwd?: string;
  timeoutMs?: number;
}

// ─── module-local cache: user_id + project_id → sandbox_id ─────────────
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

const NPMRC_PATH = ".npmrc";
const ZEROSHIP_SCOPE_REGISTRY_RE = /^\s*@zeroship:registry\s*=/i;

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

const DEV_SANDBOX_USER_UUID = "00000000-0000-7000-8000-000000000001";
export const DEV_SANDBOX_USER_ID = typedIdFromUuid("usr", DEV_SANDBOX_USER_UUID);

function isLocalDevRuntime(): boolean {
  const proc = (globalThis as { process?: { env?: Record<string, string | undefined> } }).process;
  const env = proc?.env;
  return env?.NODE_ENV !== "production" && env?.ZEROSHIP_BUILDER_DISABLE_DEV_USER !== "1";
}

function readCurrentUserId(): string | null {
  try {
    const user = currentUser() as { id?: unknown } | null;
    return typeof user?.id === "string" ? user.id : null;
  } catch {
    return null;
  }
}

function assertTypedUserId(id: string): string {
  if (!isTypedId(id, "usr")) {
    throw new Error(`sandbox user_id must be a usr_ typed-id; got ${JSON.stringify(id)}`);
  }
  return id;
}

function typedUserIdOrNull(id: string): string | null {
  if (isTypedId(id, "usr")) return id;
  if (isLocalDevRuntime()) return null;
  return assertTypedUserId(id);
}

// Identity comes from the PLATFORM session: the gateway forwards the
// verified `ZeroShip-User` envelope and the worker exposes it to app code
// as `currentUser()` (read here via `readCurrentUserId()`). There is no
// bespoke RP fallback — that surface was removed when the console became a
// regular app on the standard runtime.
function resolveSandboxUserId(explicit?: string): string {
  if (explicit) return assertTypedUserId(explicit);

  const platformUserId = readCurrentUserId();
  if (platformUserId) {
    const typed = typedUserIdOrNull(platformUserId);
    if (typed) return typed;
  }

  if (isLocalDevRuntime()) {
    return DEV_SANDBOX_USER_ID;
  }

  throw new Error("sandbox user_id unavailable: request is not authenticated");
}

export function deriveSandboxProjectId(sourceId: string): string {
  const source = sourceId.trim();
  if (!source) {
    throw new Error("sandbox project id source is empty");
  }

  try {
    return retagTypedId(source, "prj");
  } catch {
    // Not a typed-id; try the control-plane UUID shape used by today's
    // AppRecord serialization.
  }

  try {
    return typedIdFromUuid("prj", source);
  } catch {
    // App-less dev/test threads may be opaque AI SDK ids. Hash the seed
    // into a UUID-shaped value so the controller still gets a stable
    // typed-id and can dedup across process restarts.
  }

  return typedIdFromStableSeed("prj", `zeroship-builder:${source}`);
}

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
    throw new UpstreamServiceError({
      service: "sandbox",
      operation: "create",
      status: res.status,
      body,
      publicMessage: "sandbox unavailable",
    });
  }
  return (await res.json()) as SandboxInfo;
}

/**
 * Look up (or create) the sandbox for this Builder thread. Cached
 * process-locally so a long chat doesn't pay the controller round-trip
 * on every turn. Idempotent at the wire (the controller dedups on
 * (user_id, project_id)).
 */
export async function getOrCreateSandboxFor(
  threadId: string,
  opts: SandboxLookupOptions = {},
): Promise<SandboxHandle> {
  const userId = resolveSandboxUserId(opts.userId);
  const projectId = deriveSandboxProjectId(opts.projectSourceId ?? threadId);
  const cacheKey = `${userId}:${projectId}`;

  const cached = _sandboxCache.get(cacheKey);
  if (cached) {
    const handle = { id: cached, userId, projectId };
    await ensureSandboxSdkRegistryNpmrc(handle);
    return handle;
  }
  const pending = _inFlight.get(cacheKey);
  if (pending) {
    const handle = { id: await pending, userId, projectId };
    await ensureSandboxSdkRegistryNpmrc(handle);
    return handle;
  }

  const promise = controllerCreateSandbox(userId, projectId)
    .then((info) => {
      _sandboxCache.set(cacheKey, info.sandbox_id);
      return info.sandbox_id;
    })
    .finally(() => {
      _inFlight.delete(cacheKey);
    });
  _inFlight.set(cacheKey, promise);
  const handle = { id: await promise, userId, projectId };
  await ensureSandboxSdkRegistryNpmrc(handle);
  return handle;
}

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
  private readonly userId: string;

  constructor(opts: { id: string; userId: string }) {
    super();
    this.id = opts.id;
    this.userId = assertTypedUserId(opts.userId);
  }

  // POST /sandboxes/:id/exec
  async execute(command: string, opts: SandboxExecuteOptions = {}): Promise<ExecuteResponse> {
    const url = `${controllerBase()}/sandboxes/${this.id}/exec?user_id=${encodeURIComponent(this.userId)}`;
    const res = await fetch(url, {
      method: "POST",
      headers: authHeaders({ "content-type": "application/json" }),
      // The controller accepts `{cmd, cwd?, timeout_ms?}`. Default
      // timeout (60s) is fine for build/test commands; the controller
      // caps it at 600s anyway.
      body: JSON.stringify({
        cmd: command,
        cwd: opts.cwd,
        timeout_ms: opts.timeoutMs,
      }),
    });
    if (!res.ok) {
      const body = await res.text().catch(() => "");
      const err = new UpstreamServiceError({
        service: "sandbox",
        operation: "execute",
        status: res.status,
        body,
        publicMessage: "sandbox command failed",
      });
      // Surface the failure as a non-zero-exit ExecuteResponse rather
      // than throwing — deepagents tools can render the error to the
      // model, whereas an exception would crash the run.
      return {
        output: publicErrorWithRequestId(err.message, err.request_id),
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
    const url = `${controllerBase()}/sandboxes/${this.id}/files/${encodePath(filePath)}?user_id=${encodeURIComponent(this.userId)}`;
    const res = await fetch(url, {
      method: "PUT",
      headers: authHeaders({ "content-type": "text/plain" }),
      body: content,
    });
    if (!res.ok) {
      const body = await res.text().catch(() => "");
      const err = new UpstreamServiceError({
        service: "sandbox",
        operation: "write file",
        status: res.status,
        body,
        publicMessage: "sandbox file write failed",
      });
      return {
        error: publicErrorWithRequestId(err.message, err.request_id),
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
      const url = `${controllerBase()}/sandboxes/${this.id}/files/${encodePath(p)}?user_id=${encodeURIComponent(this.userId)}`;
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
      const url = `${controllerBase()}/sandboxes/${this.id}/files/${encodePath(p)}?user_id=${encodeURIComponent(this.userId)}`;
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

export function sdkRegistryNpmrcLine(registryUrl = ZEROSHIP_SDK_REGISTRY()): string | null {
  const registry = normalizeSdkRegistryUrl(registryUrl);
  return registry ? `@zeroship:registry=${registry}` : null;
}

export function upsertSdkRegistryNpmrc(existing: string, registryUrl: string): string {
  const line = sdkRegistryNpmrcLine(registryUrl);
  if (!line) return existing;

  const kept = existing
    .split(/\r?\n/)
    .filter((candidate) => !ZEROSHIP_SCOPE_REGISTRY_RE.test(candidate));
  while (kept.length > 0 && kept[kept.length - 1] === "") {
    kept.pop();
  }
  kept.push(line);
  return `${kept.join("\n")}\n`;
}

function normalizeSdkRegistryUrl(registryUrl: string): string {
  const trimmed = registryUrl.trim();
  if (!trimmed) return "";
  if (/[\r\n]/.test(trimmed)) {
    throw new Error("ZEROSHIP_SDK_REGISTRY must be a single URL, not a multi-line value");
  }
  return trimmed.replace(/\/+$/, "");
}

async function ensureSandboxSdkRegistryNpmrc(sandbox: SandboxHandle): Promise<void> {
  const registryUrl = ZEROSHIP_SDK_REGISTRY();
  if (!registryUrl.trim()) return;

  const backend = new ZeroshipSandboxBackend({
    id: sandbox.id,
    userId: sandbox.userId,
  });
  let existing = "";
  const [download] = await backend.downloadFiles([NPMRC_PATH]);
  if (download?.content && !download.error) {
    existing = new TextDecoder().decode(download.content);
  } else if (download && download.error !== "file_not_found") {
    throw new Error(`failed to read sandbox ${NPMRC_PATH}: ${download.error}`);
  }

  const next = upsertSdkRegistryNpmrc(existing, registryUrl);
  if (next === existing) return;

  const write = await backend.write(NPMRC_PATH, next);
  if (write.error) {
    throw new Error(`failed to write sandbox ${NPMRC_PATH}: ${write.error}`);
  }
}

export interface PreviewServerResult {
  port: number;
  status: "ready" | "not_ready";
  output: string;
}

function previewServerCommand(port: number): string {
  return String.raw`set -eu
PORT="__PREVIEW_PORT__"
LOG=".zeroship/preview.log"
PID=".zeroship/preview.pid"
mkdir -p .zeroship

check_port() {
  perl -MIO::Socket::INET -e '
my $port = shift;
my $socket = IO::Socket::INET->new(
  PeerAddr => "127.0.0.1",
  PeerPort => $port,
  Proto => "tcp",
  Timeout => 1,
);
exit($socket ? 0 : 1);
' "$PORT" >/dev/null 2>&1
}

if check_port; then
  echo "preview already listening on :$PORT"
  exit 0
fi

if [ -f "$PID" ]; then
  OLD_PID="$(cat "$PID" 2>/dev/null || true)"
  if [ -n "$OLD_PID" ] && kill -0 "$OLD_PID" 2>/dev/null; then
    echo "preview pid $OLD_PID is running but :$PORT is not reachable; starting a fresh server"
  fi
fi

if [ -f package.json ]; then
  if ! command -v npm >/dev/null 2>&1 && ! command -v pnpm >/dev/null 2>&1 && ! command -v yarn >/dev/null 2>&1 && ! command -v bun >/dev/null 2>&1; then
    echo "preview requires a JavaScript package manager, but none is installed in this sandbox"
    exit 3
  fi

  if [ ! -d node_modules ]; then
    if [ -f pnpm-lock.yaml ] && command -v pnpm >/dev/null 2>&1; then
      pnpm install --frozen-lockfile || pnpm install
    elif [ -f yarn.lock ] && command -v yarn >/dev/null 2>&1; then
      yarn install --frozen-lockfile || yarn install
    elif [ -f bun.lockb ] && command -v bun >/dev/null 2>&1; then
      bun install
    else
      npm install
    fi
  fi

  if [ -f pnpm-lock.yaml ] && command -v pnpm >/dev/null 2>&1; then
    CMD="pnpm dev -- --host 0.0.0.0 --port $PORT"
  elif [ -f bun.lockb ] && command -v bun >/dev/null 2>&1; then
    CMD="bun run dev -- --host 0.0.0.0 --port $PORT"
  elif [ -f yarn.lock ] && command -v yarn >/dev/null 2>&1; then
    CMD="yarn dev --host 0.0.0.0 --port $PORT"
  else
    CMD="npm run dev -- --host 0.0.0.0 --port $PORT"
  fi
  nohup sh -lc "$CMD" > "$LOG" 2>&1 < /dev/null &
  echo "$!" > "$PID"
elif [ -f index.html ] && command -v python3 >/dev/null 2>&1; then
  nohup python3 -m http.server "$PORT" --bind 0.0.0.0 > "$LOG" 2>&1 < /dev/null &
  echo "$!" > "$PID"
elif [ -f index.html ] && command -v perl >/dev/null 2>&1; then
  cat > .zeroship/preview-static.pl <<'PERL'
use strict;
use warnings;
use IO::Socket::INET;
my $port = shift @ARGV;
my $server = IO::Socket::INET->new(
  LocalAddr => "0.0.0.0",
  LocalPort => $port,
  Proto => "tcp",
  Listen => 10,
  Reuse => 1,
) or die "listen failed: $!";
while (my $client = $server->accept()) {
  my $line = <$client> // "";
  while (defined(my $h = <$client>)) {
    last if $h =~ /^\r?\n$/;
  }
  open my $fh, "<", "index.html" or do {
    print $client "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n";
    close $client;
    next;
  };
  local $/;
  my $body = <$fh>;
  print $client "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: " . length($body) . "\r\n\r\n" . $body;
  close $client;
}
PERL
  nohup perl .zeroship/preview-static.pl "$PORT" > "$LOG" 2>&1 < /dev/null &
  echo "$!" > "$PID"
else
  echo "preview source not ready: expected package.json or index.html in the sandbox root"
  exit 3
fi

i=0
while [ "$i" -lt 80 ]; do
  if check_port; then
    echo "preview listening on :$PORT"
    exit 0
  fi
  i=$((i + 1))
  sleep 0.25
done

echo "preview did not start on :$PORT"
if [ -f "$LOG" ]; then
  echo "--- .zeroship/preview.log ---"
  tail -80 "$LOG" || true
fi
exit 4
`.replace("__PREVIEW_PORT__", String(port));
}

export async function ensureSandboxPreviewServer(
  sandbox: SandboxHandle,
  port: number = DEFAULT_PREVIEW_PORT,
): Promise<PreviewServerResult> {
  if (!Number.isInteger(port) || port < 1024 || port > 65535) {
    throw new Error(`preview port must be in [1024, 65535]; got ${port}`);
  }

  const backend = new ZeroshipSandboxBackend({
    id: sandbox.id,
    userId: sandbox.userId,
  });
  const result = await backend.execute(previewServerCommand(port), {
    timeoutMs: 600_000,
  });
  const output = result.output;
  const ready = (result.exitCode ?? -1) === 0;
  return {
    port,
    status: ready ? "ready" : "not_ready",
    output,
  };
}
