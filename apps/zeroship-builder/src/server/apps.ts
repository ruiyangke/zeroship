"use server";
// Apps server functions — proxies to control plane's /api/apps/*.
//
// Browser-facing RPCs forward the caller's cookie to the control plane.
// They deliberately do not attach CONTROL_KEY; master-key operations
// belong in server-internal tools, not arbitrary dashboard RPC calls.

import { action, mutation } from "@zeroship/rpc/server";
import {
  createControlClient,
  type AppRecord as ControlAppRecord,
  type EnvVar,
} from "@zeroship/control";
import { currentHeaders, currentUser } from "@zeroship/server";
import { z } from "zod";
import { CONTROL_URL } from "./internal/env";
import { persistGet, persistSet } from "./internal/persist";

// ─── archive: KV-backed stub ─────────────────────────────────────
//
// The control plane has no `archived` column / endpoint yet. Until
// then, archive state lives in KV (per-user list of archived appIds).
// In dev, KV is the in-memory plugin in the V8 worker — it survives HMR
// module reloads and vanishes on hard worker restart. Production should
// replace this with a real `archived_at` column.
//
// Scoping: per current runtime user. In local dev we keep a deterministic
// fallback because the vite dev runtime can run without gateway-injected
// auth. Production must never collapse into a shared archive key.
const DEV_ARCHIVE_KEY = "archive-set:usr_dev";

async function loadArchive(): Promise<Set<string>> {
  const list = await persistGet<string[] | null>(archiveKey(), null);
  return new Set(list ?? []);
}

async function saveArchive(set: Set<string>): Promise<void> {
  await persistSet(archiveKey(), [...set]);
}

export interface AppRecord extends ControlAppRecord {
  server_js?: string;
  /** Soft-delete flag from the temporary archive store. */
  archived?: boolean;
}

export type { EnvVar };

function controlClient() {
  return createControlClient({
    baseUrl: CONTROL_URL(),
    cookie: requestCookie,
  });
}

function requestCookie(): string | null {
  try {
    return currentHeaders().get("cookie") ?? null;
  } catch {
    return null;
  }
}

function archiveKey(): string {
  const user = readCurrentUserId();
  if (user) return `archive-set:${user}`;
  if (isLocalDevRuntime()) return DEV_ARCHIVE_KEY;
  throw new Error("archive requires an authenticated user");
}

function readCurrentUserId(): string | null {
  try {
    const user = currentUser() as { id?: unknown } | null;
    return typeof user?.id === "string" && user.id ? user.id : null;
  } catch {
    return null;
  }
}

function isLocalDevRuntime(): boolean {
  const proc = (globalThis as { process?: { env?: Record<string, string | undefined> } }).process;
  return proc?.env?.NODE_ENV !== "production";
}

const appIdSchema = z.string().min(1).max(256);
const keySchema = z.string().min(1).max(256);
const appIdInputSchema = z.object({ appId: appIdSchema }).strict();
const createAppInputSchema = z.object({
  name: z.string().trim().min(1).max(120),
  plan_id: z.string().min(1).max(64).optional(),
}).strict();
const updatePlanInputSchema = z.object({
  appId: appIdSchema,
  plan_id: z.string().min(1).max(64),
}).strict();
const keyInputSchema = z.object({
  appId: appIdSchema,
  key: keySchema,
}).strict();
const keyValueInputSchema = keyInputSchema.extend({
  value: z.string().max(64 * 1024),
}).strict();

export const listApps = action(async (): Promise<AppRecord[]> => {
  const [apps, archive] = await Promise.all([
    controlClient().apps.list(),
    loadArchive(),
  ]);
  return apps.map((a) => ({ ...a, archived: archive.has(a.id) }));
}, { id: "apps.list", maxInputBytes: 1_024 });

export const getApp = action(async (id: string): Promise<AppRecord> => {
  const [app, archive] = await Promise.all([
    controlClient().apps.get(id),
    loadArchive(),
  ]);
  return { ...app, archived: archive.has(app.id) };
}, { id: "apps.get", input: appIdSchema, maxInputBytes: 4_096 });

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
}, { id: "apps.archive", input: appIdInputSchema, maxInputBytes: 4_096 });

export const unarchiveApp = mutation(async (
  input: { appId: string },
): Promise<{ archived: boolean }> => {
  const archive = await loadArchive();
  archive.delete(input.appId);
  await saveArchive(archive);
  return { archived: false };
}, { id: "apps.unarchive", input: appIdInputSchema, maxInputBytes: 4_096 });

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
  return controlClient().apps.create({ name, plan_id });
}, { id: "apps.create", input: createAppInputSchema, maxInputBytes: 16_384 });

export const deleteApp = action(async (id: string): Promise<{ deleted: boolean }> => {
  return controlClient().apps.delete(id);
}, { id: "apps.delete", input: appIdSchema, maxInputBytes: 4_096 });

export const updatePlan = action(async (
  input: { appId: string; plan_id: string },
): Promise<{ updated: boolean }> => {
  return controlClient().apps.setPlan(input.appId, { plan_id: input.plan_id });
}, { id: "apps.plan.update", input: updatePlanInputSchema, maxInputBytes: 8_192 });

export const getAppLogs = action(async (id: string): Promise<string[]> => {
  return controlClient().apps.logs(id);
}, { id: "apps.logs", input: appIdSchema, maxInputBytes: 4_096 });

// ─── env vars + secrets ─────────────────────────────────────────

export const listVars = action(async (id: string): Promise<{ vars: EnvVar[] }> => {
  return controlClient().env.listVars(id);
}, { id: "apps.vars.list", input: appIdSchema, maxInputBytes: 4_096 });

export const setVar = action(async (
  input: { appId: string; key: string; value: string },
): Promise<void> => {
  await controlClient().env.setVar(input.appId, {
    key: input.key,
    value: input.value,
  });
}, { id: "apps.vars.set", input: keyValueInputSchema, maxInputBytes: 131_072 });

export const deleteVar = action(async (
  input: { appId: string; key: string },
): Promise<void> => {
  await controlClient().env.deleteVar(input.appId, input.key);
}, { id: "apps.vars.delete", input: keyInputSchema, maxInputBytes: 8_192 });

export const listSecrets = action(async (id: string): Promise<{ secrets: string[] }> => {
  return controlClient().env.listSecrets(id);
}, { id: "apps.secrets.list", input: appIdSchema, maxInputBytes: 4_096 });

export const setSecret = action(async (
  input: { appId: string; key: string; value: string },
): Promise<void> => {
  await controlClient().env.setSecret(input.appId, {
    key: input.key,
    value: input.value,
  });
}, { id: "apps.secrets.set", input: keyValueInputSchema, maxInputBytes: 131_072 });

export const deleteSecret = action(async (
  input: { appId: string; key: string },
): Promise<void> => {
  await controlClient().env.deleteSecret(input.appId, input.key);
}, { id: "apps.secrets.delete", input: keyInputSchema, maxInputBytes: 8_192 });

// `appPreviewUrl` moved to `src/client/lib/preview-url.ts` — it's a
// pure URL-builder that the iframe consumes synchronously, so it must
// not live in a "use server" module (the vite-plugin would otherwise
// turn it into an async RPC stub and the iframe src would receive a
// stringified Promise).
