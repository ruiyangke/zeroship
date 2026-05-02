"use server";
// Apps server functions — proxies to control plane's /api/apps/*.
//
// Cookie session covers admin auth (control plane's `check_admin_auth`
// accepts session cookie OR Bearer master-key); we forward both.

import { CONTROL_URL, CONTROL_KEY } from "./env";
import { getRequest } from "./request-context";
import { persistGet, persistSet } from "./_persist.js";

// ─── archive: KV-backed stub (ISS-19) ────────────────────────────
//
// The control plane has no `archived` column / endpoint yet. Until
// then, archive state lives in KV (per-user list of archived appIds).
// V1 dev: KV is the in-memory plugin in the V8 worker — survives HMR
// module reloads, vanishes on hard worker restart. Production lands a
// real `archived_at` column per ISS-19's fix path.
//
// Scoping: per current user. The dev synthetic user is a single id
// (`usr_dev`), so the dashboard always reads the same list during dev.
// In prod the `getRequest()` cookie carries the session and the
// auth-cookie hash gates per-user reads — mirror that here when the
// real auth wire is on.
const ARCHIVE_KEY = "archive-set:usr_dev";

async function loadArchive(): Promise<Set<string>> {
  const list = await persistGet<string[] | null>(ARCHIVE_KEY, null);
  return new Set(list ?? []);
}

async function saveArchive(set: Set<string>): Promise<void> {
  await persistSet(ARCHIVE_KEY, [...set]);
}

export interface AppRecord {
  id: string;
  name: string;
  plan_id: string;
  deploy_hash: string | null;
  api_key: string;
  created_at: string;
  updated_at: string;
  server_js?: string;
  /** Soft-delete flag (ISS-19 stub — module-level Set, dev only). */
  archived?: boolean;
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
  const [apps, archive] = await Promise.all([
    proxy<AppRecord[]>("/api/apps"),
    loadArchive(),
  ]);
  return apps.map((a) => ({ ...a, archived: archive.has(a.id) }));
}
listApps.config = { id: "apps.listApps" };

export async function getApp(id: string): Promise<AppRecord> {
  const [app, archive] = await Promise.all([
    proxy<AppRecord>(`/api/apps/${encodeURIComponent(id)}`),
    loadArchive(),
  ]);
  return { ...app, archived: archive.has(app.id) };
}
getApp.config = { id: "apps.getApp" };

/**
 * Soft-delete an app. Tracked in KV per-user (see ARCHIVE_KEY above)
 * until the control plane gains a real `archived_at` column (ISS-19).
 * Returns the new state so the client can update its cache without a
 * refetch round-trip.
 */
export async function archiveApp(input: { appId: string }): Promise<{ archived: boolean }> {
  const archive = await loadArchive();
  archive.add(input.appId);
  await saveArchive(archive);
  return { archived: true };
}
archiveApp.config = { id: "apps.archiveApp" };

export async function unarchiveApp(input: { appId: string }): Promise<{ archived: boolean }> {
  const archive = await loadArchive();
  archive.delete(input.appId);
  await saveArchive(archive);
  return { archived: false };
}
unarchiveApp.config = { id: "apps.unarchiveApp" };

/**
 * Single-input wire — the vite-plugin RPC stub forwards `args[0]` only,
 * so taking `(name, plan_id)` as positional arguments would silently
 * lose `plan_id` (and the dev-bootstrap actually passes `ctx` as the
 * second arg, which produced an "expected a string" deserialize error
 * upstream when `JSON.stringify({..., plan_id: ctx})` ran). Wrap into
 * one object per spec §RPC.
 */
export async function createApp(input: {
  name: string;
  plan_id?: string;
}): Promise<AppRecord> {
  const name = input?.name;
  const plan_id = input?.plan_id ?? "free";
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
