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
listApps.config = { id: "apps.listApps" };

export async function getApp(id: string): Promise<AppRecord> {
  return proxy(`/api/apps/${encodeURIComponent(id)}`);
}
getApp.config = { id: "apps.getApp" };

export async function createApp(name: string, plan_id: string = "free"): Promise<AppRecord> {
  return proxy("/api/apps", {
    method: "POST",
    body: JSON.stringify({ name, plan_id }),
  });
}
createApp.config = { id: "apps.createApp" };

export async function deleteApp(id: string): Promise<{ deleted: boolean }> {
  return proxy(`/api/apps/${encodeURIComponent(id)}`, { method: "DELETE" });
}
deleteApp.config = { id: "apps.deleteApp" };

export async function deployApp(id: string, code: string): Promise<{ deploy_hash: string }> {
  return proxy(`/api/apps/${encodeURIComponent(id)}/deploy`, {
    method: "POST",
    body: code,
    contentType: "application/javascript",
  });
}
deployApp.config = { id: "apps.deployApp" };

export async function updatePlan(id: string, plan_id: string): Promise<{ updated: boolean }> {
  return proxy(`/api/apps/${encodeURIComponent(id)}/plan`, {
    method: "PUT",
    body: JSON.stringify({ plan_id }),
  });
}
updatePlan.config = { id: "apps.updatePlan" };

export async function getAppLogs(id: string): Promise<string[]> {
  return proxy(`/api/apps/${encodeURIComponent(id)}/logs`);
}
getAppLogs.config = { id: "apps.getAppLogs" };

// ─── env vars + secrets ─────────────────────────────────────────

export interface EnvVar { key: string; value: string }

export async function listVars(id: string): Promise<{ vars: EnvVar[] }> {
  return proxy(`/api/apps/${encodeURIComponent(id)}/vars`);
}
listVars.config = { id: "apps.listVars" };
export async function setVar(id: string, key: string, value: string): Promise<void> {
  await proxy(`/api/apps/${encodeURIComponent(id)}/vars`, {
    method: "POST",
    body: JSON.stringify({ key, value }),
  });
}
setVar.config = { id: "apps.setVar" };
export async function deleteVar(id: string, key: string): Promise<void> {
  await proxy(`/api/apps/${encodeURIComponent(id)}/vars/${encodeURIComponent(key)}`, {
    method: "DELETE",
  });
}
deleteVar.config = { id: "apps.deleteVar" };

export async function listSecrets(id: string): Promise<{ secrets: string[] }> {
  return proxy(`/api/apps/${encodeURIComponent(id)}/secrets`);
}
listSecrets.config = { id: "apps.listSecrets" };
export async function setSecret(id: string, key: string, value: string): Promise<void> {
  await proxy(`/api/apps/${encodeURIComponent(id)}/secrets`, {
    method: "POST",
    body: JSON.stringify({ key, value }),
  });
}
setSecret.config = { id: "apps.setSecret" };
export async function deleteSecret(id: string, key: string): Promise<void> {
  await proxy(`/api/apps/${encodeURIComponent(id)}/secrets/${encodeURIComponent(key)}`, {
    method: "DELETE",
  });
}
deleteSecret.config = { id: "apps.deleteSecret" };

// `appPreviewUrl` moved to `src/client/lib/preview-url.ts` — it's a
// pure URL-builder that the iframe consumes synchronously, so it must
// not live in a "use server" module (the vite-plugin would otherwise
// turn it into an async RPC stub and the iframe src would receive a
// stringified Promise).
