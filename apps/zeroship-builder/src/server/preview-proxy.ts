"use server";

import { CONTROL_URL, SANDBOX_TOKEN, SANDBOX_URL } from "./internal/env";
import { getOrCreateSandboxFor } from "./internal/sandbox-backend";

const AUTH_PREFIX = "/auth/";
const PREVIEW_PREFIX = "/api/preview/";
const HOP_BY_HOP = new Set([
  "connection",
  "keep-alive",
  "proxy-authenticate",
  "proxy-authorization",
  "te",
  "trailer",
  "trailers",
  "transfer-encoding",
  "upgrade",
]);

interface PreviewPath {
  appId: string;
  port: number;
  subPath: string;
}

export async function previewFetch(request: Request): Promise<Response> {
  const url = new URL(request.url);
  if (url.pathname === "/auth" || url.pathname.startsWith(AUTH_PREFIX)) {
    return proxyAuth(request, url);
  }

  const parsed = parsePreviewPath(url.pathname);
  if (!parsed) {
    return new Response("Not Found", { status: 404 });
  }

  try {
    const sandbox = await getOrCreateSandboxFor(parsed.appId, {
      projectSourceId: parsed.appId,
    });
    const controllerUrl = buildControllerPreviewUrl({
      sandboxId: sandbox.id,
      userId: sandbox.userId,
      port: parsed.port,
      subPath: parsed.subPath,
      sourceQuery: url.searchParams,
    });

    const controllerRes = await fetch(controllerUrl, {
      method: request.method,
      headers: proxyRequestHeaders(request),
      body: request.method === "GET" || request.method === "HEAD"
        ? undefined
        : await request.arrayBuffer(),
      redirect: "manual",
    });

    return new Response(controllerRes.body, {
      status: controllerRes.status,
      statusText: controllerRes.statusText,
      headers: proxyResponseHeaders(controllerRes.headers, parsed),
    });
  } catch (err) {
    const message = err instanceof Error ? err.message : String(err);
    return Response.json(
      { error: "preview_proxy_failed", message },
      { status: 502 },
    );
  }
}

async function proxyAuth(request: Request, url: URL): Promise<Response> {
  const target = new URL(`${url.pathname}${url.search}`, CONTROL_URL());
  const controlRes = await fetch(target, {
    method: request.method,
    headers: proxyAuthRequestHeaders(request),
    body: request.method === "GET" || request.method === "HEAD"
      ? undefined
      : await request.arrayBuffer(),
    redirect: "manual",
  });

  return new Response(controlRes.body, {
    status: controlRes.status,
    statusText: controlRes.statusText,
    headers: proxyGenericResponseHeaders(controlRes.headers),
  });
}

function parsePreviewPath(pathname: string): PreviewPath | null {
  if (!pathname.startsWith(PREVIEW_PREFIX)) return null;
  const rest = pathname.slice(PREVIEW_PREFIX.length);
  const firstSlash = rest.indexOf("/");
  if (firstSlash <= 0) return null;
  const appIdSegment = rest.slice(0, firstSlash);
  const afterApp = rest.slice(firstSlash + 1);
  const secondSlash = afterApp.indexOf("/");
  const portSegment = secondSlash === -1 ? afterApp : afterApp.slice(0, secondSlash);
  const subPath = secondSlash === -1 ? "" : afterApp.slice(secondSlash + 1);
  const port = Number(portSegment);
  if (!Number.isInteger(port) || port < 1024 || port > 65535) return null;

  try {
    return {
      appId: decodeURIComponent(appIdSegment),
      port,
      subPath,
    };
  } catch {
    return null;
  }
}

function buildControllerPreviewUrl(args: {
  sandboxId: string;
  userId: string;
  port: number;
  subPath: string;
  sourceQuery: URLSearchParams;
}): string {
  const base = SANDBOX_URL().replace(/\/+$/, "");
  const target = new URL(
    `${base}/sandboxes/${encodeURIComponent(args.sandboxId)}/preview/${args.port}/${args.subPath}`,
  );
  for (const [key, value] of args.sourceQuery) {
    if (key !== "user_id") target.searchParams.append(key, value);
  }
  target.searchParams.set("user_id", args.userId);
  return target.toString();
}

