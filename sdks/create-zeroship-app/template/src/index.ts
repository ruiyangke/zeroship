"use server";
// Server functions — wrapped exports here run on the zeroship runtime,
// not in the browser. The file-level `"use server"` directive opts the
// file into RPC discovery; only exports wrapped in `procedure()`,
// `query()`, `mutation()`, or `stream()` from `@zeroship/server`
// become public endpoints. Plain helpers stay server-private.
//
// The vite-plugin converts client imports of these wrapped exports
// into RPC calls.
//
// Three SDKs demonstrate the platform's shape:
//   - @zeroship/db        typed CRUD over Postgres (PGlite in dev)
//   - @zeroship/storage   file uploads / object storage
//   - @zeroship/kv        in-memory cache / counters
//
// Delete what you don't need; this file is a starting point, not a lecture.

import { query, mutation } from "@zeroship/server";
import { createDb, t } from "@zeroship/db";
import { bucket } from "@zeroship/storage";
import { kv } from "@zeroship/kv";

const db = createDb({
  notes: {
    title: t.string().required(),
    body: t.string(),
  },
});

const uploads = bucket("uploads");

// ── Notes CRUD ────────────────────────────────────────────────────────────

export const listNotes = query(async () => {
  const r = await db.notes.find().sort({ id: -1 });
  if (r.error) throw r.error;
  return r.data;
});

export const addNote = mutation(async (title: string, body: string) => {
  const r = await db.notes.create({ title, body });
  if (r.error) throw r.error;
  return r.data;
});

export const deleteNote = mutation(async (id: number) => {
  const r = await db.notes.findOneAndDelete({ id });
  if (r.error) throw r.error;
  return r.data !== null;
});

// ── File upload demo (stores base64-encoded bytes from the client) ────────

export const uploadFile = mutation(
  async (name: string, dataBase64: string) => {
    const bytes = Uint8Array.from(atob(dataBase64), (c) => c.charCodeAt(0));
    const r = await uploads.put(name, bytes);
    if (r.error) throw r.error;
    return r.data;
  },
);

export const listFiles = query(async () => {
  const r = await uploads.list();
  if (r.error) throw r.error;
  return r.data;
});

// ── Visit counter (kv) ────────────────────────────────────────────────────

export const bumpVisits = mutation(async () => {
  const r = await kv.incr("visits");
  if (r.error) throw r.error;
  return r.data;
});
