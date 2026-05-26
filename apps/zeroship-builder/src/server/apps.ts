"use server";
// Apps server functions — proxies to control plane's /api/apps/*.
//
// Cookie session covers admin auth (control plane's `check_admin_auth`
// accepts session cookie OR Bearer master-key); we forward both.

import { action, mutation } from "@zeroship/rpc/server";
import { CONTROL_URL, CONTROL_KEY } from "./internal/env";
import { getRequest } from "./internal/request-context";
import { persistGet, persistSet } from "./internal/persist.js";

// ─── archive: KV-backed stub ─────────────────────────────────────
//
// The control plane has no `archived` column / endpoint yet. Until
// then, archive state lives in KV (per-user list of archived appIds).
// In dev, KV is the in-memory plugin in the V8 worker — it survives HMR
// module reloads and vanishes on hard worker restart. Production should
// replace this with a real `archived_at` column.
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
  /** Soft-delete flag from the temporary archive store. */
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

export const listApps = action(async (): Promise<AppRecord[]> => {
  const [apps, archive] = await Promise.all([
    proxy<AppRecord[]>("/api/apps"),
    loadArchive(),
  ]);
  return apps.map((a) => ({ ...a, archived: archive.has(a.id) }));
}, { id: "apps.listApps" });

export const getApp = action(async (id: string): Promise<AppRecord> => {
  const [app, archive] = await Promise.all([
    proxy<AppRecord>(`/api/apps/${encodeURIComponent(id)}`),
    loadArchive(),
  ]);
  return { ...app, archived: archive.has(app.id) };
}, { id: "apps.getApp" });

/**
 * Soft-delete an app. Tracked in KV per-user (see ARCHIVE_KEY above)
 * until the control plane gains a real `archived_at` column.
 * Returns the new state so the client can update its cache without a
 * refetch round-trip.
 */
export const archiveApp = mutation(async (
  input: { appId: string },
): Promise<{ archived: boolean }> => {
  const archive = await loadArchive();
  archive.add(input.appId);
  await saveArchive(archive);
  return { archived: true };
}, { id: "apps.archiveApp" });

export const unarchiveApp = mutation(async (
  input: { appId: string },
): Promise<{ archived: boolean }> => {
  const archive = await loadArchive();
  archive.delete(input.appId);
  await saveArchive(archive);
  return { archived: false };
}, { id: "apps.unarchiveApp" });

/**
 * Single-input wire — the vite-plugin RPC stub forwards `args[0]` only,
 * so taking `(name, plan_id)` as positional arguments would silently
 * lose `plan_id` (and the dev-bootstrap actually passes `ctx` as the
 * second arg, which produced an "expected a string" deserialize error
 * upstream when `JSON.stringify({..., plan_id: ctx})` ran). Wrap into
 * one object per `docs/superpowers/specs/2026-04-30-zeroship-builder-design.md` §RPC.
 */
export const createApp = action(async (input: {
  name: string;
  plan_id?: string;
}): Promise<AppRecord> => {
  const name = input?.name;
  const plan_id = input?.plan_id ?? "free";
  return proxy("/api/apps", {
    method: "POST",
    body: JSON.stringify({ name, plan_id }),
  });
}, { id: "apps.createApp" });

export const deleteApp = action(async (id: string): Promise<{ deleted: boolean }> => {
  return proxy(`/api/apps/${encodeURIComponent(id)}`, { method: "DELETE" });
}, { id: "apps.deleteApp" });

export const deployApp = action(async (
  input: { appId: string; code: string },
): Promise<{ deploy_hash: string }> => {
  return proxy(`/api/apps/${encodeURIComponent(input.appId)}/deploy`, {
    method: "POST",
    body: input.code,
    contentType: "application/javascript",
  });
}, { id: "apps.deployApp" });

export const updatePlan = action(async (
  input: { appId: string; plan_id: string },
): Promise<{ updated: boolean }> => {
  return proxy(`/api/apps/${encodeURIComponent(input.appId)}/plan`, {
    method: "PUT",
    body: JSON.stringify({ plan_id: input.plan_id }),
  });
}, { id: "apps.updatePlan" });

export const getAppLogs = action(async (id: string): Promise<string[]> => {
  return proxy(`/api/apps/${encodeURIComponent(id)}/logs`);
}, { id: "apps.getAppLogs" });

// ─── env vars + secrets ─────────────────────────────────────────

export interface EnvVar { key: string; value: string }

export const listVars = action(async (id: string): Promise<{ vars: EnvVar[] }> => {
  return proxy(`/api/apps/${encodeURIComponent(id)}/vars`);
}, { id: "apps.listVars" });

export const setVar = action(async (
  input: { appId: string; key: string; value: string },
): Promise<void> => {
  await proxy(`/api/apps/${encodeURIComponent(input.appId)}/vars`, {
    method: "POST",
    body: JSON.stringify({ key: input.key, value: input.value }),
  });
}, { id: "apps.setVar" });

export const deleteVar = action(async (
  input: { appId: string; key: string },
): Promise<void> => {
  await proxy(`/api/apps/${encodeURIComponent(input.appId)}/vars/${encodeURIComponent(input.key)}`, {
    method: "DELETE",
  });
}, { id: "apps.deleteVar" });

export const listSecrets = action(async (id: string): Promise<{ secrets: string[] }> => {
  return proxy(`/api/apps/${encodeURIComponent(id)}/secrets`);
}, { id: "apps.listSecrets" });

export const setSecret = action(async (
  input: { appId: string; key: string; value: string },
): Promise<void> => {
  await proxy(`/api/apps/${encodeURIComponent(input.appId)}/secrets`, {
    method: "POST",
    body: JSON.stringify({ key: input.key, value: input.value }),
  });
}, { id: "apps.setSecret" });

export const deleteSecret = action(async (
  input: { appId: string; key: string },
): Promise<void> => {
  await proxy(`/api/apps/${encodeURIComponent(input.appId)}/secrets/${encodeURIComponent(input.key)}`, {
    method: "DELETE",
  });
}, { id: "apps.deleteSecret" });

// `appPreviewUrl` moved to `src/client/lib/preview-url.ts` — it's a
// pure URL-builder that the iframe consumes synchronously, so it must
// not live in a "use server" module (the vite-plugin would otherwise
// turn it into an async RPC stub and the iframe src would receive a
// stringified Promise).
