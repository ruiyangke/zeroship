"use server";
// Server functions — every export here runs on the zeroship runtime, not in
// the browser. The vite-plugin converts client imports into RPC calls.
//
// Three SDKs demonstrate the platform's shape:
//   - @zeroship/db        typed CRUD over Postgres (PGlite in dev)
//   - @zeroship/storage   file uploads / object storage
//   - @zeroship/kv        in-memory cache / counters
//
// Delete what you don't need; this file is a starting point, not a lecture.

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

export async function listNotes() {
  const r = await db.notes.find().sort({ id: -1 });
  if (r.error) throw r.error;
  return r.data;
}

export async function addNote(title: string, body: string) {
  const r = await db.notes.create({ title, body });
  if (r.error) throw r.error;
  return r.data;
}

export async function deleteNote(id: number) {
  const r = await db.notes.findOneAndDelete({ id });
  if (r.error) throw r.error;
  return r.data !== null;
}

// ── File upload demo (stores base64-encoded bytes from the client) ────────

export async function uploadFile(name: string, dataBase64: string) {
  const bytes = Uint8Array.from(atob(dataBase64), (c) => c.charCodeAt(0));
  const r = await uploads.put(name, bytes);
  if (r.error) throw r.error;
  return r.data;
}

export async function listFiles() {
  const r = await uploads.list();
  if (r.error) throw r.error;
  return r.data;
}

// ── Visit counter (kv) ────────────────────────────────────────────────────

export async function bumpVisits() {
  const r = await kv.incr("visits");
  if (r.error) throw r.error;
  return r.data;
}
