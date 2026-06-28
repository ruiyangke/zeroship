"use server";
// Server functions — wrapped exports here run on the zeroship runtime,
// not in the browser. The file-level `"use server"` directive opts the
// file into RPC discovery; only exports wrapped in `procedure()`,
// `query()`, `mutation()`, or `stream()` from `@zeroship/rpc/server`
// become public endpoints. Plain helpers stay server-private.
//
// The vite-plugin converts client imports of these wrapped exports
// into RPC calls.
//
// Three SDKs demonstrate the platform's shape:
//   - @zeroship/db        typed CRUD over the app database (SQLite by default in dev)
//   - @zeroship/storage   file uploads / object storage
//   - @zeroship/kv        ephemeral key-value state / counters
//
// Delete what you don't need; this file is a starting point, not a lecture.

import { query, mutation } from "@zeroship/rpc/server";
import { env } from "zeroship";
import { bucket } from "@zeroship/storage";
import { kv } from "@zeroship/kv";

// `env.db` is typed by generated/zeroship/env.db.ts, which is generated
// from the committed migrations.
const db = env.db;

const uploads = bucket("uploads");

// ── Notes CRUD ────────────────────────────────────────────────────────────

export const listNotes = query(
  async () => {
    const r = await db.notes.find().sort({ id: -1 });
    if (r.error) throw r.error;
    return r.data;
  },
  { id: "notes.list" },
);

export const addNote = mutation(
  async ({ title, body }: { title: string; body: string }) => {
    const r = await db.notes.insert({ title, body });
    if (r.error) throw r.error;
    return r.data;
  },
  { id: "notes.add", idempotent: true },
);

export const deleteNote = mutation(
  async ({ id }: { id: string }) => {
    const r = await db.notes.delete(id);
    if (r.error) throw r.error;
    return r.data !== null;
  },
  { id: "notes.delete" },
);

// ── File upload demo (stores base64-encoded bytes from the client) ────────

export const uploadFile = mutation(
  async ({ name, dataBase64 }: { name: string; dataBase64: string }) => {
    const bytes = Uint8Array.from(atob(dataBase64), (c) => c.charCodeAt(0));
    const r = await uploads.put(name, bytes);
    if (r.error) throw r.error;
    return r.data;
  },
  { id: "files.upload", idempotent: true },
);

export const listFiles = query(
  async () => {
    const r = await uploads.list();
    if (r.error) throw r.error;
    return r.data;
  },
  { id: "files.list" },
);

// ── Visit counter (kv) ────────────────────────────────────────────────────

export const bumpVisits = mutation(
  async () => {
    const r = await kv.incr("visits");
    if (r.error) throw r.error;
    return r.data;
  },
  { id: "visits.bump" },
);
