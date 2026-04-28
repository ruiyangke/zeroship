"use server";
// Apps server functions — proxies to control plane's /api/apps/*.
//
// Cookie session covers admin auth (control plane's `check_admin_auth`
// accepts session cookie OR Bearer master-key); we forward both.

import { CONTROL_URL, CONTROL_KEY } from "./env";
import { getRequest } from "./request-context";

export interface AppRecord {
  id: string;
  name: string;
  plan_id: string;
  deploy_hash: string | null;
  api_key: string;
  created_at: string;
  updated_at: string;
  server_js?: string;
}

async function proxy<T>(
  path: string,
  init: { method?: string; body?: BodyInit; contentType?: string } = {},
): Promise<T> {
  const cookie = getRequest()?.headers.get("cookie") ?? "";
  const headers: Record<string, string> = {
    "content-type": init.contentType ?? "application/json",
    "authorization": `Bearer ${CONTROL_KEY()}`,
  };
  if (cookie) headers["cookie"] = cookie;

  const res = await fetch(`${CONTROL_URL()}${path}`, {
    method: init.method ?? "GET",
    headers,
    body: init.body,
  });

  if (!res.ok) {
    const text = await res.text();
    throw new Error(text || `HTTP ${res.status}`);
  }
  if (res.status === 204) return undefined as T;
  const ct = res.headers.get("content-type") ?? "";
  if (!ct.includes("json")) return (await res.text()) as unknown as T;
  return (await res.json()) as T;
}

export async function listApps(): Promise<AppRecord[]> {
  return proxy("/api/apps");
}

export async function getApp(id: string): Promise<AppRecord> {
  return proxy(`/api/apps/${encodeURIComponent(id)}`);
}

export async function createApp(name: string, plan_id: string = "free"): Promise<AppRecord> {
  return proxy("/api/apps", {
    method: "POST",
    body: JSON.stringify({ name, plan_id }),
  });
}

export async function deleteApp(id: string): Promise<{ deleted: boolean }> {
  return proxy(`/api/apps/${encodeURIComponent(id)}`, { method: "DELETE" });
}

export async function deployApp(id: string, code: string): Promise<{ deploy_hash: string }> {
  return proxy(`/api/apps/${encodeURIComponent(id)}/deploy`, {
    method: "POST",
    body: code,
    contentType: "application/javascript",
  });
}

export async function updatePlan(id: string, plan_id: string): Promise<{ updated: boolean }> {
  return proxy(`/api/apps/${encodeURIComponent(id)}/plan`, {
    method: "PUT",
    body: JSON.stringify({ plan_id }),
  });
}

export async function getAppLogs(id: string): Promise<string[]> {
  return proxy(`/api/apps/${encodeURIComponent(id)}/logs`);
}

// ─── env vars + secrets ─────────────────────────────────────────

export interface EnvVar { key: string; value: string }

export async function listVars(id: string): Promise<{ vars: EnvVar[] }> {
  return proxy(`/api/apps/${encodeURIComponent(id)}/vars`);
}
export async function setVar(id: string, key: string, value: string): Promise<void> {
  await proxy(`/api/apps/${encodeURIComponent(id)}/vars`, {
    method: "POST",
    body: JSON.stringify({ key, value }),
  });
}
export async function deleteVar(id: string, key: string): Promise<void> {
  await proxy(`/api/apps/${encodeURIComponent(id)}/vars/${encodeURIComponent(key)}`, {
    method: "DELETE",
  });
}

export async function listSecrets(id: string): Promise<{ secrets: string[] }> {
  return proxy(`/api/apps/${encodeURIComponent(id)}/secrets`);
}
export async function setSecret(id: string, key: string, value: string): Promise<void> {
  await proxy(`/api/apps/${encodeURIComponent(id)}/secrets`, {
    method: "POST",
    body: JSON.stringify({ key, value }),
  });
}
export async function deleteSecret(id: string, key: string): Promise<void> {
  await proxy(`/api/apps/${encodeURIComponent(id)}/secrets/${encodeURIComponent(key)}`, {
    method: "DELETE",
  });
}

/** URL where a deployed app is reachable (path-based). */
export function appPreviewUrl(appName: string, path: string = "/"): string {
  const base = `/apps/${encodeURIComponent(appName)}`;
  return path === "/" ? `${base}/` : `${base}${path.startsWith("/") ? "" : "/"}${path}`;
}
