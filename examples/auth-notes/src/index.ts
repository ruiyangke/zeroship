"use server";

// auth-notes — the env.auth coverage example (G3 / ISS-54).
//
// Proves the gateway→worker identity chain reaches `env.auth` at the worker
// tier. The platform HMAC-signs the authenticated identity into the
// `ZeroShip-User` header; the worker verifies + parses it and exposes it via
// the kernel `env.auth.getUser()` / `requireUser()` primitives, which the
// `@zeroship/auth` server helper wraps.
//
// Procedures (RPC ids are explicit + dotted — each export becomes a procedure
// at /__zeroship/v1/<id>):
//
//   auth.whoami       query   — auth.getUser(); { user: User | null }. The raw
//                               identity probe: null when anonymous, the caller
//                               otherwise. No throw.
//   auth.whoamiStrict query   — RAW auth.requireUser(): no app-level catch, so
//                               the kernel-thrown error surfaces verbatim. This
//                               documents the kernel's actual failure shape for
//                               an anonymous caller (see the harness + report).
//   auth.notes.list   query   — requireSignedIn() (app-level 401 when anon),
//                               then reads THIS user's notes from env.kv scoped
//                               by `user.id`. Proves identity-scoped data: a
//                               different injected identity sees a different
//                               (empty) list — no cross-user leakage.
//   auth.notes.add    mutation— requireSignedIn(), then appends a note to the
//                               caller's own env.kv-scoped list.
//
// Notes are stored in env.kv (already GREEN over the edge — ISS-63/ISS-66) so
// the example needs no Postgres schema; the point under test is that the
// IDENTITY flows to env.auth, not the storage backend.

import { auth, type User } from "@zeroship/auth";
import { kv, type Result } from "@zeroship/kv";
import { mutation, query } from "@zeroship/rpc/server";

const PREFIX = "auth-notes:";
const store = () => kv.namespace(PREFIX);

function must<T>(r: Result<T>): T {
  if (r.error) throw r.error;
  return r.data;
}

/** App-level auth gate: a clean 401-statused error when anonymous.
 *
 * NOTE on the kernel surface: `auth.requireUser()` throws a PLAIN
 * `Error("Authentication required")` with NO `.status`, which the RPC
 * fetch-handler maps to HTTP 500 (its default for status-less throws) — see
 * `auth.whoamiStrict`. App code that wants a real 401 must therefore gate on
 * `getUser()` and throw a status-bearing error itself, as we do here. The
 * status (< 500) also keeps the message un-masked in the wire envelope.
 */
function requireSignedIn(): User {
  const user = auth.getUser();
  if (!user) {
    throw Object.assign(new Error("Authentication required"), {
      status: 401,
      code: "UNAUTHENTICATED",
    });
  }
  return user;
}

export type Note = {
  id: string;
  text: string;
  createdAt: number;
};

export type NotesList = {
  user: string;
  notes: Note[];
};

/** Per-user kv key — scoped by the authenticated user's id, so one caller can
 *  never read or write another caller's notes. This is the cross-user
 *  isolation the harness asserts. */
function notesKey(userId: string): string {
  return `notes:${userId}`;
}

async function readNotes(userId: string): Promise<Note[]> {
  return must(await store().get<Note[]>(notesKey(userId))) ?? [];
}

/** auth.whoami — the raw identity probe. Returns the caller (or null). */
export const whoami = query(
  async (): Promise<{ user: User | null }> => {
    return { user: auth.getUser() };
  },
  { id: "auth.whoami" },
);

/** auth.whoamiStrict — RAW requireUser(): no catch, the kernel throw escapes
 *  verbatim so the harness can observe the kernel's actual failure shape. */
export const whoamiStrict = query(
  async (): Promise<{ user: User }> => {
    return { user: auth.requireUser() };
  },
  { id: "auth.whoamiStrict" },
);

/** auth.notes.list — this user's own notes; app-level 401 when anonymous. */
export const listNotes = query(
  async (): Promise<NotesList> => {
    const user = requireSignedIn();
    return { user: user.id, notes: await readNotes(user.id) };
  },
  { id: "auth.notes.list" },
);

/** auth.notes.add — append a note to the caller's own scoped list. */
export const addNote = mutation(
  async ({ text }: { text: string }): Promise<NotesList> => {
    const user = requireSignedIn();
    const clean = String(text ?? "").trim().slice(0, 280);
    if (!clean) {
      throw Object.assign(new Error("text is required"), {
        status: 400,
        code: "INVALID_ARGUMENT",
      });
    }
    const notes = await readNotes(user.id);
    notes.push({
      id: `${Date.now().toString(36)}-${Math.random().toString(36).slice(2, 8)}`,
      text: clean,
      createdAt: Date.now(),
    });
    must(await store().set<Note[]>(notesKey(user.id), notes));
    return { user: user.id, notes };
  },
  { id: "auth.notes.add" },
);