function proxyRequestHeaders(request: Request): Headers {
  const out = new Headers();
  for (const [key, value] of request.headers) {
    const lower = key.toLowerCase();
    if (HOP_BY_HOP.has(lower)) continue;
    if (
      lower === "authorization" ||
      lower === "content-length" ||
      lower === "cookie" ||
      lower === "host" ||
      lower === "x-forwarded-for" ||
      lower === "x-forwarded-host" ||
      lower === "x-forwarded-proto" ||
      lower === "x-zspreview-host" ||
      lower.startsWith("x-sbx-")
    ) {
      continue;
    }
    out.set(key, value);
  }

  const token = SANDBOX_TOKEN();
  if (!token) {
    throw new Error("SANDBOX_TOKEN is not set");
  }
  out.set("authorization", `Bearer ${token}`);
  return out;
}

function proxyAuthRequestHeaders(request: Request): Headers {
  const out = new Headers();
  for (const [key, value] of request.headers) {
    const lower = key.toLowerCase();
    if (HOP_BY_HOP.has(lower)) continue;
    if (
      lower === "content-length" ||
      lower === "host" ||
      lower === "x-forwarded-for" ||
      lower === "x-forwarded-host" ||
      lower === "x-forwarded-proto"
    ) {
      continue;
    }
    out.set(key, value);
  }
  return out;
}

function proxyGenericResponseHeaders(headers: Headers): Headers {
  const out = new Headers();
  const setCookies = readSetCookies(headers);
  headers.forEach((value, key) => {
    const lower = key.toLowerCase();
    if (HOP_BY_HOP.has(lower) || lower === "content-length") return;
    if (lower === "set-cookie" && setCookies.length > 0) return;
    out.append(key, value);
  });
  for (const cookie of setCookies) {
    out.append("set-cookie", cookie);
  }
  out.set("cache-control", "no-store");
  return out;
}

function readSetCookies(headers: Headers): string[] {
  const api = headers as Headers & { getSetCookie?: () => string[] };
  return api.getSetCookie?.() ?? [];
}

function proxyResponseHeaders(headers: Headers, parsed: PreviewPath): Headers {
  const out = new Headers();
  headers.forEach((value, key) => {
    const lower = key.toLowerCase();
    if (HOP_BY_HOP.has(lower) || lower === "content-length") return;
    if (lower === "location") {
      out.append(key, rewritePreviewUrl(value, parsed));
      return;
    }
    if (lower === "refresh") {
      out.append(key, rewriteRefresh(value, parsed));
      return;
    }
    out.append(key, value);
  });
  out.set("cache-control", "no-store");
  return out;
}

function proxyBase(parsed: PreviewPath): string {
  return `/api/preview/${encodeURIComponent(parsed.appId)}/${parsed.port}`;
}

function rewritePreviewUrl(value: string, parsed: PreviewPath): string {
  if (value.startsWith("/")) {
    return `${proxyBase(parsed)}${value}`;
  }

  try {
    const url = new URL(value);
    if (isPreviewHost(url.hostname) || isLoopbackHost(url.hostname)) {
      return `${proxyBase(parsed)}${url.pathname}${url.search}${url.hash}`;
    }
  } catch {
    // Relative redirects such as "login" already resolve under the
    // current /api/preview/... path, so leave them untouched.
  }
  return value;
}

function rewriteRefresh(value: string, parsed: PreviewPath): string {
  return value
    .split(";")
    .map((part) => {
      const trimmed = part.trimStart();
      if (!trimmed.toLowerCase().startsWith("url=")) return part;
      const leading = part.slice(0, part.length - trimmed.length);
      return `${leading}url=${rewritePreviewUrl(trimmed.slice(4), parsed)}`;
    })
    .join(";");
}

function isPreviewHost(hostname: string): boolean {
  return hostname === "preview.zeroship.dev" || hostname.endsWith(".preview.zeroship.dev");
}

function isLoopbackHost(hostname: string): boolean {
  const lower = hostname.toLowerCase();
  return lower === "localhost" ||
    lower === "127.0.0.1" ||
    lower === "0.0.0.0" ||
    lower === "::1" ||
    lower === "[::1]";
}
